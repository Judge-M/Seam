use std::{collections::HashMap, sync::Arc, time::Duration};

use agent_protocol::{AuthorityEnvelope, CapabilityInvocation, CapabilityRequest, CapabilityResult};
use async_trait::async_trait;
use model_gateway::{ModelGateway, ModelMessage, ModelRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::RwLock;

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
    #[error("provider failed: {0}")]
    Provider(String),
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
    pub fn new(model: Arc<M>) -> Self { Self { model } }
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
"#;

        let raw = self.model.complete(ModelRequest {
            messages: vec![
                ModelMessage::system(system),
                ModelMessage::user(prompt.to_string()),
            ],
            temperature: 0.0,
        }).await.map_err(|e| BrokerError::Translation(e.to_string()))?;

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
        if !ok { entry.failures += 1; }
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
        self.capabilities.insert(descriptor.name.clone(), descriptor);
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

        if !authority.external_capabilities.contains(&invocation.capability) {
            return Err(BrokerError::Denied(invocation.capability));
        }
        if !self.capabilities.contains_key(&invocation.capability) {
            return Err(BrokerError::CapabilityGap);
        }

        let providers = self.providers.get(&invocation.capability)
            .ok_or_else(|| BrokerError::ProviderUnavailable(invocation.capability.clone()))?;

        let provider = providers.iter()
            .min_by(|a, b| a.score().rank().total_cmp(&b.score().rank()))
            .ok_or_else(|| BrokerError::ProviderUnavailable(invocation.capability.clone()))?;

        let started = std::time::Instant::now();
        let result = provider.execute(invocation.arguments).await;
        self.operational_memory
            .record(provider.id(), started.elapsed(), result.is_ok())
            .await;

        Ok(CapabilityResult {
            capability: invocation.capability,
            provider: provider.id().to_owned(),
            data: result?,
            metadata: serde_json::json!({}),
        })
    }
}
