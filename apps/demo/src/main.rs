use std::{collections::BTreeSet, sync::Arc};

use agent_protocol::{AuthorityEnvelope, LocalAuthority, Ticket};
use async_trait::async_trait;
use capability_broker::{
    BrokerError, CapabilityBroker, CapabilityDescriptor, CapabilityProvider,
    InMemoryTicketAuthorityStore, ProviderScore, SlmTranslator,
};
use model_gateway::LiteLlmClient;
use serde_json::{Value, json};
use uuid::Uuid;
use worker_runtime::{FsProcessWorkbench, WorkerRuntime};

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
        Ok(json!({
            "note": "Demo provider only; replace with Brave/Exa/SearXNG/MCP/etc.",
            "arguments": arguments
        }))
    }
}

#[tokio::main]
async fn main() {
    // Point these clients at a LiteLLM Proxy. Separate models can be used for worker and broker SLM.
    let worker_model = Arc::new(LiteLlmClient::new(
        "http://localhost:4000",
        None,
        "worker-model",
    ));
    let broker_slm = Arc::new(LiteLlmClient::new(
        "http://localhost:4000",
        None,
        "small-tool-model",
    ));

    let translator = Arc::new(SlmTranslator::new(broker_slm));
    // The broker resolves ticket authority from this store. A worker never passes its own
    // permissions in; the dispatcher registers the grant below.
    let authority_store = Arc::new(InMemoryTicketAuthorityStore::default());
    let mut broker = CapabilityBroker::new(translator, Arc::clone(&authority_store));
    broker.register_capability(CapabilityDescriptor {
        name: "web.search".into(),
        description: "Search the public web for candidate sources or pages.".into(),
        argument_schema: json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"]
        }),
    });
    broker.register_provider(Arc::new(DemoSearchProvider));
    let broker = Arc::new(broker);

    // The demo inspects the working tree, so it needs reads and nothing else. Both the
    // workbench ceiling and the ticket below are set accordingly: a demo should not hand a
    // model write and execute authority over the directory it happens to be run from.
    let workspace = std::env::current_dir().expect("current directory");
    let workbench = Arc::new(FsProcessWorkbench::new(workspace, false, false));
    let worker = WorkerRuntime::new(worker_model, broker, workbench);

    let mut capabilities = BTreeSet::new();
    capabilities.insert("web.search".to_string());

    let ticket = Ticket {
        id: Uuid::new_v4(),
        conversation_id: Uuid::new_v4(),
        objective: "Inspect the current workspace and report what this project is.".into(),
        context: vec![],
        constraints: vec!["Do not access anything outside the workspace unless needed.".into()],
        deliverable: "A concise project description.".into(),
        authority: AuthorityEnvelope {
            local: LocalAuthority {
                read_workspace: true,
                write_workspace: false,
                execute_local: false,
            },
            external_capabilities: capabilities,
        },
    };

    // Dispatch: identity is assigned here, and the grant is registered against it before
    // the worker starts. This is the role a durable work controller plays in production.
    let worker_id = Uuid::new_v4();
    authority_store
        .grant(ticket.id, worker_id, ticket.authority.clone())
        .await;

    let ticket_id = ticket.id;
    match worker.run(worker_id, ticket).await {
        Ok(report) => println!("{}", serde_json::to_string_pretty(&report).unwrap()),
        // `Err` now means the runtime could not continue; a failed task arrives as a
        // report with status "failed".
        Err(error) => eprintln!("worker runtime error: {error}"),
    }
    authority_store.revoke(ticket_id).await;
}
