use std::{collections::BTreeSet, sync::Arc};

use agent_protocol::{
    AuthorityEnvelope, ContextItem, ConversationId, LocalAuthority, Ticket, TicketId, WorkerReport,
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
    async fn update(
        &self,
        ticket_id: TicketId,
        context: Vec<ContextItem>,
    ) -> Result<(), KernelError>;
    async fn cancel(&self, ticket_id: TicketId) -> Result<(), KernelError>;
}

/// Deterministic ceiling on the authority a ticket may carry.
///
/// The orchestrator proposes a ticket, authority envelope included, but a model must not
/// be the thing that decides what a worker is allowed to do. The kernel clamps every
/// proposal to this policy, so the worst a confused or manipulated orchestrator can do is
/// request authority it already had.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TicketAuthorityPolicy {
    pub max_local: LocalAuthority,
    #[serde(default)]
    pub grantable_capabilities: BTreeSet<String>,
}

impl TicketAuthorityPolicy {
    /// Local reads only, no external capabilities.
    pub fn read_only() -> Self {
        Self::default()
    }

    pub fn with_local(mut self, local: LocalAuthority) -> Self {
        self.max_local = local;
        self
    }

    pub fn with_capabilities<I, S>(mut self, capabilities: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.grantable_capabilities = capabilities.into_iter().map(Into::into).collect();
        self
    }

    /// Intersect a proposed envelope with the policy. Never widens.
    pub fn clamp(&self, proposed: AuthorityEnvelope) -> AuthorityEnvelope {
        AuthorityEnvelope {
            local: LocalAuthority {
                read_workspace: proposed.local.read_workspace && self.max_local.read_workspace,
                write_workspace: proposed.local.write_workspace && self.max_local.write_workspace,
                execute_local: proposed.local.execute_local && self.max_local.execute_local,
            },
            external_capabilities: proposed
                .external_capabilities
                .into_iter()
                .filter(|capability| self.grantable_capabilities.contains(capability))
                .collect(),
        }
    }
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
    Respond {
        text: String,
    },
    Submit {
        ticket: NewTicket,
    },
    Update {
        ticket_id: TicketId,
        context: Vec<ContextItem>,
    },
    Cancel {
        ticket_id: TicketId,
    },
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
    Submitted {
        ticket: Ticket,
    },
    Updated {
        ticket_id: TicketId,
        context: Vec<ContextItem>,
    },
    Cancelled {
        ticket_id: TicketId,
    },
    WorkerReported {
        report: WorkerReport,
    },
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
    authority_policy: TicketAuthorityPolicy,
}

