//! Prototype implementations of the ports the kernel depends on, plus the dispatcher.
//!
//! These are the pieces a real deployment replaces: a durable conversation store, a
//! memory service, a durable work controller, and a transport that pushes events to
//! clients. They are small on purpose — the point of the demo is that the seams are
//! narrow enough that a throwaway implementation of each is a few lines.

use std::sync::{Arc, Mutex};

use agent_protocol::{ConversationId, Ticket, TicketId};
use async_trait::async_trait;
use model_gateway::ModelMessage;
use orchestration_api::{ApiError, ClientEvent, EventEnvelope, EventSink, InMemoryProjection};
use orchestration_kernel::{
    ActiveWork, ConversationStore, InMemoryWorkController, KernelError, MemoryItem, MemoryService,
    WorkController,
};

/// Conversation history in a `Vec`. A deployment swaps in a durable log.
#[derive(Default)]
pub struct InMemoryConversationStore {
    messages: Mutex<Vec<(ConversationId, ModelMessage)>>,
}

#[async_trait]
impl ConversationStore for InMemoryConversationStore {
    async fn recent(
        &self,
        conversation_id: ConversationId,
        limit: usize,
    ) -> Result<Vec<ModelMessage>, KernelError> {
        let messages = self
            .messages
            .lock()
            .map_err(|_| KernelError::Store("conversation store lock poisoned".into()))?;
        let mut recent: Vec<ModelMessage> = messages
            .iter()
            .filter(|(id, _)| *id == conversation_id)
            .map(|(_, message)| message.clone())
            .collect();
        if recent.len() > limit {
            recent.drain(..recent.len() - limit);
        }
        Ok(recent)
    }

    async fn append(
        &self,
        conversation_id: ConversationId,
        message: ModelMessage,
    ) -> Result<(), KernelError> {
        self.messages
            .lock()
            .map_err(|_| KernelError::Store("conversation store lock poisoned".into()))?
            .push((conversation_id, message));
        Ok(())
    }
}

/// Memory that records what the kernel admits, so the demo can show the admission point
/// without pulling in a real memory service.
#[derive(Default)]
pub struct RecordingMemory {
    admitted: Mutex<Vec<String>>,
}

