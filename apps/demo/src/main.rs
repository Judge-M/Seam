use std::{collections::BTreeSet, sync::Arc};

use agent_protocol::{AuthorityEnvelope, LocalAuthority, Ticket};
use async_trait::async_trait;
use capability_broker::{BrokerError, CapabilityBroker, CapabilityDescriptor, CapabilityProvider, ProviderScore, SlmTranslator};
use model_gateway::LiteLlmClient;
use serde_json::{Value, json};
use uuid::Uuid;
use worker_runtime::{FsProcessWorkbench, WorkerRuntime};

struct DemoSearchProvider;

#[async_trait]
impl CapabilityProvider for DemoSearchProvider {
    fn id(&self) -> &str { "demo.search" }
    fn capability(&self) -> &str { "web.search" }
    fn score(&self) -> ProviderScore {
        ProviderScore { estimated_cost: 0.0, estimated_latency_ms: 5, health: 1.0 }
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
    let mut broker = CapabilityBroker::new(translator);
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

    let workspace = std::env::current_dir().expect("current directory");
    let workbench = Arc::new(FsProcessWorkbench::new(workspace, true, true));
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
                write_workspace: true,
                execute_local: true,
            },
            external_capabilities: capabilities,
        },
    };

    match worker.run(ticket).await {
        Ok(report) => println!("{}", serde_json::to_string_pretty(&report).unwrap()),
        Err(error) => eprintln!("worker failed: {error}"),
    }
}
