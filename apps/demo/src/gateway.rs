//! Model wiring for the demo.
//!
//! The demo runs in one of two modes. With `SEAM_MODEL_GATEWAY_URL` set it uses the
//! included chat-completions HTTP adapter. Without it, a scripted gateway replays a fixed
//! conversation so the whole architecture can be run end to end with no credentials and
//! no network. This adapter is a demo composition choice, not a core dependency.
//!
//! The scripted mode is deliberately a *model* stand-in, not a Seam stand-in: every other
//! component below is the real one, so what you see is the real kernel, broker, policy
//! and worker loop executing.

use std::sync::Mutex;

use async_trait::async_trait;
use model_gateway::{ModelError, ModelGateway, ModelRequest};
use model_gateway_chat_completions::ChatCompletionsClient;

/// Which model a request is going to, so the demo can narrate the seam being crossed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Orchestrator,
    Worker,
    BrokerSlm,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Role::Orchestrator => "orchestrator (frontier)",
            Role::Worker => "worker",
            Role::BrokerSlm => "broker SLM",
        }
    }
}

/// Replays a fixed script of replies, then repeats the last one.
pub struct ScriptedGateway {
    role: Role,
    replies: Mutex<Vec<String>>,
}

impl ScriptedGateway {
    pub fn new(role: Role, replies: Vec<String>) -> Self {
        Self {
            role,
            replies: Mutex::new(replies.into_iter().rev().collect()),
        }
    }
}

#[async_trait]
impl ModelGateway for ScriptedGateway {
    async fn complete(&self, _request: ModelRequest) -> Result<String, ModelError> {
        let mut replies = self.replies.lock().map_err(|_| {
            ModelError::Other(format!("{} script lock poisoned", self.role.label()))
        })?;
        if replies.len() > 1 {
            Ok(replies.pop().unwrap_or_default())
        } else {
            replies
                .last()
                .cloned()
                .ok_or_else(|| ModelError::Other(format!("{} script empty", self.role.label())))
        }
    }
}

/// One gateway type so the rest of the wiring is identical in both modes.
pub enum DemoGateway {
    Scripted(ScriptedGateway),
    Live(ChatCompletionsClient),
}

#[async_trait]
impl ModelGateway for DemoGateway {
    async fn complete(&self, request: ModelRequest) -> Result<String, ModelError> {
        match self {
            DemoGateway::Scripted(gateway) => gateway.complete(request).await,
            DemoGateway::Live(client) => client.complete(request).await,
        }
    }
}

/// Live endpoint configuration, if the environment supplies one.
pub struct LiveConfig {
    pub base_url: String,
    pub api_key: Option<String>,
}

impl LiveConfig {
    pub fn from_env() -> Option<Self> {
        let base_url = std::env::var("SEAM_MODEL_GATEWAY_URL").ok()?;
        Some(Self {
            base_url,
            api_key: std::env::var("SEAM_MODEL_GATEWAY_API_KEY").ok(),
        })
    }
}

/// Build a gateway for one role: live when configured, scripted otherwise.
pub fn gateway_for(role: Role, live: Option<&LiveConfig>, script: Vec<String>) -> DemoGateway {
    match live {
        Some(config) => DemoGateway::Live(ChatCompletionsClient::new(
            config.base_url.clone(),
            config.api_key.clone(),
        )),
        None => DemoGateway::Scripted(ScriptedGateway::new(role, script)),
    }
}