impl RecordingMemory {
    pub fn admitted(&self) -> Vec<String> {
        self.admitted.lock().map(|m| m.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl MemoryService for RecordingMemory {
    async fn retrieve(&self, _query: &str, _limit: usize) -> Result<Vec<MemoryItem>, KernelError> {
        Ok(Vec::new())
    }

    async fn store(&self, text: &str) -> Result<(), KernelError> {
        self.admitted
            .lock()
            .map_err(|_| KernelError::Memory("memory lock poisoned".into()))?
            .push(text.to_string());
        Ok(())
    }
}

/// The dispatcher seam.
///
/// Wraps the real in-memory work controller and additionally queues the full submitted
/// tickets, because dispatching is this component's job: it assigns worker identity and
/// registers the broker grant. The kernel deliberately cannot do that itself.
#[derive(Default)]
pub struct DemoWorkController {
    inner: InMemoryWorkController,
    pending: Mutex<Vec<Ticket>>,
}

impl DemoWorkController {
    /// Take the tickets queued since the last call, for dispatch.
    pub fn take_pending(&self) -> Vec<Ticket> {
        self.pending
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default()
    }
}

#[async_trait]
impl WorkController for DemoWorkController {
    async fn submit(&self, ticket: Ticket) -> Result<(), KernelError> {
        self.pending
            .lock()
            .map_err(|_| KernelError::Work("dispatch queue lock poisoned".into()))?
            .push(ticket.clone());
        self.inner.submit(ticket).await
    }

    async fn assign(
        &self,
        ticket_id: TicketId,
        worker_id: agent_protocol::WorkerId,
    ) -> Result<(), KernelError> {
        self.inner.assign(ticket_id, worker_id).await
    }

    async fn update(
        &self,
        ticket_id: TicketId,
        context: Vec<agent_protocol::ContextItem>,
    ) -> Result<(), KernelError> {
        self.inner.update(ticket_id, context).await
    }

    async fn cancel(&self, ticket_id: TicketId) -> Result<(), KernelError> {
        self.inner.cancel(ticket_id).await
    }

    async fn accept_report(
        &self,
        report: &agent_protocol::WorkerReport,
    ) -> Result<(), KernelError> {
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

/// Stands in for a UI transport: prints each semantic event, then feeds the read
/// projection a client would reload from.
pub struct PrintingSink {
    projection: Arc<InMemoryProjection>,
}

impl PrintingSink {
    pub fn new(projection: Arc<InMemoryProjection>) -> Self {
        Self { projection }
    }
}

#[async_trait]
impl EventSink for PrintingSink {
    async fn publish(&self, envelope: EventEnvelope) -> Result<(), ApiError> {
        match &envelope.event {
            ClientEvent::MessageCreated { role, text, .. } => {
                println!("  [event] message_created   {role:?}: {text}");
            }
            ClientEvent::TicketCreated { ticket } => {
                println!(
                    "  [event] ticket_created    {} — {}",
                    &ticket.id.to_string()[..8],
                    ticket.objective
                );
            }
            ClientEvent::TicketUpdated { ticket_id, .. } => {
                println!(
                    "  [event] ticket_updated    {}",
                    &ticket_id.to_string()[..8]
                );
            }
            ClientEvent::TicketStateChanged {
                ticket_id, state, ..
            } => {
                println!(
                    "  [event] ticket_state      {} -> {state:?}",
                    &ticket_id.to_string()[..8]
                );
            }
            ClientEvent::ArtifactCreated { artifact, .. } => {
                println!("  [event] artifact_created  {}", artifact.name);
            }
        }
        self.projection.publish(envelope).await
    }
}

// ---------------------------------------------------------------------------------
// Narration
//
// Both of these are plain decorators over Seam's own traits. Nothing in the runtime
// crates knows the demo exists — being able to slot these in is itself a demonstration
// that the seams are real seams.
// ---------------------------------------------------------------------------------

use agent_protocol::{CapabilityInvocation, CapabilityRequest, LocalAuthority};
use capability_broker::{BrokerError, CapabilityDescriptor, CapabilityTranslator};
use serde_json::Value;
use worker_runtime::{LocalWorkbench, WorkerError};

/// Prints every local workbench operation and its outcome.
pub struct NarratingWorkbench<W>(pub W);

#[async_trait]
impl<W: LocalWorkbench> LocalWorkbench for NarratingWorkbench<W> {
    async fn execute(
        &self,
        operation: &str,
        arguments: Value,
        authority: &LocalAuthority,
    ) -> Result<Value, WorkerError> {
        let summary = match operation {
            "read_file" | "write_file" | "list_dir" => arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or(".")
                .to_string(),
            "exec" => arguments
                .get("program")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            _ => String::new(),
        };
        println!("  [local ] {operation} {summary}");

        let result = self.0.execute(operation, arguments, authority).await;
        match &result {
            Ok(value) => println!("           -> ok  {}", describe(value)),
            Err(error @ WorkerError::Denied(_)) => {
                println!("           -> DENIED  {error}");
                println!("              (the ticket's authority does not permit this; the");
                println!("               worker sees the refusal and continues)");
            }
            Err(error) => println!("           -> error  {error}"),
        }
        result
    }
}

/// One-line summary of an operation result, so a file's contents never flood the demo.
fn describe(value: &Value) -> String {
    if let Some(entries) = value.get("entries").and_then(Value::as_array) {
        return format!("{} entries", entries.len());
    }
    if let Some(total) = value.get("total_bytes").and_then(Value::as_u64) {
        return format!(
            "{} of {total} bytes (truncated={})",
            value
                .get("bytes_returned")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            value
                .get("truncated")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        );
    }
    value.to_string().chars().take(60).collect()
}

/// Prints the natural-language request crossing the worker -> broker seam, and the
/// capability the SLM translated it into.
pub struct NarratingTranslator<T>(pub T);

#[async_trait]
impl<T: CapabilityTranslator> CapabilityTranslator for NarratingTranslator<T> {
    async fn translate(
        &self,
        request: &CapabilityRequest,
        candidates: &[CapabilityDescriptor],
    ) -> Result<CapabilityInvocation, BrokerError> {
        println!("  [broker] worker asked, in plain language:");
        println!("           \"{}\"", request.request);
        println!(
            "           shortlist authorised for this ticket: {:?}",
            candidates
                .iter()
                .map(|descriptor| descriptor.name.as_str())
                .collect::<Vec<_>>()
        );

        let invocation = self.0.translate(request, candidates).await;
        match &invocation {
            Ok(invocation) => {
                println!(
                    "           SLM translated -> {} {}",
                    invocation.capability, invocation.arguments
                );
                println!("           (arguments are schema-validated before any provider runs)");
            }
            Err(error) => println!("           translation failed: {error}"),
        }
        invocation
    }
}
