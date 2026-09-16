//! End-to-end Seam walkthrough.
//!
//! Runs one user turn all the way through the architecture and narrates every seam it
//! crosses:
//!
//! ```text
//! client command -> orchestration-api -> kernel -> orchestrator model
//!   -> ticket (authority clamped) -> dispatcher -> worker loop
//!   -> local workbench / capability broker -> worker report
//!   -> kernel -> orchestrator model -> client events
//! ```
//!
//! By default no credentials or network are needed: the three model calls are scripted.
//! Set `SEAM_MODEL_GATEWAY_URL` (plus optional `SEAM_MODEL_GATEWAY_API_KEY`) to run the
//! identical wiring through the demo's chat-completions HTTP adapter. The gateway owns
//! model selection.

mod gateway;
mod support;

use std::sync::Arc;

use async_trait::async_trait;
use capability_broker::{
    BrokerError, CapabilityBroker, CapabilityDescriptor, CapabilityProvider,
    InMemoryTicketAuthorityStore, ProviderScore, SlmTranslator,
};
use orchestration_api::{ClientCommand, InMemoryProjection, OrchestrationApi, ReadModel};
use orchestration_kernel::{OrchestrationKernel, TicketAuthorityPolicy, WorkController};
use serde_json::{Value, json};
use uuid::Uuid;
use worker_runtime::{FsProcessWorkbench, WorkerRuntime};

use gateway::{LiveConfig, Role, gateway_for};
use support::{
    DemoWorkController, InMemoryConversationStore, NarratingTranslator, NarratingWorkbench,
    PrintingSink, RecordingMemory,
};

/// Stand-in for a real search provider. Swapping this for any provider adapter is the
/// only change needed to make the capability real — no worker or prompt changes.
struct DemoSearchProvider;

#[async_trait]
impl CapabilityProvider for DemoSearchProvider {
    fn id(&self) -> &str {
        "demo.search"
    }
    fn capability(&self) -> &str {
        "web.search"
    }
    fn score(&self) -> ProviderScore {
        ProviderScore {
            estimated_cost: 0.0,
            estimated_latency_ms: 5,
            health: 1.0,
        }
    }

    async fn execute(&self, arguments: Value) -> Result<Value, BrokerError> {
        println!("           provider `demo.search` invoked");
        Ok(json!({
            "results": [
                {
                    "title": "Rust 2024 edition — release notes",
                    "snippet": "The 2024 edition stabilises if-let chains and RPIT lifetime capture."
                }
            ],
            "note": "Demo provider; replace with any implementation of the provider port.",
            "echoed_arguments": arguments
        }))
    }
}

fn rule(title: &str) {
    println!("\n{:=<78}", "");
    println!("== {title}");
    println!("{:=<78}", "");
}

