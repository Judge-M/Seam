//! Transport-agnostic seam between presentation clients and the orchestration kernel.
//!
//! A web UI, CLI, desktop client, chat bridge, or future HTTP/SSE adapter should depend on
//! this semantic contract rather than on workers, the Capability Broker, model providers,
//! memory implementations, or durable-workflow internals.

use std::{collections::HashMap, sync::Arc};

use agent_protocol::{
    ArtifactRef, ContextItem, ConversationId, Ticket, TicketId, WorkerReport, WorkerStatus,
};
use async_trait::async_trait;
use model_gateway::ModelGateway;
use orchestration_kernel::{
    ConversationStore, KernelError, KernelOutcome, KernelTicketEvent, MemoryService,
    OrchestrationKernel, WorkController,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;
use uuid::Uuid;

pub const API_VERSION: &str = "v1";

/// Commands are semantic user/client intentions, not transport requests.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientCommand {
    SendMessage {
        conversation_id: ConversationId,
        text: String,
    },
    CancelTicket {
        conversation_id: ConversationId,
        ticket_id: TicketId,
    },
    UpdateTicket {
        conversation_id: ConversationId,
        ticket_id: TicketId,
        context: Vec<ContextItem>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandAck {
    pub api_version: String,
    pub command_id: Uuid,
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub api_version: String,
    pub event_id: Uuid,
    pub conversation_id: ConversationId,
    pub event: ClientEvent,
}

/// Stable presentation events. These intentionally expose work, not worker/broker internals.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientEvent {
    MessageCreated {
        message_id: Uuid,
        role: MessageRole,
        text: String,
    },
    TicketCreated {
        ticket: TicketView,
    },
    TicketUpdated {
        ticket_id: TicketId,
        context: Vec<ContextItem>,
    },
    TicketStateChanged {
        ticket_id: TicketId,
        state: TicketState,
        summary: Option<String>,
    },
    ArtifactCreated {
        ticket_id: TicketId,
        artifact: ArtifactRef,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketView {
    pub id: TicketId,
    pub objective: String,
    pub deliverable: String,
    pub constraints: Vec<String>,
    pub context: Vec<ContextItem>,
    pub state: TicketState,
    pub summary: Option<String>,
    pub artifacts: Vec<ArtifactRef>,
}

impl From<&Ticket> for TicketView {
    fn from(ticket: &Ticket) -> Self {
        Self {
            id: ticket.id,
            objective: ticket.objective.clone(),
            deliverable: ticket.deliverable.clone(),
            constraints: ticket.constraints.clone(),
            context: ticket.context.clone(),
            state: TicketState::Queued,
            summary: None,
            artifacts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketState {
    Queued,
    Running,
    Completed,
    Blocked,
    Failed,
    Cancelled,
}

impl From<&WorkerStatus> for TicketState {
    fn from(status: &WorkerStatus) -> Self {
        match status {
            WorkerStatus::Completed => Self::Completed,
            WorkerStatus::Blocked => Self::Blocked,
            WorkerStatus::Failed => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageView {
    pub id: Uuid,
    pub role: MessageRole,
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConversationProjection {
    pub messages: Vec<MessageView>,
    pub tickets: HashMap<TicketId, TicketView>,
}

/// Read-side seam for initial page loads, reconnects, CLI inspection, and alternate UIs.
/// A production implementation can be backed by Postgres or a projection/event store.
#[async_trait]
pub trait ReadModel: Send + Sync {
    async fn conversation(
        &self,
        conversation_id: ConversationId,
    ) -> Result<ConversationProjection, ApiError>;

    async fn ticket(&self, ticket_id: TicketId) -> Result<Option<TicketView>, ApiError>;
}

/// Minimal projection useful for prototypes/tests. It demonstrates that the UI can be a pure
/// projection of semantic events; it is not intended as the production persistence layer.
#[derive(Default)]
pub struct InMemoryProjection {
    conversations: RwLock<HashMap<ConversationId, ConversationProjection>>,
}

#[async_trait]
impl EventSink for InMemoryProjection {
    async fn publish(&self, envelope: EventEnvelope) -> Result<(), ApiError> {
        let mut conversations = self.conversations.write().await;
        let projection = conversations.entry(envelope.conversation_id).or_default();

        match envelope.event {
            ClientEvent::MessageCreated {
                message_id,
                role,
                text,
            } => projection.messages.push(MessageView {
                id: message_id,
                role,
                text,
            }),
            ClientEvent::TicketCreated { ticket } => {
                projection.tickets.insert(ticket.id, ticket);
            }
            ClientEvent::TicketUpdated { ticket_id, context } => {
                if let Some(ticket) = projection.tickets.get_mut(&ticket_id) {
                    ticket.context.extend(context);
                }
            }
            ClientEvent::TicketStateChanged {
                ticket_id,
                state,
                summary,
            } => {
                if let Some(ticket) = projection.tickets.get_mut(&ticket_id) {
                    ticket.state = state;
                    ticket.summary = summary;
                }
            }
            ClientEvent::ArtifactCreated { ticket_id, artifact } => {
                if let Some(ticket) = projection.tickets.get_mut(&ticket_id) {
                    ticket.artifacts.push(artifact);
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl ReadModel for InMemoryProjection {
    async fn conversation(
        &self,
        conversation_id: ConversationId,
    ) -> Result<ConversationProjection, ApiError> {
        Ok(self
            .conversations
            .read()
            .await
            .get(&conversation_id)
            .cloned()
            .unwrap_or_default())
    }

    async fn ticket(&self, ticket_id: TicketId) -> Result<Option<TicketView>, ApiError> {
        let conversations = self.conversations.read().await;
        Ok(conversations
            .values()
            .find_map(|projection| projection.tickets.get(&ticket_id).cloned()))
    }
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("orchestration kernel error: {0}")]
    Kernel(#[from] KernelError),
    #[error("event sink error: {0}")]
    EventSink(String),
}

/// Presentation transports implement this. Examples: SSE/WebSocket broadcaster, CLI printer,
/// desktop event bus, or test collector.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn publish(&self, event: EventEnvelope) -> Result<(), ApiError>;
}

/// The semantic UI seam. It deliberately has no reference to MCP, workers, broker providers,
/// model vendor SDKs, or memory backend types beyond the kernel generics hidden behind `K`.
pub struct OrchestrationApi<K, E> {
    kernel: Arc<K>,
    events: Arc<E>,
}

impl<K, E> OrchestrationApi<K, E>
where
    K: KernelPort + 'static,
    E: EventSink + 'static,
{
    pub fn new(kernel: Arc<K>, events: Arc<E>) -> Self {
        Self { kernel, events }
    }

    pub async fn execute(&self, command: ClientCommand) -> Result<CommandAck, ApiError> {
        let command_id = Uuid::new_v4();

        match command {
            ClientCommand::SendMessage {
                conversation_id,
                text,
            } => {
                self.publish(
                    conversation_id,
                    ClientEvent::MessageCreated {
                        message_id: Uuid::new_v4(),
                        role: MessageRole::User,
                        text: text.clone(),
                    },
                )
                .await?;

                let outcome = self.kernel.handle_user_turn(conversation_id, &text).await?;
                self.publish_outcome(conversation_id, outcome).await?;
            }
            ClientCommand::CancelTicket {
                conversation_id,
                ticket_id,
            } => {
                let outcome = self.kernel.cancel_ticket(conversation_id, ticket_id).await?;
                self.publish_outcome(conversation_id, outcome).await?;
            }
            ClientCommand::UpdateTicket {
                conversation_id,
                ticket_id,
                context,
            } => {
                let outcome = self
                    .kernel
                    .update_ticket(conversation_id, ticket_id, context)
                    .await?;
                self.publish_outcome(conversation_id, outcome).await?;
            }
        }

        Ok(CommandAck {
            api_version: API_VERSION.to_owned(),
            command_id,
            accepted: true,
        })
    }

    /// Called by the durable work controller when a queued ticket starts executing.
    pub async fn ticket_started(
        &self,
        conversation_id: ConversationId,
        ticket_id: TicketId,
    ) -> Result<(), ApiError> {
        self.publish(
            conversation_id,
            ClientEvent::TicketStateChanged {
                ticket_id,
                state: TicketState::Running,
                summary: None,
            },
        )
        .await
    }

    /// Called by the durable worker controller when a worker finishes/blocks/fails.
    /// This keeps event/resume plumbing outside presentation clients.
    pub async fn worker_report(&self, report: WorkerReport) -> Result<(), ApiError> {
        let conversation_id = report.conversation_id;
        let outcome = self.kernel.handle_worker_result(report).await?;
        self.publish_outcome(conversation_id, outcome).await
    }

    async fn publish_outcome(
        &self,
        conversation_id: ConversationId,
        outcome: KernelOutcome,
    ) -> Result<(), ApiError> {
        for text in outcome.user_messages {
            self.publish(
                conversation_id,
                ClientEvent::MessageCreated {
                    message_id: Uuid::new_v4(),
                    role: MessageRole::Assistant,
                    text,
                },
            )
            .await?;
        }

        for ticket_event in outcome.ticket_events {
            match ticket_event {
                KernelTicketEvent::Submitted { ticket } => {
                    self.publish(
                        conversation_id,
                        ClientEvent::TicketCreated {
                            ticket: TicketView::from(&ticket),
                        },
                    )
                    .await?;
                }
                KernelTicketEvent::Updated { ticket_id, context } => {
                    self.publish(
                        conversation_id,
                        ClientEvent::TicketUpdated { ticket_id, context },
                    )
                    .await?;
                }
                KernelTicketEvent::Cancelled { ticket_id } => {
                    self.publish(
                        conversation_id,
                        ClientEvent::TicketStateChanged {
                            ticket_id,
                            state: TicketState::Cancelled,
                            summary: None,
                        },
                    )
                    .await?;
                }
                KernelTicketEvent::WorkerReported { report } => {
                    self.publish_worker_report(conversation_id, report).await?;
                }
            }
        }

        Ok(())
    }

    async fn publish_worker_report(
        &self,
        conversation_id: ConversationId,
        report: WorkerReport,
    ) -> Result<(), ApiError> {
        self.publish(
            conversation_id,
            ClientEvent::TicketStateChanged {
                ticket_id: report.ticket_id,
                state: TicketState::from(&report.status),
                summary: Some(report.summary.clone()),
            },
        )
        .await?;

        for artifact in report.artifacts {
            self.publish(
                conversation_id,
                ClientEvent::ArtifactCreated {
                    ticket_id: report.ticket_id,
                    artifact,
                },
            )
            .await?;
        }

        Ok(())
    }

    async fn publish(
        &self,
        conversation_id: ConversationId,
        event: ClientEvent,
    ) -> Result<(), ApiError> {
        self.events
            .publish(EventEnvelope {
                api_version: API_VERSION.to_owned(),
                event_id: Uuid::new_v4(),
                conversation_id,
                event,
            })
            .await
    }
}

/// Small port used by the seam. This prevents presentation code from depending on the kernel's
/// concrete model/memory/storage/workflow implementation.
#[async_trait]
pub trait KernelPort: Send + Sync {
    async fn handle_user_turn(
        &self,
        conversation_id: ConversationId,
        text: &str,
    ) -> Result<KernelOutcome, KernelError>;

    async fn handle_worker_result(
        &self,
        report: WorkerReport,
    ) -> Result<KernelOutcome, KernelError>;

    async fn cancel_ticket(
        &self,
        conversation_id: ConversationId,
        ticket_id: TicketId,
    ) -> Result<KernelOutcome, KernelError>;

    async fn update_ticket(
        &self,
        conversation_id: ConversationId,
        ticket_id: TicketId,
        context: Vec<ContextItem>,
    ) -> Result<KernelOutcome, KernelError>;
}

#[async_trait]
impl<M, Mem, Store, Work> KernelPort for OrchestrationKernel<M, Mem, Store, Work>
where
    M: ModelGateway + 'static,
    Mem: MemoryService + 'static,
    Store: ConversationStore + 'static,
    Work: WorkController + 'static,
{
    async fn handle_user_turn(
        &self,
        conversation_id: ConversationId,
        text: &str,
    ) -> Result<KernelOutcome, KernelError> {
        OrchestrationKernel::handle_user_turn(self, conversation_id, text).await
    }

    async fn handle_worker_result(
        &self,
        report: WorkerReport,
    ) -> Result<KernelOutcome, KernelError> {
        OrchestrationKernel::handle_worker_result(self, report).await
    }

    async fn cancel_ticket(
        &self,
        conversation_id: ConversationId,
        ticket_id: TicketId,
    ) -> Result<KernelOutcome, KernelError> {
        OrchestrationKernel::cancel_ticket(self, conversation_id, ticket_id).await
    }

    async fn update_ticket(
        &self,
        conversation_id: ConversationId,
        ticket_id: TicketId,
        context: Vec<ContextItem>,
    ) -> Result<KernelOutcome, KernelError> {
        OrchestrationKernel::update_ticket(self, conversation_id, ticket_id, context).await
    }
}
