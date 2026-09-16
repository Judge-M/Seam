use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex},
};

use agent_protocol::{
    AuthorityEnvelope, ContextItem, ConversationId, LocalAuthority, Ticket, TicketId, WorkState,
    WorkerId, WorkerReport,
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
    #[error("ticket {0} does not belong to this conversation")]
    TicketNotInConversation(TicketId),
    #[error("invalid work transition for ticket {ticket_id}: {reason}")]
    InvalidWorkTransition { ticket_id: TicketId, reason: String },
    #[error("worker report rejected for ticket {ticket_id}: {reason}")]
    InvalidWorkerReport { ticket_id: TicketId, reason: String },
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
    /// Atomically bind a queued ticket to the worker selected by the dispatcher and mark
    /// it running. Worker reports are accepted only from this identity.
    async fn assign(&self, ticket_id: TicketId, worker_id: WorkerId) -> Result<(), KernelError>;
    async fn update(
        &self,
        ticket_id: TicketId,
        context: Vec<ContextItem>,
    ) -> Result<(), KernelError>;
    async fn cancel(&self, ticket_id: TicketId) -> Result<(), KernelError>;

    /// Validate a terminal worker report against ticket ownership, assignment and
    /// lifecycle state, then atomically apply its state transition.
    async fn accept_report(&self, report: &WorkerReport) -> Result<(), KernelError>;

    /// Work still in flight for a conversation, for orchestrator context assembly.
    async fn active_work(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Vec<ActiveWork>, KernelError>;

    /// Which conversation a ticket belongs to, for ownership checks. `None` if unknown.
    async fn ticket_owner(
        &self,
        ticket_id: TicketId,
    ) -> Result<Option<ConversationId>, KernelError>;
}

/// One unit of work as the orchestrator sees it.
///
/// Deliberately three fields: the orchestrator needs to know what work it has in flight
/// to use `update` and `cancel` meaningfully, and nothing more. Worker logs, tool state
/// and raw observations stay below the boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveWork {
    pub id: TicketId,
    pub objective: String,
    pub state: WorkState,
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
        // A report is untrusted ingress until the work controller binds all three ids and
        // atomically accepts the running -> terminal transition.
        self.work.accept_report(&report).await?;
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
        conversation_id: ConversationId,
        ticket_id: TicketId,
    ) -> Result<KernelOutcome, KernelError> {
        self.ensure_ticket_in_conversation(conversation_id, ticket_id)
            .await?;
        self.work.cancel(ticket_id).await?;
        Ok(KernelOutcome {
            user_messages: Vec::new(),
            ticket_events: vec![KernelTicketEvent::Cancelled { ticket_id }],
        })
    }

    /// Direct control-plane ticket context update. This is not worker memory; it changes the active ticket.
    pub async fn update_ticket(
        &self,
        conversation_id: ConversationId,
        ticket_id: TicketId,
        context: Vec<ContextItem>,
    ) -> Result<KernelOutcome, KernelError> {
        self.ensure_ticket_in_conversation(conversation_id, ticket_id)
            .await?;
        self.work.update(ticket_id, context.clone()).await?;
        Ok(KernelOutcome {
            user_messages: Vec::new(),
            ticket_events: vec![KernelTicketEvent::Updated { ticket_id, context }],
        })
    }

    /// A ticket id is not a capability. Clients hold ids for one conversation, so the
    /// seam checks the pairing here rather than trusting every future frontend to
    /// remember to.
    async fn ensure_ticket_in_conversation(
        &self,
        conversation_id: ConversationId,
        ticket_id: TicketId,
    ) -> Result<(), KernelError> {
        match self.work.ticket_owner(ticket_id).await? {
            Some(owner) if owner == conversation_id => Ok(()),
            // An unknown ticket is refused rather than passed through: "we lost track of
            // it" must not read as "anyone may cancel it".
            _ => Err(KernelError::TicketNotInConversation(ticket_id)),
        }
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
You cannot use shell, files, web, MCP, APIs, databases, code-hosting services, cloud, or the Capability Broker.
Assume work can be attempted unless a worker reports otherwise.

Your only control-plane actions are:
- respond to the user;
- submit a work ticket;
- update a ticket;
- cancel a ticket.

`active_work` lists the tickets you have created that are still in flight, with their
ids and states. Use those ids when updating or cancelling. It is a summary of your own
work, not worker logs — a worker's findings reach you only through its report.

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
            "active_work": self.work.active_work(conversation_id).await?,
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
                    self.ensure_ticket_in_conversation(conversation_id, ticket_id)
                        .await?;
                    self.work.update(ticket_id, context.clone()).await?;
                    outcome
                        .ticket_events
                        .push(KernelTicketEvent::Updated { ticket_id, context });
                }
                OrchestratorAction::Cancel { ticket_id } => {
                    self.ensure_ticket_in_conversation(conversation_id, ticket_id)
                        .await?;
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

    /// Records what was submitted, and delegates state/ownership to the real in-memory
    /// controller so the ownership and active-work paths are exercised as shipped.
    #[derive(Default)]
    struct RecordingWork {
        submitted: Mutex<Vec<Ticket>>,
        inner: InMemoryWorkController,
        cancelled: Mutex<Vec<TicketId>>,
        updated: Mutex<Vec<TicketId>>,
    }

    #[async_trait]
    impl WorkController for RecordingWork {
        async fn submit(&self, ticket: Ticket) -> Result<(), KernelError> {
            self.submitted.lock().expect("lock").push(ticket.clone());
            self.inner.submit(ticket).await
        }
        async fn assign(
            &self,
            ticket_id: TicketId,
            worker_id: WorkerId,
        ) -> Result<(), KernelError> {
            self.inner.assign(ticket_id, worker_id).await
        }
        async fn update(
            &self,
            ticket_id: TicketId,
            context: Vec<ContextItem>,
        ) -> Result<(), KernelError> {
            self.updated.lock().expect("lock").push(ticket_id);
            self.inner.update(ticket_id, context).await
        }
        async fn cancel(&self, ticket_id: TicketId) -> Result<(), KernelError> {
            self.cancelled.lock().expect("lock").push(ticket_id);
            self.inner.cancel(ticket_id).await
        }
        async fn accept_report(&self, report: &WorkerReport) -> Result<(), KernelError> {
            self.inner.accept_report(report).await
        }
        async fn active_work(
            &self,
            conversation_id: ConversationId,
        ) -> Result<Vec<ActiveWork>, KernelError> {
            self.inner.active_work(conversation_id).await
        }
        async fn ticket_owner(
            &self,
            ticket_id: TicketId,
        ) -> Result<Option<ConversationId>, KernelError> {
            self.inner.ticket_owner(ticket_id).await
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

        let tickets = work.submitted.lock().expect("lock");
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

    /// Builds a kernel whose orchestrator only ever replies, so control-plane calls can
    /// be tested without the model driving them.
    fn quiet_kernel() -> (
        OrchestrationKernel<ScriptedModel, NullMemory, NullStore, RecordingWork>,
        Arc<RecordingWork>,
    ) {
        let work = Arc::new(RecordingWork::default());
        let reply = json!({"actions": [{"type": "respond", "text": "ok"}]}).to_string();
        let kernel = OrchestrationKernel::new(
            Arc::new(ScriptedModel(reply)),
            Arc::new(NullMemory),
            Arc::new(NullStore),
            Arc::clone(&work),
            TicketAuthorityPolicy::read_only(),
        );
        (kernel, work)
    }

    #[tokio::test]
    async fn a_ticket_cannot_be_cancelled_from_another_conversation() {
        let work = Arc::new(RecordingWork::default());
        let owner = Uuid::new_v4();
        let ticket_id = Uuid::new_v4();
        work.submit(Ticket {
            id: ticket_id,
            conversation_id: owner,
            objective: "inspect CI".into(),
            context: Vec::new(),
            constraints: Vec::new(),
            deliverable: "a cause".into(),
            authority: AuthorityEnvelope::default(),
        })
        .await
        .expect("submit");

        let kernel = OrchestrationKernel::new(
            Arc::new(ScriptedModel(String::new())),
            Arc::new(NullMemory),
            Arc::new(NullStore),
            Arc::clone(&work),
            TicketAuthorityPolicy::read_only(),
        );

        let intruder = Uuid::new_v4();
        let error = kernel
            .cancel_ticket(intruder, ticket_id)
            .await
            .expect_err("a foreign conversation must not cancel this ticket");
        assert!(
            matches!(error, KernelError::TicketNotInConversation(id) if id == ticket_id),
            "{error}"
        );
        assert!(
            work.cancelled.lock().expect("lock").is_empty(),
            "the controller must never have been called"
        );

        // The owning conversation still can.
        kernel
            .cancel_ticket(owner, ticket_id)
            .await
            .expect("owner cancels");
        assert_eq!(work.cancelled.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn an_unknown_ticket_is_refused_rather_than_passed_through() {
        let (kernel, work) = quiet_kernel();
        let error = kernel
            .cancel_ticket(Uuid::new_v4(), Uuid::new_v4())
            .await
            .expect_err("unknown ticket must be refused");
        assert!(
            matches!(error, KernelError::TicketNotInConversation(_)),
            "{error}"
        );
        assert!(work.cancelled.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn updating_a_ticket_from_another_conversation_is_refused() {
        let (kernel, work) = quiet_kernel();
        let owner = Uuid::new_v4();
        let ticket = submit_under_kernel(&kernel, owner).await;

        let error = kernel
            .update_ticket(Uuid::new_v4(), ticket, vec![])
            .await
            .expect_err("foreign update must be refused");
        assert!(
            matches!(error, KernelError::TicketNotInConversation(_)),
            "{error}"
        );
        assert!(work.updated.lock().expect("lock").is_empty());

        kernel
            .update_ticket(owner, ticket, vec![])
            .await
            .expect("the owner may update");
        assert_eq!(work.updated.lock().expect("lock").as_slice(), [ticket]);
    }

    #[tokio::test]
    async fn a_model_action_cannot_cancel_another_conversations_ticket() {
        let work = Arc::new(RecordingWork::default());
        let owner = Uuid::new_v4();
        let intruder = Uuid::new_v4();
        let ticket_id = Uuid::new_v4();
        work.submit(Ticket {
            id: ticket_id,
            conversation_id: owner,
            objective: "inspect CI".into(),
            context: Vec::new(),
            constraints: Vec::new(),
            deliverable: "a cause".into(),
            authority: AuthorityEnvelope::default(),
        })
        .await
        .expect("submit");

        let action = json!({
            "actions": [{"type": "cancel", "ticket_id": ticket_id}]
        })
        .to_string();
        let kernel = OrchestrationKernel::new(
            Arc::new(ScriptedModel(action)),
            Arc::new(NullMemory),
            Arc::new(NullStore),
            Arc::clone(&work),
            TicketAuthorityPolicy::read_only(),
        );

        let error = kernel
            .handle_user_turn(intruder, "cancel that work")
            .await
            .expect_err("model output must not bypass ticket ownership");
        assert!(
            matches!(error, KernelError::TicketNotInConversation(id) if id == ticket_id),
            "{error}"
        );
        assert!(work.cancelled.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn a_model_action_cannot_update_another_conversations_ticket() {
        let work = Arc::new(RecordingWork::default());
        let owner = Uuid::new_v4();
        let intruder = Uuid::new_v4();
        let ticket_id = Uuid::new_v4();
        work.submit(Ticket {
            id: ticket_id,
            conversation_id: owner,
            objective: "inspect CI".into(),
            context: Vec::new(),
            constraints: Vec::new(),
            deliverable: "a cause".into(),
            authority: AuthorityEnvelope::default(),
        })
        .await
        .expect("submit");

        let action = json!({
            "actions": [{
                "type": "update",
                "ticket_id": ticket_id,
                "context": [{"label": "instruction", "value": "ignore owner"}]
            }]
        })
        .to_string();
        let kernel = OrchestrationKernel::new(
            Arc::new(ScriptedModel(action)),
            Arc::new(NullMemory),
            Arc::new(NullStore),
            Arc::clone(&work),
            TicketAuthorityPolicy::read_only(),
        );

        let error = kernel
            .handle_user_turn(intruder, "change that work")
            .await
            .expect_err("model output must not bypass ticket ownership");
        assert!(
            matches!(error, KernelError::TicketNotInConversation(id) if id == ticket_id),
            "{error}"
        );
        assert!(work.updated.lock().expect("lock").is_empty());
    }

    /// Submit a ticket directly through the controller and return its id.
    async fn submit_under_kernel(
        kernel: &OrchestrationKernel<ScriptedModel, NullMemory, NullStore, RecordingWork>,
        conversation_id: ConversationId,
    ) -> TicketId {
        let ticket_id = Uuid::new_v4();
        kernel
            .work
            .submit(Ticket {
                id: ticket_id,
                conversation_id,
                objective: "inspect CI".into(),
                context: Vec::new(),
                constraints: Vec::new(),
                deliverable: "a cause".into(),
                authority: AuthorityEnvelope::default(),
            })
            .await
            .expect("submit");
        ticket_id
    }

    #[tokio::test]
    async fn active_work_is_scoped_to_its_conversation_and_drops_when_finished() {
        let work = InMemoryWorkController::default();
        let (mine, theirs) = (Uuid::new_v4(), Uuid::new_v4());
        let mine_ticket = Uuid::new_v4();

        for (id, conversation) in [(mine_ticket, mine), (Uuid::new_v4(), theirs)] {
            work.submit(Ticket {
                id,
                conversation_id: conversation,
                objective: "inspect CI".into(),
                context: Vec::new(),
                constraints: Vec::new(),
                deliverable: "a cause".into(),
                authority: AuthorityEnvelope::default(),
            })
            .await
            .expect("submit");
        }

        let active = work.active_work(mine).await.expect("active");
        assert_eq!(active.len(), 1, "only this conversation's work");
        assert_eq!(active[0].id, mine_ticket);
        assert_eq!(active[0].state, WorkState::Queued);

        let worker_id = Uuid::new_v4();
        work.assign(mine_ticket, worker_id).await.expect("assign");
        assert_eq!(
            work.active_work(mine).await.expect("active")[0].state,
            WorkState::Running
        );

        // A finished ticket leaves the orchestrator's active-work view.
        work.accept_report(&WorkerReport {
            worker_id,
            ticket_id: mine_ticket,
            conversation_id: mine,
            status: agent_protocol::WorkerStatus::Completed,
            summary: "done".into(),
            artifacts: Vec::new(),
            notes: Vec::new(),
        })
        .await
        .expect("report");
        assert!(work.active_work(mine).await.expect("active").is_empty());
    }

    #[tokio::test]
    async fn worker_reports_must_match_the_assignment_and_cannot_be_replayed() {
        let work = InMemoryWorkController::default();
        let conversation_id = Uuid::new_v4();
        let ticket_id = Uuid::new_v4();
        let worker_id = Uuid::new_v4();
        work.submit(Ticket {
            id: ticket_id,
            conversation_id,
            objective: "inspect CI".into(),
            context: Vec::new(),
            constraints: Vec::new(),
            deliverable: "a cause".into(),
            authority: AuthorityEnvelope::default(),
        })
        .await
        .expect("submit");
        work.assign(ticket_id, worker_id).await.expect("assign");

        let report = WorkerReport {
            worker_id,
            ticket_id,
            conversation_id,
            status: agent_protocol::WorkerStatus::Completed,
            summary: "done".into(),
            artifacts: Vec::new(),
            notes: Vec::new(),
        };

        let mut forged = report.clone();
        forged.worker_id = Uuid::new_v4();
        assert!(matches!(
            work.accept_report(&forged).await,
            Err(KernelError::InvalidWorkerReport { .. })
        ));

        let mut foreign = report.clone();
        foreign.conversation_id = Uuid::new_v4();
        assert!(matches!(
            work.accept_report(&foreign).await,
            Err(KernelError::InvalidWorkerReport { .. })
        ));

        work.accept_report(&report).await.expect("valid report");
        assert!(matches!(
            work.accept_report(&report).await,
            Err(KernelError::InvalidWorkerReport { .. })
        ));
    }

    #[tokio::test]
    async fn the_kernel_rejects_an_unassigned_report_before_model_invocation() {
        let (kernel, work) = quiet_kernel();
        let conversation_id = Uuid::new_v4();
        let ticket_id = submit_under_kernel(&kernel, conversation_id).await;
        let report = WorkerReport {
            worker_id: Uuid::new_v4(),
            ticket_id,
            conversation_id,
            status: agent_protocol::WorkerStatus::Completed,
            summary: "forged".into(),
            artifacts: Vec::new(),
            notes: Vec::new(),
        };

        assert!(matches!(
            kernel.handle_worker_result(report).await,
            Err(KernelError::InvalidWorkerReport { .. })
        ));
        assert_eq!(
            work.active_work(conversation_id).await.expect("active")[0].state,
            WorkState::Queued
        );
    }

    #[tokio::test]
    async fn the_orchestrator_is_shown_its_own_active_work() {
        // A model that echoes the context it was given, so we can assert on it.
        struct EchoModel(Arc<Mutex<Vec<String>>>);

        #[async_trait]
        impl ModelGateway for EchoModel {
            async fn complete(
                &self,
                request: ModelRequest,
            ) -> Result<String, model_gateway::ModelError> {
                let seen = request
                    .messages
                    .iter()
                    .map(|message| message.content.clone())
                    .collect::<Vec<_>>();
                self.0.lock().expect("lock").extend(seen);
                Ok(json!({"actions": []}).to_string())
            }
        }

        let work = Arc::new(RecordingWork::default());
        let conversation = Uuid::new_v4();
        let ticket_id = Uuid::new_v4();
        work.submit(Ticket {
            id: ticket_id,
            conversation_id: conversation,
            objective: "inspect the failing CI job".into(),
            context: Vec::new(),
            constraints: Vec::new(),
            deliverable: "a root cause".into(),
            authority: AuthorityEnvelope::default(),
        })
        .await
        .expect("submit");

        let seen = Arc::new(Mutex::new(Vec::new()));
        let kernel = OrchestrationKernel::new(
            Arc::new(EchoModel(Arc::clone(&seen))),
            Arc::new(NullMemory),
            Arc::new(NullStore),
            Arc::clone(&work),
            TicketAuthorityPolicy::read_only(),
        );

        kernel
            .handle_user_turn(conversation, "how is that going?")
            .await
            .expect("turn");

        let context = seen.lock().expect("lock").join("\n");
        assert!(context.contains("active_work"), "context lacks active work");
        assert!(
            context.contains("inspect the failing CI job"),
            "the orchestrator cannot see its own in-flight ticket"
        );
        assert!(
            context.contains(&ticket_id.to_string()),
            "the orchestrator needs the id to update or cancel"
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

/// Prototype work controller: tracks ticket ownership and state in memory.
///
/// It exists so the kernel's ownership and active-work reads have a real implementation
/// to run against. Phase 5 replaces it with a durable runtime; the trait is the seam
/// that makes that a swap rather than a rewrite.
#[derive(Default)]
pub struct InMemoryWorkController {
    tickets: Mutex<HashMap<TicketId, TrackedTicket>>,
}

struct TrackedTicket {
    conversation_id: ConversationId,
    objective: String,
    state: WorkState,
    assigned_worker: Option<WorkerId>,
}

#[async_trait]
impl WorkController for InMemoryWorkController {
    async fn submit(&self, ticket: Ticket) -> Result<(), KernelError> {
        self.tickets
            .lock()
            .map_err(|_| KernelError::Work("work controller lock poisoned".into()))?
            .insert(
                ticket.id,
                TrackedTicket {
                    conversation_id: ticket.conversation_id,
                    objective: ticket.objective,
                    state: WorkState::Queued,
                    assigned_worker: None,
                },
            );
        Ok(())
    }

    async fn assign(&self, ticket_id: TicketId, worker_id: WorkerId) -> Result<(), KernelError> {
        let mut tickets = self
            .tickets
            .lock()
            .map_err(|_| KernelError::Work("work controller lock poisoned".into()))?;
        let tracked =
            tickets
                .get_mut(&ticket_id)
                .ok_or_else(|| KernelError::InvalidWorkTransition {
                    ticket_id,
                    reason: "ticket does not exist".into(),
                })?;
        if tracked.state != WorkState::Queued {
            return Err(KernelError::InvalidWorkTransition {
                ticket_id,
                reason: format!("expected queued ticket, found {:?}", tracked.state),
            });
        }
        tracked.assigned_worker = Some(worker_id);
        tracked.state = WorkState::Running;
        Ok(())
    }

    async fn update(
        &self,
        _ticket_id: TicketId,
        _context: Vec<ContextItem>,
    ) -> Result<(), KernelError> {
        Ok(())
    }

    async fn cancel(&self, ticket_id: TicketId) -> Result<(), KernelError> {
        let mut tickets = self
            .tickets
            .lock()
            .map_err(|_| KernelError::Work("work controller lock poisoned".into()))?;
        let tracked =
            tickets
                .get_mut(&ticket_id)
                .ok_or_else(|| KernelError::InvalidWorkTransition {
                    ticket_id,
                    reason: "ticket does not exist".into(),
                })?;
        if !matches!(tracked.state, WorkState::Queued | WorkState::Running) {
            return Err(KernelError::InvalidWorkTransition {
                ticket_id,
                reason: format!("cannot cancel ticket in {:?} state", tracked.state),
            });
        }
        tracked.state = WorkState::Cancelled;
        Ok(())
    }

    async fn accept_report(&self, report: &WorkerReport) -> Result<(), KernelError> {
        let mut tickets = self
            .tickets
            .lock()
            .map_err(|_| KernelError::Work("work controller lock poisoned".into()))?;
        let tracked =
            tickets
                .get_mut(&report.ticket_id)
                .ok_or_else(|| KernelError::InvalidWorkerReport {
                    ticket_id: report.ticket_id,
                    reason: "ticket does not exist".into(),
                })?;

        if tracked.conversation_id != report.conversation_id {
            return Err(KernelError::InvalidWorkerReport {
                ticket_id: report.ticket_id,
                reason: "conversation does not own this ticket".into(),
            });
        }
        if tracked.assigned_worker != Some(report.worker_id) {
            return Err(KernelError::InvalidWorkerReport {
                ticket_id: report.ticket_id,
                reason: "worker is not assigned to this ticket".into(),
            });
        }
        if tracked.state != WorkState::Running {
            return Err(KernelError::InvalidWorkerReport {
                ticket_id: report.ticket_id,
                reason: format!("expected running ticket, found {:?}", tracked.state),
            });
        }

        tracked.state = WorkState::from(&report.status);
        Ok(())
    }

    async fn active_work(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Vec<ActiveWork>, KernelError> {
        let tickets = self
            .tickets
            .lock()
            .map_err(|_| KernelError::Work("work controller lock poisoned".into()))?;
        Ok(tickets
            .iter()
            .filter(|(_, tracked)| {
                tracked.conversation_id == conversation_id
                    && matches!(tracked.state, WorkState::Queued | WorkState::Running)
            })
            .map(|(id, tracked)| ActiveWork {
                id: *id,
                objective: tracked.objective.clone(),
                state: tracked.state,
            })
            .collect())
    }

    async fn ticket_owner(
        &self,
        ticket_id: TicketId,
    ) -> Result<Option<ConversationId>, KernelError> {
        Ok(self
            .tickets
            .lock()
            .map_err(|_| KernelError::Work("work controller lock poisoned".into()))?
            .get(&ticket_id)
            .map(|tracked| tracked.conversation_id))
    }
}