#[tokio::main]
async fn main() {
    let live = LiveConfig::from_env();

    rule("Seam end-to-end demo");
    match &live {
        Some(config) => println!(
            "Model mode: LIVE against {}\n\
             The gateway owns model selection and routing.",
            config.base_url
        ),
        None => println!(
            "Model mode: SCRIPTED (no credentials or network needed).\n\
             Every component below is the real one; only the three model calls are canned.\n\
             Set SEAM_MODEL_GATEWAY_URL to use a live gateway."
        ),
    }

    // ---- models, one per role -------------------------------------------------------
    let orchestrator_model = Arc::new(gateway_for(
        Role::Orchestrator,
        live.as_ref(),
        orchestrator_script(),
    ));
    let worker_model = Arc::new(gateway_for(Role::Worker, live.as_ref(), worker_script()));
    let broker_model = Arc::new(gateway_for(Role::BrokerSlm, live.as_ref(), broker_script()));

    // ---- capability broker ----------------------------------------------------------
    let authority_store = Arc::new(InMemoryTicketAuthorityStore::default());
    let mut broker = CapabilityBroker::new(
        Arc::new(NarratingTranslator(SlmTranslator::new(broker_model))),
        Arc::clone(&authority_store),
    );
    broker.register_capability(CapabilityDescriptor {
        name: "web.search".into(),
        description: "Search the public web for candidate sources or pages.".into(),
        argument_schema: json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"],
            "additionalProperties": false
        }),
    });
    broker.register_provider(Arc::new(DemoSearchProvider));
    let broker = Arc::new(broker);
    let telemetry = broker.operational_memory();

    // ---- worker runtime -------------------------------------------------------------
    // Read-only ceiling: this demo inspects a checkout, so the workbench is not given
    // write or execute authority at all, independent of what any ticket asks for.
    let workspace = std::env::current_dir().expect("current directory");
    let workbench = Arc::new(NarratingWorkbench(FsProcessWorkbench::new(
        workspace, false, false,
    )));
    let worker = WorkerRuntime::new(worker_model, Arc::clone(&broker), workbench);

    // ---- orchestration --------------------------------------------------------------
    // The deployment's authority ceiling. The orchestrator may ask for more; it cannot
    // receive more.
    let policy = TicketAuthorityPolicy::read_only().with_capabilities(["web.search"]);
    let work = Arc::new(DemoWorkController::default());
    let memory = Arc::new(RecordingMemory::default());
    let kernel = Arc::new(OrchestrationKernel::new(
        orchestrator_model,
        Arc::clone(&memory),
        Arc::new(InMemoryConversationStore::default()),
        Arc::clone(&work),
        policy,
    ));

    let projection = Arc::new(InMemoryProjection::default());
    let api = OrchestrationApi::new(kernel, Arc::new(PrintingSink::new(Arc::clone(&projection))));

    // ---- 1. a user turn -------------------------------------------------------------
    let conversation_id = Uuid::new_v4();
    let prompt = "What Rust edition does this workspace use, and what did that edition change?";

    rule("1. Client -> orchestration-api");
    println!("  user: {prompt}");
    println!("\n  Events published to the client:");

    if let Err(error) = api
        .execute(ClientCommand::SendMessage {
            conversation_id,
            text: prompt.into(),
        })
        .await
    {
        eprintln!("\n  command failed: {error}");
        if live.is_some() {
            eprintln!("  (check SEAM_MODEL_GATEWAY_URL is reachable and correctly configured)");
        }
        return;
    }

    // ---- 2. dispatch ----------------------------------------------------------------
    let pending = work.take_pending();
    if pending.is_empty() {
        println!("\n  The orchestrator answered without delegating; nothing to dispatch.");
        return;
    }

    for ticket in pending {
        rule("2. Kernel -> dispatcher: ticket");
        println!("  objective:   {}", ticket.objective);
        println!("  deliverable: {}", ticket.deliverable);
        println!(
            "  authority AFTER the kernel's deterministic clamp:\n    \
             local: read={} write={} execute={}\n    external: {:?}",
            ticket.authority.local.read_workspace,
            ticket.authority.local.write_workspace,
            ticket.authority.local.execute_local,
            ticket.authority.external_capabilities
        );
        println!(
            "  (the orchestrator asked for {:?} plus write+execute; the policy narrowed it)",
            requested_capabilities()
        );

        // The dispatcher assigns identity and registers the grant. The worker never
        // passes its own permissions to the broker.
        let worker_id = Uuid::new_v4();
        authority_store
            .grant(ticket.id, worker_id, ticket.authority.clone())
            .await;
        if let Err(error) = work.assign(ticket.id, worker_id).await {
            eprintln!("  dispatch failed: {error}");
            authority_store.revoke(ticket.id).await;
            continue;
        }
        let _ = api.ticket_started(conversation_id, ticket.id).await;

        rule("3. Worker loop");
        println!(
            "  worker {} runs the ticket. Local ops are direct; anything external is brokered.\n",
            &worker_id.to_string()[..8]
        );

        let ticket_id = ticket.id;
        match worker.run(worker_id, ticket).await {
            Ok(report) => {
                println!("  worker finished: {:?}", report.status);
                println!("  summary: {}", report.summary);

                rule("4. Worker report -> kernel -> client");
                if let Err(error) = api.worker_report(report).await {
                    eprintln!("  reporting failed: {error}");
                }
            }
            Err(error) => {
                // A task failure arrives as a report; this branch means the runtime
                // itself could not continue.
                eprintln!("  worker runtime error: {error}");
            }
        }
        authority_store.revoke(ticket_id).await;
    }

    // ---- 5. what each compartment ended up holding ----------------------------------
    rule("5. Compartments after the turn");

    let view = projection
        .conversation(conversation_id)
        .await
        .expect("projection");
    println!("  CLIENT read model (what a UI reloads):");
    for message in &view.messages {
        println!("    {:?}: {}", message.role, message.text);
    }
    for ticket in view.tickets.values() {
        println!(
            "    ticket {} [{:?}] {}",
            &ticket.id.to_string()[..8],
            ticket.state,
            ticket.objective
        );
    }

    println!("\n  ORCHESTRATOR durable memory (admitted by the kernel, not the model):");
    let admitted = memory.admitted();
    if admitted.is_empty() {
        println!("    (nothing proposed)");
    }
    for item in admitted {
        println!("    - {item}");
    }

    println!("\n  BROKER operational memory (provider names live here, not in worker results):");
    for (provider, stats) in telemetry.snapshot().await {
        println!(
            "    {provider}: calls={} failures={} total_latency_ms={}",
            stats.calls, stats.failures, stats.total_latency_ms
        );
    }

    println!("\n  Note: the worker's CapabilityResult carried no provider name, and the");
    println!("  orchestrator never saw the worker's raw observations — only its report.");
    println!();
}

