use std::{path::{Path, PathBuf}, sync::Arc};

use agent_protocol::{CapabilityRequest, Ticket, WorkerId, WorkerReport, WorkerStatus};
use async_trait::async_trait;
use capability_broker::{BrokerError, CapabilityBroker, CapabilityTranslator};
use model_gateway::{ModelGateway, ModelMessage, ModelRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{fs, process::Command};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("model error: {0}")]
    Model(String),
    #[error("invalid worker action: {0}")]
    InvalidAction(String),
    #[error("local workbench error: {0}")]
    Local(String),
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
    async fn execute(&self, operation: &str, arguments: Value) -> Result<Value, WorkerError>;
}

/// Workspace-constrained primitives. This is NOT a substitute for an OS/container sandbox.
pub struct FsProcessWorkbench {
    root: PathBuf,
    allow_write: bool,
    allow_execute: bool,
}

impl FsProcessWorkbench {
    pub fn new(root: impl Into<PathBuf>, allow_write: bool, allow_execute: bool) -> Self {
        Self { root: root.into(), allow_write, allow_execute }
    }

    fn resolve_existing(&self, relative: &str) -> Result<PathBuf, WorkerError> {
        let path = self.root.join(relative);
        let canonical_root = self.root.canonicalize().map_err(|e| WorkerError::Local(e.to_string()))?;
        let canonical = path.canonicalize().map_err(|e| WorkerError::Local(e.to_string()))?;
        if !canonical.starts_with(canonical_root) {
            return Err(WorkerError::Local("path escapes workspace".into()));
        }
        Ok(canonical)
    }

    fn resolve_for_write(&self, relative: &str) -> Result<PathBuf, WorkerError> {
        let path = self.root.join(relative);
        let parent = path.parent().ok_or_else(|| WorkerError::Local("invalid path".into()))?;
        let canonical_root = self.root.canonicalize().map_err(|e| WorkerError::Local(e.to_string()))?;
        let canonical_parent = parent.canonicalize().map_err(|e| WorkerError::Local(e.to_string()))?;
        if !canonical_parent.starts_with(canonical_root) {
            return Err(WorkerError::Local("path escapes workspace".into()));
        }
        Ok(path)
    }
}

#[async_trait]
impl LocalWorkbench for FsProcessWorkbench {
    async fn execute(&self, operation: &str, arguments: Value) -> Result<Value, WorkerError> {
        match operation {
            "read_file" => {
                let path = arguments.get("path").and_then(Value::as_str)
                    .ok_or_else(|| WorkerError::Local("read_file requires path".into()))?;
                let path = self.resolve_existing(path)?;
                let content = fs::read_to_string(path).await.map_err(|e| WorkerError::Local(e.to_string()))?;
                Ok(json!({"content": content}))
            }
            "write_file" => {
                if !self.allow_write { return Err(WorkerError::Local("workspace writes denied".into())); }
                let path = arguments.get("path").and_then(Value::as_str)
                    .ok_or_else(|| WorkerError::Local("write_file requires path".into()))?;
                let content = arguments.get("content").and_then(Value::as_str)
                    .ok_or_else(|| WorkerError::Local("write_file requires content".into()))?;
                let path = self.resolve_for_write(path)?;
                fs::write(path, content).await.map_err(|e| WorkerError::Local(e.to_string()))?;
                Ok(json!({"ok": true}))
            }
            "list_dir" => {
                let path = arguments.get("path").and_then(Value::as_str).unwrap_or(".");
                let path = self.resolve_existing(path)?;
                let mut dir = fs::read_dir(path).await.map_err(|e| WorkerError::Local(e.to_string()))?;
                let mut entries = Vec::new();
                while let Some(entry) = dir.next_entry().await.map_err(|e| WorkerError::Local(e.to_string()))? {
                    entries.push(entry.file_name().to_string_lossy().to_string());
                }
                Ok(json!({"entries": entries}))
            }
            "exec" => {
                if !self.allow_execute { return Err(WorkerError::Local("local execution denied".into())); }
                let program = arguments.get("program").and_then(Value::as_str)
                    .ok_or_else(|| WorkerError::Local("exec requires program".into()))?;
                let args: Vec<&str> = arguments.get("args")
                    .and_then(Value::as_array)
                    .map(|items| items.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let cwd = arguments.get("cwd").and_then(Value::as_str).unwrap_or(".");
                let cwd = self.resolve_existing(cwd)?;

                let output = Command::new(program)
                    .args(args)
                    .current_dir(cwd)
                    .output()
                    .await
                    .map_err(|e| WorkerError::Local(e.to_string()))?;

                Ok(json!({
                    "status": output.status.code(),
                    "stdout": String::from_utf8_lossy(&output.stdout),
                    "stderr": String::from_utf8_lossy(&output.stderr),
                }))
            }
            _ => Err(WorkerError::Local(format!("unknown local operation: {operation}"))),
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
        Self { model, broker, workbench, max_steps: 128 }
    }

    pub async fn run(&self, ticket: Ticket) -> Result<WorkerReport, WorkerError> {
        let worker_id: WorkerId = Uuid::new_v4();
        let mut observations: Vec<String> = Vec::new();

        for _ in 0..self.max_steps {
            let action = self.next_action(&ticket, &observations).await?;
            match action {
                WorkerAction::Local { operation, arguments } => {
                    let result = self.workbench.execute(&operation, arguments).await?;
                    observations.push(json!({"local_operation": operation, "result": result}).to_string());
                }
                WorkerAction::Capability { request } => {
                    let result = self.broker.execute(
                        &CapabilityRequest {
                            worker_id,
                            ticket_id: ticket.id,
                            request,
                        },
                        &ticket.authority,
                    ).await?;
                    observations.push(json!({"capability_result": result}).to_string());
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

    async fn next_action(&self, ticket: &Ticket, observations: &[String]) -> Result<WorkerAction, WorkerError> {
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

        let raw = self.model.complete(ModelRequest {
            messages: vec![ModelMessage::system(system), ModelMessage::user(input.to_string())],
            temperature: 0.1,
        }).await.map_err(|e| WorkerError::Model(e.to_string()))?;

        serde_json::from_str(&raw).map_err(|e| WorkerError::InvalidAction(e.to_string()))
    }
}
