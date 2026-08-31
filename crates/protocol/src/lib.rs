use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub type ConversationId = Uuid;
pub type TicketId = Uuid;
pub type WorkerId = Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticket {
    pub id: TicketId,
    pub conversation_id: ConversationId,
    pub objective: String,
    #[serde(default)]
    pub context: Vec<ContextItem>,
    #[serde(default)]
    pub constraints: Vec<String>,
    pub deliverable: String,
    pub authority: AuthorityEnvelope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItem {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthorityEnvelope {
    pub local: LocalAuthority,
    #[serde(default)]
    pub external_capabilities: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalAuthority {
    pub read_workspace: bool,
    pub write_workspace: bool,
    pub execute_local: bool,
}

impl Default for LocalAuthority {
    fn default() -> Self {
        Self {
            read_workspace: true,
            write_workspace: false,
            execute_local: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityRequest {
    pub worker_id: WorkerId,
    pub ticket_id: TicketId,
    /// Natural-language operational intent. No project/user memory is attached.
    pub request: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityInvocation {
    pub capability: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityResult {
    pub capability: String,
    pub provider: String,
    pub data: Value,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Completed,
    Blocked,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerReport {
    pub worker_id: WorkerId,
    pub ticket_id: TicketId,
    pub conversation_id: ConversationId,
    pub status: WorkerStatus,
    pub summary: String,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub name: String,
    pub uri: String,
}
