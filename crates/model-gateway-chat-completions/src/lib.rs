use std::time::Duration;

use async_trait::async_trait;
use model_gateway::{ModelError, ModelGateway, ModelOutput, ModelRequest};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Map, Value, json};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Adapter for gateways that accept the chat-completions JSON request shape while owning
/// model selection and routing themselves.
///
/// This is one replaceable implementation of [`ModelGateway`], not Seam's model
/// architecture. It deliberately sends no model identifier. Other serving protocols
/// belong in sibling adapter crates.
#[derive(Clone)]
pub struct ChatCompletionsClient {
    http: Client,
    base_url: String,
    api_key: Option<String>,
}

impl ChatCompletionsClient {
    pub fn new(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self::with_timeout(base_url, api_key, DEFAULT_TIMEOUT)
    }

    pub fn with_timeout(
        base_url: impl Into<String>,
        api_key: Option<String>,
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
        }
    }

    fn payload(&self, request: &ModelRequest) -> Value {
        let mut payload = Map::new();
        payload.insert("messages".into(), json!(request.messages));
        payload.insert("temperature".into(), json!(request.temperature));
        if request.output == ModelOutput::JsonObject {
            payload.insert("response_format".into(), json!({"type": "json_object"}));
        }
        if let Some(max_output_tokens) = request.max_output_tokens {
            // `max_tokens` is a field of this adapter's wire protocol, not the shared
            // ModelGateway contract.
            payload.insert("max_tokens".into(), json!(max_output_tokens));
        }
        Value::Object(payload)
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
impl ModelGateway for ChatCompletionsClient {
    async fn complete(&self, request: ModelRequest) -> Result<String, ModelError> {
        let mut builder = self
            .http
            .post(format!("{}/v1/chat/completions", self.base_url))
            .json(&self.payload(&request));

        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }

        let response = builder
            .send()
            .await
            .map_err(|error| ModelError::Transport(error.to_string()))?
            .error_for_status()
            .map_err(|error| ModelError::Transport(error.to_string()))?;
        let body: ChatResponse = response
            .json()
            .await
            .map_err(|error| ModelError::InvalidResponse(error.to_string()))?;
        body.choices
            .into_iter()
            .next()
            .and_then(|choice| choice.message.content)
            .ok_or(ModelError::MissingOutput)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model_gateway::ModelMessage;

    #[test]
    fn adapter_maps_output_contract_without_selecting_a_model() {
        let client = ChatCompletionsClient::new("http://localhost:1234", None);
        let request =
            ModelRequest::json(vec![ModelMessage::user("work")], 0.1).with_max_output_tokens(512);

        let payload = client.payload(&request);

        assert!(payload.get("model").is_none());
        assert_eq!(payload["messages"][0]["role"], "user");
        assert_eq!(payload["response_format"]["type"], "json_object");
        assert_eq!(payload["max_tokens"], 512);
    }

    #[test]
    fn text_requests_do_not_gain_adapter_specific_json_constraints() {
        let client = ChatCompletionsClient::new("http://localhost:1234", None);
        let payload = client.payload(&ModelRequest::text(vec![ModelMessage::user("work")], 0.1));

        assert!(payload.get("response_format").is_none());
        assert!(payload.get("max_tokens").is_none());
    }
}
