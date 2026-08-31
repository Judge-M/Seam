use std::{collections::HashMap, sync::Arc, time::Duration};

use agent_protocol::{
    AuthorityEnvelope, CapabilityInvocation, CapabilityRequest, CapabilityResult,
};
use async_trait::async_trait;
use model_gateway::{ModelGateway, ModelMessage, ModelRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::RwLock;

pub mod schema;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityDescriptor {
    pub name: String,
    pub description: String,
    pub argument_schema: Value,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderScore {
    pub estimated_cost: f64,
    pub estimated_latency_ms: u64,
    pub health: f64,
}

impl ProviderScore {
    fn rank(self) -> f64 {
        // Simple deterministic baseline. Replace with policy/config, not model reasoning.
        self.estimated_cost * 1000.0
            + self.estimated_latency_ms as f64 / 1000.0
            + (1.0 - self.health.clamp(0.0, 1.0)) * 100.0
    }
}

#[derive(Debug, Error)]
pub enum BrokerError {
    #[error("capability denied: {0}")]
    Denied(String),
    #[error("no capability matches request")]
    CapabilityGap,
    #[error("no provider is available for {0}")]
    ProviderUnavailable(String),
    #[error("invalid SLM translation: {0}")]
    Translation(String),
    #[error("invalid capability arguments: {0}")]
    InvalidArguments(String),
    #[error("provider failed: {0}")]
    Provider(String),
}

impl BrokerError {
    /// Stable, worker-facing code.
    ///
    /// Provider topology, transport errors and credential-bearing detail stay inside the
    /// broker; the worker gets something actionable that leaks nothing about how the
    /// capability is implemented.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Denied(_) => "CAPABILITY_DENIED",
            Self::CapabilityGap => "CAPABILITY_GAP",
            Self::ProviderUnavailable(_) => "CAPABILITY_TEMPORARILY_UNAVAILABLE",
            Self::Translation(_) => "CAPABILITY_REQUEST_NOT_UNDERSTOOD",
            Self::InvalidArguments(_) => "CAPABILITY_ARGUMENTS_INVALID",
            Self::Provider(_) => "CAPABILITY_TEMPORARILY_UNAVAILABLE",
        }
    }
}

#[async_trait]
pub trait CapabilityTranslator: Send + Sync {
    async fn translate(
        &self,
        request: &CapabilityRequest,
        candidates: &[CapabilityDescriptor],
    ) -> Result<CapabilityInvocation, BrokerError>;
}

/// The SLM sees only the immediate request and capability shortlist.
pub struct SlmTranslator<M> {
    model: Arc<M>,
}

impl<M> SlmTranslator<M> {
    pub fn new(model: Arc<M>) -> Self {
        Self { model }
    }
}

#[async_trait]
impl<M> CapabilityTranslator for SlmTranslator<M>
where
    M: ModelGateway + 'static,
{
    async fn translate(
        &self,
        request: &CapabilityRequest,
        candidates: &[CapabilityDescriptor],
    ) -> Result<CapabilityInvocation, BrokerError> {
        let prompt = serde_json::json!({
            "request": request.request,
            "capabilities": candidates,
        });

        let system = r#"
You are the translation component inside a Capability Broker.
Your only job is to translate the immediate worker request into ONE logical capability invocation.
Do not solve the worker's task. Do not plan. Do not write general-purpose code.
Do not assume user/project context beyond the request.
Return JSON exactly shaped as:
{"capability":"capability.name","arguments":{...}}
Only select from the supplied capability list.
Arguments must satisfy the selected capability's argument_schema.
"#;

        let raw = self
            .model
            .complete(ModelRequest::json(
                vec![
                    ModelMessage::system(system),
                    ModelMessage::user(prompt.to_string()),
                ],
                0.0,
            ))
            .await
            .map_err(|e| BrokerError::Translation(e.to_string()))?;

        serde_json::from_str(&raw).map_err(|e| BrokerError::Translation(e.to_string()))
    }
}

#[async_trait]
pub trait CapabilityProvider: Send + Sync {
    fn id(&self) -> &str;
    fn capability(&self) -> &str;
    fn score(&self) -> ProviderScore;

    async fn execute(&self, arguments: Value) -> Result<Value, BrokerError>;
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderTelemetry {
    pub calls: u64,
    pub failures: u64,
    pub total_latency_ms: u128,
}

#[derive(Default)]
pub struct OperationalMemory {
    inner: RwLock<HashMap<String, ProviderTelemetry>>,
}

impl OperationalMemory {
    pub async fn record(&self, provider: &str, elapsed: Duration, ok: bool) {
        let mut state = self.inner.write().await;
        let entry = state.entry(provider.to_owned()).or_default();
        entry.calls += 1;
        entry.total_latency_ms += elapsed.as_millis();
        if !ok {
            entry.failures += 1;
        }
    }

