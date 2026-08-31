use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use agent_protocol::{
    CapabilityRequest, LocalAuthority, Ticket, WorkerId, WorkerReport, WorkerStatus,
};
use async_trait::async_trait;
use capability_broker::{
    BrokerError, CapabilityBroker, CapabilityTranslator, TicketAuthorityStore,
};
use model_gateway::{ModelGateway, ModelMessage, ModelRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{fs, process::Command, time};

pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_MAX_STEPS: usize = 128;
/// Consecutive unparseable model replies tolerated before the task is failed.
pub const DEFAULT_MAX_MALFORMED_ACTIONS: usize = 3;

/// Caps on what a single local operation may return.
///
/// Context economy is one of Seam's central hypotheses, so an unbounded `read_file` or a
/// chatty command is not a cosmetic problem: it silently invalidates the thing the
/// architecture is meant to demonstrate.
#[derive(Debug, Clone, Copy)]
pub struct WorkbenchLimits {
    pub max_file_bytes: usize,
    pub max_stdout_bytes: usize,
    pub max_stderr_bytes: usize,
}

impl Default for WorkbenchLimits {
    fn default() -> Self {
        Self {
            max_file_bytes: 64 * 1024,
            max_stdout_bytes: 32 * 1024,
            max_stderr_bytes: 8 * 1024,
        }
    }
}

/// Caps on the observation history fed back into each model turn.
#[derive(Debug, Clone, Copy)]
pub struct ObservationLimits {
    /// Cap on one recorded observation.
    pub max_observation_bytes: usize,
    /// Cap on the whole window sent to the model in a turn.
    pub max_window_bytes: usize,
    /// Cap on how many observations the window may contain.
    pub max_window_observations: usize,
}

impl Default for ObservationLimits {
    fn default() -> Self {
        Self {
            max_observation_bytes: 8 * 1024,
            max_window_bytes: 32 * 1024,
            max_window_observations: 16,
        }
    }
}

/// Truncate on a char boundary, marking what was dropped so the model can tell the
/// difference between "this is all of it" and "there was more".
fn truncate_text(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("model error: {0}")]
    Model(String),
    #[error("invalid worker action: {0}")]
    InvalidAction(String),
    #[error("local workbench error: {0}")]
    Local(String),
    #[error("local authority denied: {0}")]
    Denied(String),
    #[error("capability broker error: {0}")]
    Broker(#[from] BrokerError),
    #[error("worker exceeded step limit")]
    StepLimit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerAction {
    Local {
        operation: String,
        #[serde(default)]
        arguments: Value,
    },
    Capability {
        request: String,
    },
    Finish {
        summary: String,
        #[serde(default)]
        artifacts: Vec<agent_protocol::ArtifactRef>,
    },
    Blocked {
        reason: String,
    },
}

#[async_trait]
pub trait LocalWorkbench: Send + Sync {
    /// `authority` is the ticket's local authority. Implementations must treat it as the
    /// upper bound on what the operation may do; it is not advisory.
    async fn execute(
        &self,
        operation: &str,
        arguments: Value,
        authority: &LocalAuthority,
    ) -> Result<Value, WorkerError>;
}

/// Workspace-constrained primitives. This is NOT a substitute for an OS/container sandbox.
///
/// Two independent limits apply to every call: the deployment ceiling configured here and
/// the per-ticket [`LocalAuthority`]. An operation must be permitted by both.
pub struct FsProcessWorkbench {
    root: PathBuf,
    allow_write: bool,
    allow_execute: bool,
    limits: WorkbenchLimits,
    exec_timeout: Duration,
    /// Environment variables passed through to child processes. Everything else is
    /// cleared so a worker cannot read ambient host credentials out of its own env.
    exec_env_allowlist: Vec<String>,
}

impl FsProcessWorkbench {
    pub fn new(root: impl Into<PathBuf>, allow_write: bool, allow_execute: bool) -> Self {
        Self {
            root: root.into(),
            allow_write,
            allow_execute,
            limits: WorkbenchLimits::default(),
            exec_timeout: DEFAULT_EXEC_TIMEOUT,
            exec_env_allowlist: vec!["PATH".to_string()],
        }
    }

    pub fn with_limits(mut self, limits: WorkbenchLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_exec_timeout(mut self, timeout: Duration) -> Self {
        self.exec_timeout = timeout;
        self
    }

    pub fn with_exec_env_allowlist(mut self, names: Vec<String>) -> Self {
        self.exec_env_allowlist = names;
        self
    }

    /// Resolve a workspace-relative path, refusing anything that leaves the workspace.
    ///
    /// Symlinks are resolved on the *final* component too. Checking only the parent
    /// directory would let a symlink inside the workspace redirect a write to an
    /// arbitrary host path.
    fn resolve(&self, relative: &str, must_exist: bool) -> Result<PathBuf, WorkerError> {
        let candidate = Path::new(relative);
        for component in candidate.components() {
            match component {
                Component::Normal(_) | Component::CurDir => {}
                _ => {
                    return Err(WorkerError::Local(format!(
                        "path must be workspace-relative: {relative}"
                    )));
                }
            }
        }

        let root = self
            .root
            .canonicalize()
            .map_err(|e| WorkerError::Local(e.to_string()))?;
        let joined = root.join(candidate);

        let resolved = match joined.canonicalize() {
            Ok(path) => path,
            Err(error) if !must_exist => {
                // The entry does not resolve. If something is nonetheless present at the
                // path it is a dangling symlink, and writing through it would land
                // wherever it points.
                if joined.symlink_metadata().is_ok() {
                    return Err(WorkerError::Local("path escapes workspace".into()));
                }
                let parent = joined
                    .parent()
                    .ok_or_else(|| WorkerError::Local("invalid path".into()))?;
                let name = joined
                    .file_name()
                    .ok_or_else(|| WorkerError::Local("invalid path".into()))?;
                parent
                    .canonicalize()
                    .map_err(|_| WorkerError::Local(error.to_string()))?
                    .join(name)
            }
            Err(error) => return Err(WorkerError::Local(error.to_string())),
        };

        if !resolved.starts_with(&root) {
            return Err(WorkerError::Local("path escapes workspace".into()));
        }
        Ok(resolved)
    }

    fn child_env(&self) -> Vec<(String, OsString)> {
        self.exec_env_allowlist
            .iter()
            .filter_map(|name| std::env::var_os(name).map(|value| (name.clone(), value)))
            .collect()
    }
}

#[async_trait]
impl LocalWorkbench for FsProcessWorkbench {
    async fn execute(
        &self,
        operation: &str,
        arguments: Value,
        authority: &LocalAuthority,
    ) -> Result<Value, WorkerError> {
        match operation {
            "read_file" => {
                if !authority.read_workspace {
                    return Err(WorkerError::Denied("workspace reads denied".into()));
                }
                let path = arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WorkerError::Local("read_file requires path".into()))?;
                let path = self.resolve(path, true)?;
                let content = fs::read_to_string(path)
                    .await
                    .map_err(|e| WorkerError::Local(e.to_string()))?;

                // Windowed read: the model can page through a large file instead of
                // pulling all of it into context to look at one function.
                let total_bytes = content.len();
                let offset = arguments
                    .get("offset")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(total_bytes as u64) as usize;
                let offset = (offset..=total_bytes)
                    .find(|candidate| content.is_char_boundary(*candidate))
                    .unwrap_or(total_bytes);
                let limit = arguments
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|limit| limit as usize)
                    .unwrap_or(self.limits.max_file_bytes)
                    .min(self.limits.max_file_bytes);

                let (window, truncated) = truncate_text(&content[offset..], limit);
                let next_offset = offset + window.len();
                Ok(json!({
                    "content": window,
                    "offset": offset,
                    "bytes_returned": window.len(),
                    "total_bytes": total_bytes,
                    "truncated": truncated,
                    "next_offset": if truncated { Some(next_offset) } else { None },
                }))
            }
            "write_file" => {
                if !self.allow_write {
                    return Err(WorkerError::Denied("workspace writes denied".into()));
                }
                if !authority.write_workspace {
                    return Err(WorkerError::Denied(
                        "ticket does not grant workspace writes".into(),
                    ));
                }
                let path = arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WorkerError::Local("write_file requires path".into()))?;
                let content = arguments
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WorkerError::Local("write_file requires content".into()))?;
                let path = self.resolve(path, false)?;
                fs::write(path, content)
                    .await
                    .map_err(|e| WorkerError::Local(e.to_string()))?;
                Ok(json!({ "ok": true }))
            }
            "list_dir" => {
                if !authority.read_workspace {
                    return Err(WorkerError::Denied("workspace reads denied".into()));
                }
                let path = arguments.get("path").and_then(Value::as_str).unwrap_or(".");
                let path = self.resolve(path, true)?;
                let mut dir = fs::read_dir(path)
                    .await
                    .map_err(|e| WorkerError::Local(e.to_string()))?;
                let mut entries = Vec::new();
                while let Some(entry) = dir
                    .next_entry()
                    .await
                    .map_err(|e| WorkerError::Local(e.to_string()))?
                {
                    entries.push(entry.file_name().to_string_lossy().to_string());
                }
                Ok(json!({ "entries": entries }))
            }
            "exec" => {
                if !self.allow_execute {
                    return Err(WorkerError::Denied("local execution denied".into()));
                }
                if !authority.execute_local {
                    return Err(WorkerError::Denied(
                        "ticket does not grant local execution".into(),
                    ));
                }
                let program = arguments
                    .get("program")
                    .and_then(Value::as_str)
                    .ok_or_else(|| WorkerError::Local("exec requires program".into()))?;
                let args: Vec<&str> = arguments
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|items| items.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let cwd = arguments.get("cwd").and_then(Value::as_str).unwrap_or(".");
                let cwd = self.resolve(cwd, true)?;

                let child = Command::new(program)
                    .args(args)
                    .current_dir(cwd)
                    .env_clear()
                    .envs(self.child_env())
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    // Ensures the timeout below actually terminates the process rather
                    // than just abandoning it.
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| WorkerError::Local(e.to_string()))?;

                let output = time::timeout(self.exec_timeout, child.wait_with_output())
                    .await
                    .map_err(|_| {
                        WorkerError::Local(format!(
                            "command exceeded {}s timeout",
                            self.exec_timeout.as_secs()
                        ))
                    })?
                    .map_err(|e| WorkerError::Local(e.to_string()))?;

                let (stdout, stdout_truncated) = truncate_text(
                    &String::from_utf8_lossy(&output.stdout),
                    self.limits.max_stdout_bytes,
                );
                let (stderr, stderr_truncated) = truncate_text(
                    &String::from_utf8_lossy(&output.stderr),
                    self.limits.max_stderr_bytes,
                );

                Ok(json!({
                    "status": output.status.code(),
                    "stdout": stdout,
                    "stdout_truncated": stdout_truncated,
                    "stderr": stderr,
                    "stderr_truncated": stderr_truncated,
                }))
            }
            _ => Err(WorkerError::Local(format!(
                "unknown local operation: {operation}"
            ))),
        }
    }
}

