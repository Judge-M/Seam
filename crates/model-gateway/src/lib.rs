use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: String,
    pub content: String,
}

impl ModelMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<ModelMessage>,
    pub temperature: f32,
    /// Ask the provider to constrain the reply to a JSON object.
    ///
    /// Seam's internal control surfaces (orchestrator envelope, worker action, broker
    /// translation) are JSON, but the gateway is a general seam: prose replies must stay
    /// possible, so this is per request rather than baked into the client.
    pub json_mode: bool,
    pub max_tokens: Option<u32>,
}

impl ModelRequest {
    /// Request a JSON object reply. Used by Seam's structured control surfaces.
    pub fn json(messages: Vec<ModelMessage>, temperature: f32) -> Self {
        Self {
            messages,
            temperature,
            json_mode: true,
            max_tokens: None,
        }
    }

    /// Request an unconstrained reply.
    pub fn text(messages: Vec<ModelMessage>, temperature: f32) -> Self {
        Self {
            messages,
            temperature,
            json_mode: false,
            max_tokens: None,
        }
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }
}

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("model transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("model returned no text")]
    MissingText,
    #[error("model error: {0}")]
    Other(String),
}

#[async_trait]
pub trait ModelGateway: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> Result<String, ModelError>;
}

/// Thin OpenAI-compatible client intended to point at LiteLLM Proxy.
#[derive(Clone)]
pub struct LiteLlmClient {
    http: Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl LiteLlmClient {
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        Self::with_timeout(base_url, api_key, model, DEFAULT_TIMEOUT)
    }

    /// A gateway call with no timeout can hang a worker step indefinitely, so the
    /// timeout is part of constructing the client rather than an optional extra.
    pub fn with_timeout(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
        timeout: Duration,
    ) -> Self {
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            model: model.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
}

#[async_trait]
impl ModelGateway for LiteLlmClient {
    async fn complete(&self, request: ModelRequest) -> Result<String, ModelError> {
        let mut payload = Map::new();
        payload.insert("model".into(), json!(self.model));
        payload.insert("messages".into(), json!(request.messages));
        payload.insert("temperature".into(), json!(request.temperature));
        if request.json_mode {
            payload.insert("response_format".into(), json!({"type": "json_object"}));
        }
        if let Some(max_tokens) = request.max_tokens {
            payload.insert("max_tokens".into(), json!(max_tokens));
        }

        let mut builder = self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&Value::Object(payload));

        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }

        let response = builder.send().await?.error_for_status()?;
        let body: ChatResponse = response.json().await?;
        body.choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .ok_or(ModelError::MissingText)
    }
}