    pub async fn snapshot(&self) -> HashMap<String, ProviderTelemetry> {
        self.inner.read().await.clone()
    }
}

pub struct CapabilityBroker<T> {
    translator: Arc<T>,
    capabilities: HashMap<String, CapabilityDescriptor>,
    providers: HashMap<String, Vec<Arc<dyn CapabilityProvider>>>,
    operational_memory: Arc<OperationalMemory>,
}

impl<T> CapabilityBroker<T>
where
    T: CapabilityTranslator + 'static,
{
    pub fn new(translator: Arc<T>) -> Self {
        Self {
            translator,
            capabilities: HashMap::new(),
            providers: HashMap::new(),
            operational_memory: Arc::new(OperationalMemory::default()),
        }
    }

    pub fn operational_memory(&self) -> Arc<OperationalMemory> {
        Arc::clone(&self.operational_memory)
    }

    pub fn register_capability(&mut self, descriptor: CapabilityDescriptor) {
        self.capabilities
            .insert(descriptor.name.clone(), descriptor);
    }

    pub fn register_provider(&mut self, provider: Arc<dyn CapabilityProvider>) {
        self.providers
            .entry(provider.capability().to_owned())
            .or_default()
            .push(provider);
    }

    /// Deterministic shortlist before the SLM. This deliberately does not consult user/project memory.
    fn candidates(&self, authority: &AuthorityEnvelope) -> Vec<CapabilityDescriptor> {
        self.capabilities
            .values()
            .filter(|descriptor| authority.external_capabilities.contains(&descriptor.name))
            .cloned()
            .collect()
    }

    pub async fn execute(
        &self,
        request: &CapabilityRequest,
        authority: &AuthorityEnvelope,
    ) -> Result<CapabilityResult, BrokerError> {
        let candidates = self.candidates(authority);
        if candidates.is_empty() {
            return Err(BrokerError::CapabilityGap);
        }

        let invocation = self.translator.translate(request, &candidates).await?;

        if !authority
            .external_capabilities
            .contains(&invocation.capability)
        {
            return Err(BrokerError::Denied(invocation.capability));
        }
        let descriptor = self
            .capabilities
            .get(&invocation.capability)
            .ok_or(BrokerError::CapabilityGap)?;

        // The SLM's output is untrusted structured input. Arguments are checked against the
        // capability's declared schema before any provider sees them.
        schema::validate(&descriptor.argument_schema, &invocation.arguments)
            .map_err(BrokerError::InvalidArguments)?;

        let providers = self
            .providers
            .get(&invocation.capability)
            .filter(|providers| !providers.is_empty())
            .ok_or_else(|| BrokerError::ProviderUnavailable(invocation.capability.clone()))?;

        let provider = providers
            .iter()
            .min_by(|a, b| a.score().rank().total_cmp(&b.score().rank()))
            .ok_or_else(|| BrokerError::ProviderUnavailable(invocation.capability.clone()))?;

        let started = std::time::Instant::now();
        let result = provider.execute(invocation.arguments).await;
        let elapsed = started.elapsed();
        self.operational_memory
            .record(provider.id(), elapsed, result.is_ok())
            .await;

        match result {
            Ok(data) => {
                tracing::info!(
                    capability = %invocation.capability,
                    provider = provider.id(),
                    ticket_id = %request.ticket_id,
                    worker_id = %request.worker_id,
                    latency_ms = elapsed.as_millis(),
                    "capability invocation succeeded"
                );
                Ok(CapabilityResult {
                    capability: invocation.capability,
                    provider: provider.id().to_owned(),
                    data,
                    metadata: serde_json::json!({}),
                })
            }
            Err(error) => {
                // Full detail is recorded here and nowhere above this boundary.
                tracing::warn!(
                    capability = %invocation.capability,
                    provider = provider.id(),
                    ticket_id = %request.ticket_id,
                    worker_id = %request.worker_id,
                    latency_ms = elapsed.as_millis(),
                    error = %error,
                    "capability invocation failed"
                );
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_protocol::LocalAuthority;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    /// Stands in for the SLM: returns a fixed invocation so the deterministic core is
    /// what is under test.
    struct FixedTranslator(CapabilityInvocation);

    #[async_trait]
    impl CapabilityTranslator for FixedTranslator {
        async fn translate(
            &self,
            _request: &CapabilityRequest,
            _candidates: &[CapabilityDescriptor],
        ) -> Result<CapabilityInvocation, BrokerError> {
            Ok(self.0.clone())
        }
    }

    struct RecordingProvider {
        id: &'static str,
        score: ProviderScore,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl CapabilityProvider for RecordingProvider {
        fn id(&self) -> &str {
            self.id
        }
        fn capability(&self) -> &str {
            "web.search"
        }
        fn score(&self) -> ProviderScore {
            self.score
        }
        async fn execute(&self, arguments: Value) -> Result<Value, BrokerError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"provider": self.id, "arguments": arguments}))
        }
    }

    fn search_descriptor() -> CapabilityDescriptor {
        CapabilityDescriptor {
            name: "web.search".into(),
            description: "Search the public web.".into(),
            argument_schema: json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn authority(capabilities: &[&str]) -> AuthorityEnvelope {
        AuthorityEnvelope {
            local: LocalAuthority::default(),
            external_capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn request() -> CapabilityRequest {
        CapabilityRequest {
            worker_id: Uuid::new_v4(),
            ticket_id: Uuid::new_v4(),
            request: "find the seam docs".into(),
        }
    }

    fn broker_with(
        invocation: CapabilityInvocation,
        calls: Arc<AtomicUsize>,
    ) -> CapabilityBroker<FixedTranslator> {
        let mut broker = CapabilityBroker::new(Arc::new(FixedTranslator(invocation)));
        broker.register_capability(search_descriptor());
        broker.register_provider(Arc::new(RecordingProvider {
            id: "demo.search",
            score: ProviderScore {
                estimated_cost: 0.0,
                estimated_latency_ms: 5,
                health: 1.0,
            },
            calls,
        }));
        broker
    }

    #[tokio::test]
    async fn executes_a_valid_authorized_invocation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let broker = broker_with(
            CapabilityInvocation {
                capability: "web.search".into(),
                arguments: json!({"query": "seam"}),
            },
            Arc::clone(&calls),
        );

        let result = broker
            .execute(&request(), &authority(&["web.search"]))
            .await
            .expect("execute");
        assert_eq!(result.provider, "demo.search");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalid_slm_arguments_never_reach_a_provider() {
        let calls = Arc::new(AtomicUsize::new(0));
        let broker = broker_with(
            CapabilityInvocation {
                capability: "web.search".into(),
                // No `query`, plus a property the schema does not permit.
                arguments: json!({"command": "rm -rf /"}),
            },
            Arc::clone(&calls),
        );

        let error = broker
            .execute(&request(), &authority(&["web.search"]))
            .await
            .expect_err("invalid arguments must be refused");
        assert!(matches!(error, BrokerError::InvalidArguments(_)), "{error}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "provider must not be invoked"
        );
    }

    #[tokio::test]
    async fn a_capability_outside_the_ticket_authority_is_denied() {
        let calls = Arc::new(AtomicUsize::new(0));
        // The SLM names a capability the ticket does not carry.
        let broker = broker_with(
            CapabilityInvocation {
                capability: "infrastructure.host.inspect".into(),
                arguments: json!({}),
            },
            Arc::clone(&calls),
        );

        let error = broker
            .execute(&request(), &authority(&["web.search"]))
            .await
            .expect_err("unauthorized capability must be denied");
        assert!(matches!(error, BrokerError::Denied(_)), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_ticket_with_no_capabilities_reports_a_gap() {
        let broker = broker_with(
            CapabilityInvocation {
                capability: "web.search".into(),
                arguments: json!({"query": "seam"}),
            },
            Arc::new(AtomicUsize::new(0)),
        );

        let error = broker
            .execute(&request(), &authority(&[]))
            .await
            .expect_err("no authority means no candidates");
        assert!(matches!(error, BrokerError::CapabilityGap), "{error}");
    }

    #[tokio::test]
    async fn the_cheapest_healthy_provider_is_selected_deterministically() {
        let cheap = Arc::new(AtomicUsize::new(0));
        let expensive = Arc::new(AtomicUsize::new(0));

        let mut broker = CapabilityBroker::new(Arc::new(FixedTranslator(CapabilityInvocation {
            capability: "web.search".into(),
            arguments: json!({"query": "seam"}),
        })));
        broker.register_capability(search_descriptor());
        broker.register_provider(Arc::new(RecordingProvider {
            id: "expensive",
            score: ProviderScore {
                estimated_cost: 1.0,
                estimated_latency_ms: 5,
                health: 1.0,
            },
            calls: Arc::clone(&expensive),
        }));
        broker.register_provider(Arc::new(RecordingProvider {
            id: "cheap",
            score: ProviderScore {
                estimated_cost: 0.0,
                estimated_latency_ms: 5,
                health: 1.0,
            },
            calls: Arc::clone(&cheap),
        }));

        let result = broker
            .execute(&request(), &authority(&["web.search"]))
            .await
            .expect("execute");
        assert_eq!(result.provider, "cheap");
        assert_eq!(cheap.load(Ordering::SeqCst), 1);
        assert_eq!(expensive.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn successful_calls_are_recorded_in_operational_memory() {
        let broker = broker_with(
            CapabilityInvocation {
                capability: "web.search".into(),
                arguments: json!({"query": "seam"}),
            },
            Arc::new(AtomicUsize::new(0)),
        );
        let memory = broker.operational_memory();

        broker
            .execute(&request(), &authority(&["web.search"]))
            .await
            .expect("execute");

        let snapshot = memory.snapshot().await;
        let telemetry = snapshot.get("demo.search").expect("telemetry");
        assert_eq!(telemetry.calls, 1);
        assert_eq!(telemetry.failures, 0);
    }

    #[test]
    fn worker_facing_codes_do_not_leak_provider_detail() {
        let error = BrokerError::Provider("github-mcp socket error: 503 at 10.0.0.4".into());
        assert_eq!(error.code(), "CAPABILITY_TEMPORARILY_UNAVAILABLE");
        assert!(!error.code().contains("10.0.0.4"));
    }
}