pub struct WorkerRuntime<M, T, A, W> {
    model: Arc<M>,
    broker: Arc<CapabilityBroker<T, A>>,
    workbench: Arc<W>,
    max_steps: usize,
    max_malformed_actions: usize,
    observation_limits: ObservationLimits,
}

impl<M, T, A, W> WorkerRuntime<M, T, A, W>
where
    M: ModelGateway + 'static,
    T: CapabilityTranslator + 'static,
    A: TicketAuthorityStore + 'static,
    W: LocalWorkbench + 'static,
{
    pub fn new(model: Arc<M>, broker: Arc<CapabilityBroker<T, A>>, workbench: Arc<W>) -> Self {
        Self {
            model,
            broker,
            workbench,
            max_steps: DEFAULT_MAX_STEPS,
            max_malformed_actions: DEFAULT_MAX_MALFORMED_ACTIONS,
            observation_limits: ObservationLimits::default(),
        }
    }

    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }

    pub fn with_observation_limits(mut self, limits: ObservationLimits) -> Self {
        self.observation_limits = limits;
        self
    }

    pub fn with_max_malformed_actions(mut self, max_malformed_actions: usize) -> Self {
        self.max_malformed_actions = max_malformed_actions;
        self
    }

    /// Run a ticket to a bounded result.
    ///
    /// `worker_id` is assigned by whoever dispatches the work, not minted here: the
    /// broker binds authority to that identity, and an identity a worker chose for itself
    /// would prove nothing.
    ///
    /// A task that fails returns `Ok(WorkerReport { status: Failed, .. })`. `Err` is
    /// reserved for the runtime itself being unable to continue — a task failure and a
    /// crashed harness are different events and the orchestrator needs to tell them apart.
    pub async fn run(
        &self,
        worker_id: WorkerId,
        ticket: Ticket,
    ) -> Result<WorkerReport, WorkerError> {
        let mut observations: Vec<String> = Vec::new();
        let mut consecutive_malformed = 0usize;

        for _ in 0..self.max_steps {
            let action = match self.next_action(&ticket, &observations).await {
                Ok(action) => {
                    consecutive_malformed = 0;
                    action
                }
                // A model that emitted unparseable output gets told what was wrong and
                // another turn, rather than taking the whole worker down with it.
                Err(WorkerError::InvalidAction(detail)) => {
                    consecutive_malformed += 1;
                    if consecutive_malformed >= self.max_malformed_actions {
                        return Ok(self.failed(
                            worker_id,
                            &ticket,
                            format!(
                                "Model produced unparseable actions {consecutive_malformed} times in a row; last error: {detail}"
                            ),
                        ));
                    }
                    self.push_observation(
                        &mut observations,
                        json!({
                            "error": "invalid_action",
                            "detail": detail,
                            "expected": "exactly one JSON action object of type local|capability|finish|blocked",
                        }),
                    );
                    continue;
                }
                Err(error) => return Err(error),
            };

            match action {
                WorkerAction::Local {
                    operation,
                    arguments,
                } => {
                    // Denials and ordinary operational errors (missing file, bad
                    // arguments) are observations, not worker crashes: the model should
                    // see the boundary and pick another route or report blocked.
                    let observation = match self
                        .workbench
                        .execute(&operation, arguments, &ticket.authority.local)
                        .await
                    {
                        Ok(result) => json!({"local_operation": operation, "result": result}),
                        Err(error @ WorkerError::Denied(_)) => {
                            json!({"local_operation": operation, "denied": error.to_string()})
                        }
                        Err(error @ WorkerError::Local(_)) => {
                            json!({"local_operation": operation, "error": error.to_string()})
                        }
                        Err(error) => return Err(error),
                    };
                    self.push_observation(&mut observations, observation);
                }
                WorkerAction::Capability { request } => {
                    // Only identity and intent cross this boundary. The broker resolves
                    // what this ticket may do from its own trusted source.
                    let result = self
                        .broker
                        .execute(&CapabilityRequest {
                            worker_id,
                            ticket_id: ticket.id,
                            request,
                        })
                        .await;
                    let observation = match result {
                        Ok(result) => json!({ "capability_result": result }),
                        // Provider topology and transport detail stay below the boundary;
                        // the worker sees a stable typed code it can act on.
                        Err(error) => json!({"capability_error": error.code()}),
                    };
                    self.push_observation(&mut observations, observation);
                }
                WorkerAction::Finish { summary, artifacts } => {
                    return Ok(WorkerReport {
                        worker_id,
                        ticket_id: ticket.id,
                        conversation_id: ticket.conversation_id,
                        status: WorkerStatus::Completed,
                        summary,
                        artifacts,
                        notes: Vec::new(),
                    });
                }
                WorkerAction::Blocked { reason } => {
                    return Ok(WorkerReport {
                        worker_id,
                        ticket_id: ticket.id,
                        conversation_id: ticket.conversation_id,
                        status: WorkerStatus::Blocked,
                        summary: reason,
                        artifacts: Vec::new(),
                        notes: Vec::new(),
                    });
                }
            }
        }

        // Exhausting the step budget is the task failing to converge, not the harness
        // breaking.
        Ok(self.failed(
            worker_id,
            &ticket,
            format!(
                "Worker did not reach a conclusion within its {} step budget.",
                self.max_steps
            ),
        ))
    }

    fn failed(&self, worker_id: WorkerId, ticket: &Ticket, summary: String) -> WorkerReport {
        WorkerReport {
            worker_id,
            ticket_id: ticket.id,
            conversation_id: ticket.conversation_id,
            status: WorkerStatus::Failed,
            summary,
            artifacts: Vec::new(),
            notes: Vec::new(),
        }
    }

    /// Record an observation, capped so one huge result cannot dominate the context.
    fn push_observation(&self, observations: &mut Vec<String>, observation: Value) {
        let (text, truncated) = truncate_text(
            &observation.to_string(),
            self.observation_limits.max_observation_bytes,
        );
        observations.push(if truncated {
            format!("{text}… [observation truncated]")
        } else {
            text
        });
    }

    /// The most recent observations that fit the window budget, oldest first.
    fn observation_window<'a>(&self, observations: &'a [String]) -> Vec<&'a str> {
        let mut window = Vec::new();
        let mut used = 0usize;
        for observation in observations
            .iter()
            .rev()
            .take(self.observation_limits.max_window_observations)
        {
            if used + observation.len() > self.observation_limits.max_window_bytes {
                break;
            }
            used += observation.len();
            window.push(observation.as_str());
        }
        window.reverse();
        window
    }

    async fn next_action(
        &self,
        ticket: &Ticket,
        observations: &[String],
    ) -> Result<WorkerAction, WorkerError> {
        let system = r#"
You are a task-scoped worker. You know only the supplied ticket and observations.
You have no durable memory and should not infer a larger project context.

You may:
1. Use local workspace primitives directly.
2. Request an EXTERNAL capability through the Capability Broker.
3. Finish or report that you are blocked.

Local operations available:
- read_file {path, offset?, limit?}   returns a bounded window; follow next_offset for more
- write_file {path, content}
- list_dir {path}
- exec {program, args[], cwd}

Paths must be workspace-relative. Absolute paths and parent traversal are refused.
Your ticket's authority decides which local operations are permitted; a denial is final.

Anything outside the sandbox (web, remote APIs, MCP, messaging, cloud, remote git, secrets, etc.) MUST be requested as a capability in plain language.

Return exactly one JSON action:
{"type":"local","operation":"...","arguments":{...}}
{"type":"capability","request":"..."}
{"type":"finish","summary":"...","artifacts":[]}
{"type":"blocked","reason":"..."}
"#;

        let input = json!({
            "ticket": ticket,
            "recent_observations": self.observation_window(observations),
        });

        let raw = self
            .model
            .complete(ModelRequest::json(
                vec![
                    ModelMessage::system(system),
                    ModelMessage::user(input.to_string()),
                ],
                0.1,
            ))
            .await
            .map_err(|e| WorkerError::Model(e.to_string()))?;

        serde_json::from_str(&raw).map_err(|e| WorkerError::InvalidAction(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn full_authority() -> LocalAuthority {
        LocalAuthority {
            read_workspace: true,
            write_workspace: true,
            execute_local: true,
        }
    }

    fn workspace() -> (TempDir, FsProcessWorkbench) {
        let dir = TempDir::new().expect("temp dir");
        let workbench = FsProcessWorkbench::new(dir.path(), true, true);
        (dir, workbench)
    }

    #[tokio::test]
    async fn reads_and_writes_inside_the_workspace() {
        let (_dir, workbench) = workspace();
        workbench
            .execute(
                "write_file",
                json!({"path": "notes.txt", "content": "hello"}),
                &full_authority(),
            )
            .await
            .expect("write");

        let read = workbench
            .execute("read_file", json!({"path": "notes.txt"}), &full_authority())
            .await
            .expect("read");
        assert_eq!(read["content"], "hello");
    }

    #[tokio::test]
    async fn write_through_a_symlink_cannot_escape_the_workspace() {
        let (dir, workbench) = workspace();
        let outside = dir.path().parent().expect("parent").join("outside.txt");
        std::fs::write(&outside, "original").expect("seed");
        std::os::unix::fs::symlink(&outside, dir.path().join("link.txt")).expect("symlink");

        let error = workbench
            .execute(
                "write_file",
                json!({"path": "link.txt", "content": "overwritten"}),
                &full_authority(),
            )
            .await
            .expect_err("symlink write must be refused");

        assert!(error.to_string().contains("escapes workspace"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&outside).expect("read outside"),
            "original",
            "the file outside the workspace must be untouched"
        );
        let _ = std::fs::remove_file(&outside);
    }

    #[tokio::test]
    async fn write_through_a_dangling_symlink_cannot_escape_the_workspace() {
        let (dir, workbench) = workspace();
        let outside = dir.path().parent().expect("parent").join("not-created.txt");
        let _ = std::fs::remove_file(&outside);
        std::os::unix::fs::symlink(&outside, dir.path().join("link.txt")).expect("symlink");

        let error = workbench
            .execute(
                "write_file",
                json!({"path": "link.txt", "content": "created"}),
                &full_authority(),
            )
            .await
            .expect_err("dangling symlink write must be refused");

        assert!(error.to_string().contains("escapes workspace"), "{error}");
        assert!(!outside.exists(), "no file may be created outside");
    }

    #[tokio::test]
    async fn reading_a_symlink_out_of_the_workspace_is_refused() {
        let (dir, workbench) = workspace();
        let outside = dir.path().parent().expect("parent").join("secret.txt");
        std::fs::write(&outside, "secret").expect("seed");
        std::os::unix::fs::symlink(&outside, dir.path().join("link.txt")).expect("symlink");

        let error = workbench
            .execute("read_file", json!({"path": "link.txt"}), &full_authority())
            .await
            .expect_err("symlink read must be refused");
        assert!(error.to_string().contains("escapes workspace"), "{error}");
        let _ = std::fs::remove_file(&outside);
    }

    #[tokio::test]
    async fn absolute_and_traversal_paths_are_refused() {
        let (_dir, workbench) = workspace();
        for path in ["/etc/passwd", "../escape.txt", "sub/../../escape.txt"] {
            let error = workbench
                .execute(
                    "write_file",
                    json!({"path": path, "content": "x"}),
                    &full_authority(),
                )
                .await
                .expect_err("must be refused");
            assert!(
                error.to_string().contains("workspace-relative"),
                "{path}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn ticket_authority_gates_each_operation() {
        let (_dir, workbench) = workspace();
        let read_only = LocalAuthority {
            read_workspace: true,
            write_workspace: false,
            execute_local: false,
        };

        let error = workbench
            .execute(
                "write_file",
                json!({"path": "a.txt", "content": "x"}),
                &read_only,
            )
            .await
            .expect_err("write must be denied by ticket authority");
        assert!(matches!(error, WorkerError::Denied(_)), "{error}");

        let error = workbench
            .execute("exec", json!({"program": "true"}), &read_only)
            .await
            .expect_err("exec must be denied by ticket authority");
        assert!(matches!(error, WorkerError::Denied(_)), "{error}");

        let no_read = LocalAuthority {
            read_workspace: false,
            ..read_only
        };
        let error = workbench
            .execute("list_dir", json!({}), &no_read)
            .await
            .expect_err("read must be denied by ticket authority");
        assert!(matches!(error, WorkerError::Denied(_)), "{error}");
    }

    #[tokio::test]
    async fn a_permissive_ticket_cannot_exceed_the_deployment_ceiling() {
        let dir = TempDir::new().expect("temp dir");
        let workbench = FsProcessWorkbench::new(dir.path(), false, false);

        let error = workbench
            .execute(
                "write_file",
                json!({"path": "a.txt", "content": "x"}),
                &full_authority(),
            )
            .await
            .expect_err("the workbench ceiling still applies");
        assert!(matches!(error, WorkerError::Denied(_)), "{error}");
    }

    #[tokio::test]
    async fn exec_does_not_inherit_ambient_host_environment() {
        let (_dir, workbench) = workspace();
        // SAFETY: single-threaded within this test's setup, before any child is spawned.
        unsafe { std::env::set_var("SEAM_TEST_AMBIENT_SECRET", "leaked") };

        let result = workbench
            .execute(
                "exec",
                json!({"program": "sh", "args": ["-c", "printenv SEAM_TEST_AMBIENT_SECRET || true"]}),
                &full_authority(),
            )
            .await
            .expect("exec");

        let stdout = result["stdout"].as_str().unwrap_or_default();
        assert!(
            !stdout.contains("leaked"),
            "child saw ambient env: {stdout}"
        );
        unsafe { std::env::remove_var("SEAM_TEST_AMBIENT_SECRET") };
    }

    #[tokio::test]
    async fn read_file_returns_a_bounded_window_and_can_be_paged() {
        let (dir, _) = workspace();
        let workbench =
            FsProcessWorkbench::new(dir.path(), true, true).with_limits(WorkbenchLimits {
                max_file_bytes: 10,
                ..WorkbenchLimits::default()
            });
        std::fs::write(dir.path().join("big.txt"), "abcdefghijklmnopqrstuvwxyz").expect("seed");

        let first = workbench
            .execute("read_file", json!({"path": "big.txt"}), &full_authority())
            .await
            .expect("read");
        assert_eq!(first["content"], "abcdefghij");
        assert_eq!(first["truncated"], true);
        assert_eq!(first["total_bytes"], 26);

        let next = first["next_offset"].as_u64().expect("next_offset");
        let second = workbench
            .execute(
                "read_file",
                json!({"path": "big.txt", "offset": next}),
                &full_authority(),
            )
            .await
            .expect("read");
        assert_eq!(second["content"], "klmnopqrst");
    }

    #[tokio::test]
    async fn an_explicit_limit_cannot_exceed_the_configured_cap() {
        let (dir, _) = workspace();
        let workbench =
            FsProcessWorkbench::new(dir.path(), true, true).with_limits(WorkbenchLimits {
                max_file_bytes: 4,
                ..WorkbenchLimits::default()
            });
        std::fs::write(dir.path().join("big.txt"), "abcdefghij").expect("seed");

        let result = workbench
            .execute(
                "read_file",
                json!({"path": "big.txt", "limit": 9999}),
                &full_authority(),
            )
            .await
            .expect("read");
        assert_eq!(result["bytes_returned"], 4);
    }

    #[tokio::test]
    async fn command_output_is_capped() {
        let (dir, _) = workspace();
        let workbench =
            FsProcessWorkbench::new(dir.path(), true, true).with_limits(WorkbenchLimits {
                max_stdout_bytes: 16,
                ..WorkbenchLimits::default()
            });

        let result = workbench
            .execute(
                "exec",
                json!({"program": "sh", "args": ["-c", "printf 'x%.0s' $(seq 1 5000)"]}),
                &full_authority(),
            )
            .await
            .expect("exec");
        assert_eq!(result["stdout"].as_str().expect("stdout").len(), 16);
        assert_eq!(result["stdout_truncated"], true);
    }

    // ---- runtime loop ----

    use agent_protocol::{AuthorityEnvelope, ConversationId};
    use capability_broker::{CapabilityDescriptor, InMemoryTicketAuthorityStore, SlmTranslator};
    use std::sync::Mutex;
    use uuid::Uuid;

    /// Replays a fixed script of model replies, then repeats the last one.
    struct ScriptedModel {
        replies: Mutex<Vec<String>>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl ScriptedModel {
        fn new(replies: &[&str]) -> Self {
            Self {
                replies: Mutex::new(replies.iter().rev().map(|r| r.to_string()).collect()),
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    #[async_trait]
    impl ModelGateway for ScriptedModel {
        async fn complete(
            &self,
            _request: ModelRequest,
        ) -> Result<String, model_gateway::ModelError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut replies = self.replies.lock().expect("lock");
            Ok(if replies.len() > 1 {
                replies.pop().expect("reply")
            } else {
                replies.last().cloned().unwrap_or_else(|| "{}".to_string())
            })
        }
    }

    type TestRuntime = WorkerRuntime<
        ScriptedModel,
        SlmTranslator<ScriptedModel>,
        InMemoryTicketAuthorityStore,
        FsProcessWorkbench,
    >;

    fn ticket_for(dir: &TempDir) -> Ticket {
        let _ = dir;
        Ticket {
            id: Uuid::new_v4(),
            conversation_id: ConversationId::new_v4(),
            objective: "inspect the workspace".into(),
            context: Vec::new(),
            constraints: Vec::new(),
            deliverable: "a description".into(),
            authority: AuthorityEnvelope {
                local: full_authority(),
                external_capabilities: Default::default(),
            },
        }
    }

    fn runtime_with(replies: &[&str], dir: &TempDir) -> TestRuntime {
        let store = Arc::new(InMemoryTicketAuthorityStore::default());
        let mut broker = CapabilityBroker::new(
            Arc::new(SlmTranslator::new(Arc::new(ScriptedModel::new(&["{}"])))),
            store,
        );
        broker.register_capability(CapabilityDescriptor {
            name: "web.search".into(),
            description: "search".into(),
            argument_schema: json!({"type": "object"}),
        });
        WorkerRuntime::new(
            Arc::new(ScriptedModel::new(replies)),
            Arc::new(broker),
            Arc::new(FsProcessWorkbench::new(dir.path(), true, true)),
        )
    }

    #[tokio::test]
    async fn a_finishing_worker_reports_completed() {
        let dir = TempDir::new().expect("temp dir");
        let runtime = runtime_with(
            &[r#"{"type":"finish","summary":"a Rust workspace","artifacts":[]}"#],
            &dir,
        );

        let report = runtime
            .run(Uuid::new_v4(), ticket_for(&dir))
            .await
            .expect("run");
        assert!(matches!(report.status, WorkerStatus::Completed));
        assert_eq!(report.summary, "a Rust workspace");
    }

    #[tokio::test]
    async fn a_malformed_action_is_corrected_rather_than_fatal() {
        let dir = TempDir::new().expect("temp dir");
        // One unparseable reply, then a valid finish.
        let runtime = runtime_with(
            &[
                "I think I should probably look at the files first!",
                r#"{"type":"finish","summary":"recovered","artifacts":[]}"#,
            ],
            &dir,
        );

        let report = runtime
            .run(Uuid::new_v4(), ticket_for(&dir))
            .await
            .expect("a malformed action must not crash the runtime");
        assert!(matches!(report.status, WorkerStatus::Completed));
        assert_eq!(report.summary, "recovered");
    }

    #[tokio::test]
    async fn persistent_malformed_output_fails_the_task_not_the_runtime() {
        let dir = TempDir::new().expect("temp dir");
        let runtime = runtime_with(&["not json at all"], &dir).with_max_malformed_actions(3);

        let report = runtime
            .run(Uuid::new_v4(), ticket_for(&dir))
            .await
            .expect("task failure must be a report, not Err");
        assert!(matches!(report.status, WorkerStatus::Failed), "{report:?}");
        assert!(report.summary.contains("unparseable"), "{}", report.summary);
    }

    #[tokio::test]
    async fn exhausting_the_step_budget_fails_the_task_not_the_runtime() {
        let dir = TempDir::new().expect("temp dir");
        // Always asks to list the directory, never finishes.
        let runtime = runtime_with(
            &[r#"{"type":"local","operation":"list_dir","arguments":{}}"#],
            &dir,
        )
        .with_max_steps(3);

        let report = runtime
            .run(Uuid::new_v4(), ticket_for(&dir))
            .await
            .expect("step exhaustion must be a report, not Err");
        assert!(matches!(report.status, WorkerStatus::Failed), "{report:?}");
        assert!(report.summary.contains("step budget"), "{}", report.summary);
    }

    #[tokio::test]
    async fn a_failed_local_operation_is_an_observation_not_a_crash() {
        let dir = TempDir::new().expect("temp dir");
        let runtime = runtime_with(
            &[
                r#"{"type":"local","operation":"read_file","arguments":{"path":"missing.txt"}}"#,
                r#"{"type":"blocked","reason":"the file I needed is not there"}"#,
            ],
            &dir,
        );

        let report = runtime
            .run(Uuid::new_v4(), ticket_for(&dir))
            .await
            .expect("a missing file must not crash the runtime");
        assert!(matches!(report.status, WorkerStatus::Blocked), "{report:?}");
    }

    #[test]
    fn the_observation_window_respects_its_byte_budget() {
        let dir = TempDir::new().expect("temp dir");
        let runtime = runtime_with(&["{}"], &dir).with_observation_limits(ObservationLimits {
            max_observation_bytes: 1024,
            max_window_bytes: 25,
            max_window_observations: 16,
        });

        let observations: Vec<String> = (0..10).map(|i| format!("observation-{i:02}")).collect();
        let window = runtime.observation_window(&observations);

        assert_eq!(window.len(), 1, "only what fits in the budget");
        assert_eq!(
            window[0], "observation-09",
            "the window keeps the most recent, in order"
        );
    }

    #[test]
    fn a_single_huge_observation_is_truncated_before_it_is_stored() {
        let dir = TempDir::new().expect("temp dir");
        let runtime = runtime_with(&["{}"], &dir).with_observation_limits(ObservationLimits {
            max_observation_bytes: 64,
            ..ObservationLimits::default()
        });

        let mut observations = Vec::new();
        runtime.push_observation(&mut observations, json!({ "stdout": "x".repeat(10_000) }));

        assert_eq!(observations.len(), 1);
        assert!(observations[0].len() < 200, "observation was not capped");
        assert!(observations[0].contains("truncated"));
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        // 'é' is two bytes: cutting at 1 must not split it.
        let (text, truncated) = truncate_text("é", 1);
        assert!(truncated);
        assert_eq!(text, "");
        assert!(text.is_char_boundary(text.len()));
    }

    #[tokio::test]
    async fn exec_enforces_a_timeout() {
        let dir = TempDir::new().expect("temp dir");
        let workbench = FsProcessWorkbench::new(dir.path(), true, true)
            .with_exec_timeout(Duration::from_millis(200));

        let error = workbench
            .execute(
                "exec",
                json!({"program": "sleep", "args": ["30"]}),
                &full_authority(),
            )
            .await
            .expect_err("a hung command must not hang the worker");
        assert!(error.to_string().contains("timeout"), "{error}");
    }
}
