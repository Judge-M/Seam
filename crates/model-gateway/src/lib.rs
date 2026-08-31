use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: String,
    pub content: String,
}

impl ModelMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".into(), content: content.into() }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self { role: "user".into(), content: content.into() }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: content.into() }
    }
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<ModelMessage>,
    pub temperature: f32,
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
        Self {
            http: Client::new(),
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
        let mut builder = self.http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&json!({
                "model": self.model,
                "messages": request.messages,
                "temperature": request.temperature,
                "response_format": { "type": "json_object" }
            }));

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
