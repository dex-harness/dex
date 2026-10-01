//! The model provider.
//!
//! One abstraction, one implementation: an OpenAI-compatible chat completions
//! endpoint. Keeping the surface this small is deliberate — the provider is the
//! one part of DEX that is allowed to speak a protocol full of tool-calling
//! machinery, and it is never used that way. DEX does not expose a tool-calling
//! interface to the model at all; the model writes a program instead.

pub mod openai_compat;
pub mod prompt;
pub mod sse;

use async_trait::async_trait;
use dex_protocol::SessionId;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// One turn in the conversation, from DEX's point of view.
///
/// Deliberately not the provider's message type: a turn is either something the
/// user said, a program the model produced, or an observation the runtime
/// produced from running one. Translating to whatever the provider calls a
/// message happens in [`openai_compat`], and nowhere else.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Turn {
    /// The human.
    User { text: String },
    /// A program the model produced, and anything the runtime said about it.
    Program {
        source: String,
        /// The program's return value, or the reason it failed.
        outcome: String,
    },
}

impl Turn {
    pub fn user(text: impl Into<String>) -> Self {
        Turn::User { text: text.into() }
    }

    pub fn program(source: impl Into<String>, outcome: impl Into<String>) -> Self {
        Turn::Program {
            source: source.into(),
            outcome: outcome.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct CompletionRequest {
    pub system: String,
    pub turns: Vec<Turn>,
    /// Stable across the conversation, so a gateway can route and cache per
    /// session rather than per request.
    pub session_id: SessionId,
}

#[derive(Clone, Debug)]
pub struct Completion {
    /// The generated program source.
    pub text: String,
}

/// Why a provider call failed. Kept distinct from capability and script errors
/// because the remedy is different: a model failure is reported to the user, not
/// handed back to the model as something to correct.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("could not reach the provider: {0}")]
    Transport(String),
    #[error("the provider rejected the request ({status}): {message}")]
    Rejected { status: u16, message: String },
    #[error("the provider's response could not be read: {0}")]
    Malformed(String),
    #[error("the provider produced no output")]
    Empty,
    #[error("the request was cancelled")]
    Cancelled,
}

/// Something that can produce a program's source from a conversation.
#[async_trait]
pub trait Provider: Send + Sync {
    async fn complete(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<Completion, ProviderError>;

    /// Identifies the configured model, for the session banner.
    fn model(&self) -> &str;
}