fn requested_capabilities() -> Vec<&'static str> {
    vec!["web.search", "communication.email.send"]
}

/// The orchestrator's two turns: delegate, then synthesise the worker's report.
fn orchestrator_script() -> Vec<String> {
    vec![
        json!({
            "actions": [
                {
                    "type": "respond",
                    "text": "I'll have a worker check the workspace manifest and confirm what that edition changed."
                },
                {
                    "type": "submit",
                    "ticket": {
                        "objective": "Determine the Rust edition this workspace targets and summarise what that edition changed.",
                        "context": [
                            {"label": "workspace", "value": "A Cargo workspace; the edition is set in the root Cargo.toml."}
                        ],
                        "constraints": ["Do not modify any files."],
                        "deliverable": "The edition in use, and a one-line summary of what it changed.",
                        // Deliberately greedy: asks for write, execute and an email
                        // capability it was never granted, to show the clamp working.
                        "authority": {
                            "local": {
                                "read_workspace": true,
                                "write_workspace": true,
                                "execute_local": true
                            },
                            "external_capabilities": ["web.search", "communication.email.send"]
                        }
                    }
                }
            ],
            "memory_proposals": []
        })
        .to_string(),
        json!({
            "actions": [
                {
                    "type": "respond",
                    "text": "This workspace targets the Rust 2024 edition. That edition stabilised if-let chains and changed how return-position impl Trait captures lifetimes."
                }
            ],
            "memory_proposals": [
                "The Seam workspace targets the Rust 2024 edition."
            ]
        })
        .to_string(),
    ]
}

/// The worker's turns: look around, read the manifest, hit an authority boundary, use a
/// brokered capability, then report.
fn worker_script() -> Vec<String> {
    vec![
        json!({"type": "local", "operation": "list_dir", "arguments": {"path": "."}}).to_string(),
        json!({
            "type": "local",
            "operation": "read_file",
            "arguments": {"path": "Cargo.toml", "limit": 400}
        })
        .to_string(),
        // Denied: the ticket's clamped authority does not include writes. The worker sees
        // the refusal as an observation and carries on.
        json!({
            "type": "local",
            "operation": "write_file",
            "arguments": {"path": "notes.txt", "content": "scratch"}
        })
        .to_string(),
        // Natural language across the worker -> broker seam. No capability name, no
        // provider, no arguments — that is the SLM's job on the other side.
        json!({
            "type": "capability",
            "request": "Find out what the Rust 2024 edition changed."
        })
        .to_string(),
        json!({
            "type": "finish",
            "summary": "The workspace sets edition 2024 in the root Cargo.toml. That edition stabilised if-let chains and changed RPIT lifetime capture.",
            "artifacts": []
        })
        .to_string(),
    ]
}

/// The broker SLM's only job: turn that sentence into one validated capability call.
fn broker_script() -> Vec<String> {
    vec![
        json!({
            "capability": "web.search",
            "arguments": {"query": "Rust 2024 edition changes"}
        })
        .to_string(),
    ]
}
