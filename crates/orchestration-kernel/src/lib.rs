use std::sync::Arc;

use agent_protocol::{
    AuthorityEnvelope, ContextItem, ConversationId, Ticket, TicketId, WorkerReport,
};
use async_trait::async_trait;
use model_gateway::{ModelGateway, ModelMessage, ModelRequest};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum KernelError {
    #[error("model error: {0}")]
    Model(String),
    #[error("invalid orchestrator response: {0}")]
    InvalidDecision(String),
    #[error("memory error: {0}")]
    Memory(String),
    #[error("work controller error: {0}")]
    Work(String),
    #[error("conversation store error: {0}")]
    Store(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub text: String,
    pub relevance: f32,
}

#[async_trait]
pub trait MemoryService: Send + Sync {
    async fn retrieve(&self, query: &str, limit: usize) -> Result<Vec<MemoryItem>, KernelError>;
    async fn store(&self, text: &str) -> Result<(), KernelError>;
}

#[async_trait]
pub trait ConversationStore: Send + Sync {
    async fn recent(
        &self,
        conversation_id: ConversationId,
        limit: usize,
    ) -> Result<Vec<ModelMessage>, KernelError>;

    async fn append(
        &self,
        conversation_id: ConversationId,
        message: ModelMessage,
    ) -> Result<(), KernelError>;
}

#[async_trait]
pub trait WorkController: Send + Sync {
    async fn submit(&self, ticket: Ticket) -> Result<(), KernelError>;
    async fn update(&self, ticket_id: TicketId, context: Vec<ContextItem>) -> Result<(), KernelError>;
    async fn cancel(&self, ticket_id: TicketId) -> Result<(), KernelError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewTicket {
    pub objective: String,
    #[serde(default)]
    pub context: Vec<ContextItem>,
    #[serde(default)]
    pub constraints: Vec<String>,
    pub deliverable: String,
    #[serde(default)]
    pub authority: AuthorityEnvelope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrchestratorAction {
    Respond { text: String },
    Submit { ticket: NewTicket },
    Update { ticket_id: TicketId, context: Vec<ContextItem> },
    Cancel { ticket_id: TicketId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorEnvelope {
    pub actions: Vec<OrchestratorAction>,
    #[serde(default)]
    pub memory_proposals: Vec<String>,
}

/// Domain-level events produced by the kernel. These are intentionally transport- and UI-agnostic.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KernelTicketEvent {
    Submitted { ticket: Ticket },
    Updated { ticket_id: TicketId, context: Vec<ContextItem> },
    Cancelled { ticket_id: TicketId },
    WorkerReported { report: WorkerReport },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KernelOutcome {
    #[serde(default)]
    pub user_messages: Vec<String>,
    #[serde(default)]
    pub ticket_events: Vec<KernelTicketEvent>,
}

pub struct OrchestrationKernel<M, Mem, Store, Work> {
    model: Arc<M>,
    memory: Arc<Mem>,
    store: Arc<Store>,
    work: Arc<Work>,
}

impl<M, Mem, Store, Work> OrchestrationKernel<M, Mem, Store, Work>
where
    M: ModelGateway + 'static,
    Mem: MemoryService + 'static,
    Store: ConversationStore + 'static,
    Work: WorkController + 'static,
{
    pub fn new(model: Arc<M>, memory: Arc<Mem>, store: Arc<Store>, work: Arc<Work>) -> Self {
        Self { model, memory, store, work }
    }

    pub async fn handle_user_turn(
        &self,
        conversation_id: ConversationId,
        user_text: &str,
    ) -> Result<KernelOutcome, KernelError> {
        self.store
            .append(conversation_id, ModelMessage::user(user_text))
            .await?;
        let memories = self.memory.retrieve(user_text, 8).await?;
        let recent = self.store.recent(conversation_id, 24).await?;
        self.invoke(conversation_id, recent, memories, None).await
    }

    pub async fn handle_worker_result(
        &self,
        report: WorkerReport,
    ) -> Result<KernelOutcome, KernelError> {
        let conversation_id = report.conversation_id;
        let event = json!({"worker_result": &report}).to_string();
        let memories = self.memory.retrieve(&event, 4).await?;
        let recent = self.store.recent(conversation_id, 24).await?;
        let mut outcome = self
            .invoke(conversation_id, recent, memories, Some(event))
            .await?;
        outcome
            .ticket_events
            .insert(0, KernelTicketEvent::WorkerReported { report });
        Ok(outcome)
    }

    /// Direct control-plane cancellation for UI/CLI clients. No model round-trip is required.
    pub async fn cancel_ticket(
        &self,
        _conversation_id: ConversationId,
        ticket_id: TicketId,
    ) -> Result<KernelOutcome, KernelError> {
        self.work.cancel(ticket_id).await?;
        Ok(KernelOutcome {
            user_messages: Vec::new(),
            ticket_events: vec![KernelTicketEvent::Cancelled { ticket_id }],
        })
    }

    /// Direct control-plane ticket context update. This is not worker memory; it changes the active ticket.
    pub async fn update_ticket(
        &self,
        _conversation_id: ConversationId,
        ticket_id: TicketId,
        context: Vec<ContextItem>,
    ) -> Result<KernelOutcome, KernelError> {
        self.work.update(ticket_id, context.clone()).await?;
        Ok(KernelOutcome {
            user_messages: Vec::new(),
            ticket_events: vec![KernelTicketEvent::Updated { ticket_id, context }],
        })
    }

    async fn invoke(
        &self,
        conversation_id: ConversationId,
        mut recent: Vec<ModelMessage>,
        memories: Vec<MemoryItem>,
        event: Option<String>,
    ) -> Result<KernelOutcome, KernelError> {
        let system = r#"
You are the persistent orchestrator.

Responsibilities:
- understand the user and maintain the big picture;
- decompose work into self-contained tickets;
- decide what context each worker needs;
- evaluate bounded worker results;
- synthesize and communicate.

You DO NOT perform operational work and you have NO operational tools.
You cannot use shell, files, web, MCP, APIs, databases, GitHub, cloud, or the Capability Broker.
Assume work can be attempted unless a worker reports otherwise.

Your only control-plane actions are:
- respond to the user;
- submit a work ticket;
- update a ticket;
- cancel a ticket.

Workers have NO durable memory. A worker knows only what you put in its ticket plus its task-local observations. Tickets must therefore be self-contained.

Return JSON:
{
  "actions": [
    {"type":"respond","text":"..."},
    {"type":"submit","ticket":{...}},
    {"type":"update","ticket_id":"uuid","context":[]},
    {"type":"cancel","ticket_id":"uuid"}
  ],
  "memory_proposals": ["only durable facts/decisions worth retaining"]
}
"#;

        let context = json!({
            "retrieved_memories": memories,
            "external_event": event,
        });

        let mut messages = vec![
            ModelMessage::system(system),
            ModelMessage::system(context.to_string()),
        ];
        messages.append(&mut recent);

        let raw = self
            .model
            .complete(ModelRequest {
                messages,
                temperature: 0.2,
            })
            .await
            .map_err(|e| KernelError::Model(e.to_string()))?;
        let envelope: OrchestratorEnvelope = serde_json::from_str(&raw)
            .map_err(|e| KernelError::InvalidDecision(e.to_string()))?;

        let mut outcome = KernelOutcome::default();
        for action in envelope.actions {
            match action {
                OrchestratorAction::Respond { text } => {
                    self.store
                        .append(conversation_id, ModelMessage::assistant(&text))
                        .await?;
                    outcome.user_messages.push(text);
                }
                OrchestratorAction::Submit { ticket } => {
                    let ticket = Ticket {
                        id: Uuid::new_v4(),
                        conversation_id,
                        objective: ticket.objective,
                        context: ticket.context,
                        constraints: ticket.constraints,
                        deliverable: ticket.deliverable,
                        authority: ticket.authority,
                    };
                    self.work.submit(ticket.clone()).await?;
                    outcome
                        .ticket_events
                        .push(KernelTicketEvent::Submitted { ticket });
                }
                OrchestratorAction::Update { ticket_id, context } => {
                    self.work.update(ticket_id, context.clone()).await?;
                    outcome.ticket_events.push(KernelTicketEvent::Updated {
                        ticket_id,
                        context,
                    });
                }
                OrchestratorAction::Cancel { ticket_id } => {
                    self.work.cancel(ticket_id).await?;
                    outcome
                        .ticket_events
                        .push(KernelTicketEvent::Cancelled { ticket_id });
                }
            }
        }

        // The kernel, not the model, owns the memory admission point.
        // This baseline accepts proposals; production policy can filter/review them first.
        for proposal in envelope.memory_proposals {
            self.memory.store(&proposal).await?;
        }

        Ok(outcome)
    }
}
