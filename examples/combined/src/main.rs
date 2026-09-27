//! Optional integration; neither implementation repository depends on the other.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use seam_engine::{
    AuthorityContext, Capability, Gateway, GatewayDecisionEngine, GatewayError, GatewayRequest,
    GatewayScope, InMemoryAuthoritySource, IntentRequest, ReturnAmbiguity, SeamEngine, Subject,
};
use seam_sdk::{
    ExternalError, ExternalRequest, ExternalResult, ExternalRuntime, FsProcessWorkbench, Reasoner,
    ReasoningError, ReasoningRequest, Ticket, WorkerAction, WorkerRuntime,
};
use serde_json::{Value, json};
use uuid::Uuid;

struct FixtureUnifiedGateway;

#[async_trait]
impl Gateway for FixtureUnifiedGateway {
    async fn execute(&self, request: GatewayRequest) -> Result<Value, GatewayError> {
        match request.scope {
            GatewayScope::Control => {
                let candidate = if request.intent.starts_with("Choose the next worker action") {
                    "inference"
                } else {
                    "development"
                };
                Ok(json!({"form":"choice", "candidate":candidate, "confidence":0.98}))
            }
            GatewayScope::Caller if request.capability == "inference.code.reason" => {
                if request.arguments["recent_observations"]
                    .as_array()
                    .is_some_and(|items| !items.is_empty())
                {
                    Ok(
                        json!({"type":"finish", "summary":"Fixture issue inspected", "artifacts":[]}),
                    )
                } else {
                    Ok(json!({"type":"external", "intent":"Find the fixture issue"}))
                }
            }
            GatewayScope::Caller => Ok(json!({"issues":["fixture-1"]})),
        }
    }
}

type IntegratedEngine = SeamEngine<
    GatewayDecisionEngine<FixtureUnifiedGateway>,
    InMemoryAuthoritySource,
    FixtureUnifiedGateway,
    ReturnAmbiguity,
>;

struct EngineAdapter {
    engine: Arc<IntegratedEngine>,
}

fn subject(worker_id: Uuid) -> Subject {
    Subject {
        id: worker_id.to_string(),
        kind: "seam-worker".into(),
    }
}

#[async_trait]
impl Reasoner for EngineAdapter {
    async fn next_action(&self, request: ReasoningRequest) -> Result<WorkerAction, ReasoningError> {
        let result = self
            .engine
            .execute(IntentRequest {
                subject: subject(request.worker_id),
                grant_id: request.ticket.id.to_string(),
                intent: "Choose the next worker action for this ticket".into(),
                arguments: json!({
                    "ticket": request.ticket,
                    "recent_observations": request.recent_observations,
                }),
            })
            .await
            .map_err(|_| ReasoningError::Unavailable)?;
        serde_json::from_value(result.data)
            .map_err(|error| ReasoningError::InvalidAction(error.to_string()))
    }
}

#[async_trait]
impl ExternalRuntime for EngineAdapter {
    async fn invoke(&self, request: ExternalRequest) -> Result<ExternalResult, ExternalError> {
        let result = self
            .engine
            .execute(IntentRequest {
                subject: subject(request.worker_id),
                grant_id: request.ticket_id.to_string(),
                intent: request.intent,
                arguments: json!({}),
            })
            .await
            .map_err(|error| match error {
                seam_engine::EngineError::Denied => ExternalError::Denied,
                seam_engine::EngineError::Gap | seam_engine::EngineError::Ambiguous => {
                    ExternalError::Ambiguous
                }
                seam_engine::EngineError::InvalidArguments
                | seam_engine::EngineError::InvalidDecision => ExternalError::Invalid,
                seam_engine::EngineError::Unavailable => ExternalError::Unavailable,
            })?;
        Ok(ExternalResult { data: result.data })
    }
}

#[tokio::main]
async fn main() {
    let worker_id = Uuid::new_v4();
    let ticket = Ticket {
        id: Uuid::new_v4(),
        objective: "Inspect an issue".into(),
        context: vec![],
        constraints: vec![],
        deliverable: "summary".into(),
        local_authority: Default::default(),
    };
    let authority = Arc::new(InMemoryAuthoritySource::default());
    authority
        .grant(
            subject(worker_id),
            AuthorityContext {
                grant_id: ticket.id.to_string(),
                allowed_capabilities: BTreeSet::from([
                    "inference.code.reason".into(),
                    "development.issue.search".into(),
                ]),
                expires_at_epoch_seconds: None,
            },
        )
        .await;
    let gateway = Arc::new(FixtureUnifiedGateway);
    let mut engine = SeamEngine::new(
        Arc::new(GatewayDecisionEngine::new(gateway.clone())),
        authority,
        gateway,
        Arc::new(ReturnAmbiguity),
    );
    for name in ["inference.code.reason", "development.issue.search"] {
        engine
            .register_capability(Capability {
                name: name.into(),
                description: name.into(),
                argument_schema: json!({"type":"object"}),
            })
            .expect("catalog");
    }
    let adapter = Arc::new(EngineAdapter {
        engine: Arc::new(engine),
    });
    let worker = WorkerRuntime::new(
        adapter.clone(),
        adapter,
        Arc::new(FsProcessWorkbench::new(".", false, false)),
    );
    let report = worker.run(worker_id, ticket).await.expect("worker runtime");
    println!("{}", report.summary);
}
