use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Roles in Seam's provider-neutral conversation contract.
///
/// Serving adapters translate these roles into their transport's representation. The
/// shared contract deliberately carries no vendor SDK or wire-protocol types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelRole {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: ModelRole,
    pub content: String,
}

impl ModelMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::System,
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ModelRole::Assistant,
            content: content.into(),
        }
    }
}

/// Semantic output contract requested by a Seam component.
///
/// An adapter may implement this with a native schema feature, constrained decoding,
/// prompting, or another mechanism. Callers do not select the provider mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelOutput {
    Text,
    JsonObject,
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub messages: Vec<ModelMessage>,
    pub temperature: f32,
    pub output: ModelOutput,
    pub max_output_tokens: Option<u32>,
}

impl ModelRequest {
    /// Request one JSON object. Used by Seam's structured control surfaces.
    pub fn json(messages: Vec<ModelMessage>, temperature: f32) -> Self {
        Self {
            messages,
            temperature,
            output: ModelOutput::JsonObject,
            max_output_tokens: None,
        }
    }

    /// Request unconstrained text.
    pub fn text(messages: Vec<ModelMessage>, temperature: f32) -> Self {
        Self {
            messages,
            temperature,
            output: ModelOutput::Text,
            max_output_tokens: None,
        }
    }

    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }
}

/// Provider- and transport-neutral failures crossing the model seam.
///
/// Adapters retain their SDK/HTTP/process error types internally and map them into this
/// bounded surface. This keeps every Seam component independent of the selected serving
/// mechanism.
#[derive(Debug, Error)]
pub enum ModelError {
    #[error("model transport error: {0}")]
    Transport(String),
    #[error("model returned an invalid response: {0}")]
    InvalidResponse(String),
    #[error("model returned no output")]
    MissingOutput,
    #[error("model error: {0}")]
    Other(String),
}

/// The only model-access contract visible to Seam's core components.
///
/// Implementations may call a remote service, a local server, an in-process model, a
/// gateway/proxy, or a deterministic test fixture.
#[async_trait]
pub trait ModelGateway: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> Result<String, ModelError>;
}
