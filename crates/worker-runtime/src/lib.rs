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
use capability_broker::{BrokerError, CapabilityBroker, CapabilityTranslator};
use model_gateway::{ModelGateway, ModelMessage, ModelRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{fs, process::Command, time};
use uuid::Uuid;

pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_MAX_STEPS: usize = 128;

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
            exec_timeout: DEFAULT_EXEC_TIMEOUT,
            exec_env_allowlist: vec!["PATH".to_string()],
        }
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
                Ok(json!({ "content": content }))
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

                Ok(json!({
                    "status": output.status.code(),
                    "stdout": String::from_utf8_lossy(&output.stdout),
                    "stderr": String::from_utf8_lossy(&output.stderr),
                }))
            }
            _ => Err(WorkerError::Local(format!(
                "unknown local operation: {operation}"
            ))),
        }
    }
}

pub struct WorkerRuntime<M, T, W> {
    model: Arc<M>,
    broker: Arc<CapabilityBroker<T>>,
    workbench: Arc<W>,
    max_steps: usize,
}

impl<M, T, W> WorkerRuntime<M, T, W>
where
    M: ModelGateway + 'static,
    T: CapabilityTranslator + 'static,
    W: LocalWorkbench + 'static,
{
    pub fn new(model: Arc<M>, broker: Arc<CapabilityBroker<T>>, workbench: Arc<W>) -> Self {
        Self {
            model,
            broker,
            workbench,
            max_steps: DEFAULT_MAX_STEPS,
        }
    }

    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }

    pub async fn run(&self, ticket: Ticket) -> Result<WorkerReport, WorkerError> {
        let worker_id: WorkerId = Uuid::new_v4();
        let mut observations: Vec<String> = Vec::new();

        for _ in 0..self.max_steps {
            let action = self.next_action(&ticket, &observations).await?;
            match action {
                WorkerAction::Local {
                    operation,
                    arguments,
                } => {
                    // A denial is an observation, not a worker crash: the model should be
                    // able to see the boundary and choose another route or report blocked.
                    let observation = match self
                        .workbench
                        .execute(&operation, arguments, &ticket.authority.local)
                        .await
                    {
                        Ok(result) => json!({"local_operation": operation, "result": result}),
                        Err(error @ WorkerError::Denied(_)) => {
                            json!({"local_operation": operation, "denied": error.to_string()})
                        }
                        Err(error) => return Err(error),
                    };
                    observations.push(observation.to_string());
                }
                WorkerAction::Capability { request } => {
                    let result = self
                        .broker
                        .execute(
                            &CapabilityRequest {
                                worker_id,
                                ticket_id: ticket.id,
                                request,
                            },
                            &ticket.authority,
                        )
                        .await;
                    let observation = match result {
                        Ok(result) => json!({ "capability_result": result }),
                        // Provider topology and transport detail stay below the boundary;
                        // the worker sees a stable typed code it can act on.
                        Err(error) => json!({"capability_error": error.code()}),
                    };
                    observations.push(observation.to_string());
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

        Err(WorkerError::StepLimit)
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
- read_file {path}
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
            "recent_observations": observations.iter().rev().take(16).rev().collect::<Vec<_>>(),
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
