use std::collections::BTreeSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
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

/// What the worker sees when a capability succeeds.
///
/// Deliberately no provider field: which concrete adapter served the call is the broker's
/// operational business, and a worker that can read it is a worker that can start
/// depending on it. The broker records the provider in its telemetry and audit log
/// instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityResult {
    pub capability: String,
    pub data: Value,
    #[serde(default)]
    pub metadata: Value,
}

/// Stable failure surface exposed across the worker/broker seam.
///
/// Provider identities, transport failures and credential-bearing details remain inside
/// the broker. An in-process broker and a future RPC client expose the same bounded
/// contract to the worker runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Error)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CapabilityFailure {
    #[error("capability denied")]
    #[serde(rename = "CAPABILITY_DENIED")]
    Denied,
    #[error("no capability matches the request")]
    #[serde(rename = "CAPABILITY_GAP")]
    Gap,
    #[error("capability temporarily unavailable")]
    #[serde(rename = "CAPABILITY_TEMPORARILY_UNAVAILABLE")]
    TemporarilyUnavailable,
    #[error("capability request not understood")]
    #[serde(rename = "CAPABILITY_REQUEST_NOT_UNDERSTOOD")]
    RequestNotUnderstood,
    #[error("capability arguments invalid")]
    #[serde(rename = "CAPABILITY_ARGUMENTS_INVALID")]
    InvalidArguments,
}

impl CapabilityFailure {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Denied => "CAPABILITY_DENIED",
            Self::Gap => "CAPABILITY_GAP",
            Self::TemporarilyUnavailable => "CAPABILITY_TEMPORARILY_UNAVAILABLE",
            Self::RequestNotUnderstood => "CAPABILITY_REQUEST_NOT_UNDERSTOOD",
            Self::InvalidArguments => "CAPABILITY_ARGUMENTS_INVALID",
        }
    }
}

/// Port used by workers to request external capabilities.
///
/// The worker runtime depends on this contract rather than the broker's concrete type.
/// Implementations may call an in-process broker, cross an RPC boundary, or record calls
/// for tests without changing the worker loop.
#[async_trait]
pub trait CapabilityClient: Send + Sync {
    async fn request(
        &self,
        request: &CapabilityRequest,
    ) -> Result<CapabilityResult, CapabilityFailure>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Completed,
    Blocked,
    Failed,
}

/// Lifecycle state of a unit of work, shared by the kernel and the client API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkState {
    Queued,
    Running,
    Completed,
    Blocked,
    Failed,
    Cancelled,
}

impl From<&WorkerStatus> for WorkState {
    fn from(status: &WorkerStatus) -> Self {
        match status {
            WorkerStatus::Completed => Self::Completed,
            WorkerStatus::Blocked => Self::Blocked,
            WorkerStatus::Failed => Self::Failed,
        }
    }
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