impl<M, Mem, Store, Work> OrchestrationKernel<M, Mem, Store, Work>
where
    M: ModelGateway + 'static,
    Mem: MemoryService + 'static,
    Store: ConversationStore + 'static,
    Work: WorkController + 'static,
{
    pub fn new(
        model: Arc<M>,
        memory: Arc<Mem>,
        store: Arc<Store>,
        work: Arc<Work>,
        authority_policy: TicketAuthorityPolicy,
    ) -> Self {
        Self {
            model,
            memory,
            store,
            work,
            authority_policy,
        }
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

A ticket's authority is a request, not a grant. The kernel clamps every ticket to a
deterministic policy, so requesting more authority than the deployment permits simply
removes it. Ask only for the authority the task needs.

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
            .complete(ModelRequest::json(messages, 0.2))
            .await
            .map_err(|e| KernelError::Model(e.to_string()))?;
        let envelope: OrchestratorEnvelope =
            serde_json::from_str(&raw).map_err(|e| KernelError::InvalidDecision(e.to_string()))?;

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
                        // Clamped, not trusted: see `TicketAuthorityPolicy`.
                        authority: self.authority_policy.clamp(ticket.authority),
                    };
                    self.work.submit(ticket.clone()).await?;
                    outcome
                        .ticket_events
                        .push(KernelTicketEvent::Submitted { ticket });
                }
                OrchestratorAction::Update { ticket_id, context } => {
                    self.work.update(ticket_id, context.clone()).await?;
                    outcome
                        .ticket_events
                        .push(KernelTicketEvent::Updated { ticket_id, context });
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct ScriptedModel(String);

    #[async_trait]
    impl ModelGateway for ScriptedModel {
        async fn complete(
            &self,
            _request: ModelRequest,
        ) -> Result<String, model_gateway::ModelError> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct NullMemory;

    #[async_trait]
    impl MemoryService for NullMemory {
        async fn retrieve(
            &self,
            _query: &str,
            _limit: usize,
        ) -> Result<Vec<MemoryItem>, KernelError> {
            Ok(Vec::new())
        }
        async fn store(&self, _text: &str) -> Result<(), KernelError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct NullStore;

    #[async_trait]
    impl ConversationStore for NullStore {
        async fn recent(
            &self,
            _conversation_id: ConversationId,
            _limit: usize,
        ) -> Result<Vec<ModelMessage>, KernelError> {
            Ok(Vec::new())
        }
        async fn append(
            &self,
            _conversation_id: ConversationId,
            _message: ModelMessage,
        ) -> Result<(), KernelError> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingWork(Mutex<Vec<Ticket>>);

    #[async_trait]
    impl WorkController for RecordingWork {
        async fn submit(&self, ticket: Ticket) -> Result<(), KernelError> {
            self.0.lock().expect("lock").push(ticket);
            Ok(())
        }
        async fn update(
            &self,
            _ticket_id: TicketId,
            _context: Vec<ContextItem>,
        ) -> Result<(), KernelError> {
            Ok(())
        }
        async fn cancel(&self, _ticket_id: TicketId) -> Result<(), KernelError> {
            Ok(())
        }
    }

    /// An orchestrator asking for far more authority than the deployment allows.
    fn greedy_submission() -> String {
        json!({
            "actions": [{
                "type": "submit",
                "ticket": {
                    "objective": "investigate the failure",
                    "deliverable": "a root cause",
                    "authority": {
                        "local": {
                            "read_workspace": true,
                            "write_workspace": true,
                            "execute_local": true
                        },
                        "external_capabilities": [
                            "web.search",
                            "infrastructure.host.inspect",
                            "communication.email.send"
                        ]
                    }
                }
            }]
        })
        .to_string()
    }

    async fn submit_under(policy: TicketAuthorityPolicy) -> Ticket {
        let work = Arc::new(RecordingWork::default());
        let kernel = OrchestrationKernel::new(
            Arc::new(ScriptedModel(greedy_submission())),
            Arc::new(NullMemory),
            Arc::new(NullStore),
            Arc::clone(&work),
            policy,
        );

        kernel
            .handle_user_turn(Uuid::new_v4(), "why is CI failing?")
            .await
            .expect("turn");

        let tickets = work.0.lock().expect("lock");
        tickets.first().cloned().expect("a ticket was submitted")
    }

    #[tokio::test]
    async fn the_orchestrator_cannot_grant_itself_capabilities() {
        let ticket =
            submit_under(TicketAuthorityPolicy::read_only().with_capabilities(["web.search"]))
                .await;

        assert_eq!(
            ticket.authority.external_capabilities,
            ["web.search".to_string()]
                .into_iter()
                .collect::<BTreeSet<_>>(),
            "only policy-grantable capabilities survive"
        );
    }

    #[tokio::test]
    async fn the_orchestrator_cannot_grant_itself_local_authority() {
        let ticket = submit_under(TicketAuthorityPolicy::read_only()).await;

        assert!(ticket.authority.local.read_workspace);
        assert!(
            !ticket.authority.local.write_workspace,
            "writes were not grantable"
        );
        assert!(
            !ticket.authority.local.execute_local,
            "execution was not grantable"
        );
        assert!(ticket.authority.external_capabilities.is_empty());
    }

    #[tokio::test]
    async fn a_permissive_policy_still_only_grants_what_was_asked_for() {
        let policy = TicketAuthorityPolicy::default()
            .with_local(LocalAuthority {
                read_workspace: true,
                write_workspace: true,
                execute_local: true,
            })
            .with_capabilities(["web.search", "infrastructure.host.inspect"]);

        let ticket = submit_under(policy).await;
        assert!(ticket.authority.local.execute_local);
        assert_eq!(ticket.authority.external_capabilities.len(), 2);
        assert!(
            !ticket
                .authority
                .external_capabilities
                .contains("communication.email.send"),
            "a capability outside the policy is never granted"
        );
    }

    #[test]
    fn clamping_never_widens_authority() {
        let policy = TicketAuthorityPolicy::default().with_capabilities(["web.search"]);
        let clamped = policy.clamp(AuthorityEnvelope::default());
        assert!(
            clamped.external_capabilities.is_empty(),
            "an unasked-for capability is not added"
        );
    }
}
