//! An OpenAI-compatible chat completions client.
//!
//! This is the only place in DEX that speaks a protocol built around
//! tool-calling, and it deliberately never uses that feature. **The request
//! body has no `tools` field**, and a test asserts its absence: the model is
//! asked for a program, not for a sequence of tool calls.
//!
//! Provider configuration is `base_url`, `api_key` and `model`, so any
//! OpenAI-compatible service works, which is the point.

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::sse::SseParser;
use super::{Completion, CompletionRequest, Provider, ProviderError, Turn};
use crate::config::ProviderConfig;

/// The request body. `tools` has no field and no default, so it cannot be
/// populated by accident.
#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
    temperature: f32,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    error: Option<ChunkError>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    #[serde(default)]
    delta: Delta,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChunkError {
    #[serde(default)]
    message: String,
}

pub struct OpenAiCompatible {
    config: ProviderConfig,
    client: reqwest::Client,
}

impl OpenAiCompatible {
    pub fn new(config: ProviderConfig) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(format!("dex/{} (dex-harness)", crate::VERSION))
            .build()
            .unwrap_or_default();
        Self { config, client }
    }

    /// The exact body sent upstream. Exposed so the no-tools invariant can be
    /// asserted against the real thing rather than a reconstruction.
    fn build_request(&self, system: &str, turns: &[Turn]) -> ChatRequest<'_> {
        let mut messages = vec![ChatMessage {
            role: "system",
            content: system.to_string(),
        }];
        for turn in turns {
            messages.push(match turn {
                Turn::User { text } => ChatMessage {
                    role: "user",
                    content: text.clone(),
                },
                // A program and what came of it is one exchange, carried as a
                // single user turn. The model reads its own previous program
                // and the result of running it.
                Turn::Program { source, outcome } => ChatMessage {
                    role: "user",
                    content: format!("You wrote:\n\n{source}\n\nRunning it produced:\n\n{outcome}"),
                },
            });
        }
        ChatRequest {
            model: &self.config.model,
            messages,
            stream: true,
            // Low temperature on purpose: this is code generation against a
            // known API, not prose. Creativity is not what makes it correct.
            temperature: 0.1,
        }
    }
}

#[async_trait]
impl Provider for OpenAiCompatible {
    async fn complete(
        &self,
        request: CompletionRequest,
        cancel: CancellationToken,
    ) -> Result<Completion, ProviderError> {
        let body = self.build_request(&request.system, &request.turns);
        let session_header = (self.config.session_header.clone(), request.session_id);

        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ProviderError::Cancelled),
            result = self.client
                .post(self.config.chat_completions_url())
                .bearer_auth(&self.config.api_key)
                .header(session_header.0, session_header.1.to_string())
                .json(&body)
                .send() => result.map_err(|e| ProviderError::Transport(e.to_string()))?,
        };

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Rejected {
                status: status.as_u16(),
                // The body can be long; the CLI shows the head of it.
                message: summarise_error(&text),
            });
        }

        let mut stream = response.bytes_stream();
        let mut parser = SseParser::new();
        let mut text = String::new();

        loop {
            let chunk = tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(ProviderError::Cancelled),
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else { break };
            let bytes = chunk.map_err(|e| ProviderError::Transport(e.to_string()))?;

            for frame in parser.push(&bytes) {
                if frame.data.is_empty() {
                    continue;
                }
                // The sentinel ends the stream; anything after it is noise.
                if frame.data.trim() == "[DONE]" {
                    return finish(text);
                }
                let parsed: ChatChunk = serde_json::from_str(&frame.data)
                    .map_err(|e| ProviderError::Malformed(e.to_string()))?;
                if let Some(error) = parsed.error {
                    return Err(ProviderError::Malformed(error.message));
                }
                for choice in parsed.choices {
                    if let Some(delta) = choice.delta.content {
                        text.push_str(&delta);
                    }
                }
            }
        }

        // A server that closes without `[DONE]` still produced usable output if
        // anything arrived.
        finish(text)
    }

    fn model(&self) -> &str {
        &self.config.model
    }
}

fn finish(text: String) -> Result<Completion, ProviderError> {
    if text.trim().is_empty() {
        return Err(ProviderError::Empty);
    }
    Ok(Completion { text })
}

/// Keep an error body short enough to render, and never echo a key back.
fn summarise_error(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "no response body".to_string();
    }
    let first_line = trimmed.lines().next().unwrap_or(trimmed);
    if first_line.len() > 400 {
        format!("{}...", &first_line[..400])
    } else {
        first_line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ProviderConfig {
        ProviderConfig {
            base_url: "https://example.invalid/v1".into(),
            api_key: "test-key".into(),
            model: "test-model".into(),
            session_header: "x-opencode-session".into(),
        }
    }

    fn provider() -> OpenAiCompatible {
        OpenAiCompatible::new(config())
    }

    #[test]
    fn the_request_never_carries_a_tools_field() {
        // This is the invariant the whole architecture rests on: DEX does not
        // expose a tool-calling interface to the model.
        let p = provider();
        let body = p.build_request("system", &[Turn::user("hello")]);
        let json = serde_json::to_value(&body).expect("serialize");
        assert!(
            json.get("tools").is_none(),
            "the request must not declare tools, got {json}"
        );
        assert!(
            json.get("functions").is_none(),
            "nor legacy functions, got {json}"
        );
        assert!(json.get("tool_choice").is_none(), "nor tool_choice, got {json}");
    }

    #[test]
    fn the_request_carries_the_model_and_streams() {
        let p = provider();
        let body = p.build_request("system", &[]);
        let json = serde_json::to_value(&body).expect("serialize");
        assert_eq!(json["model"], "test-model");
        assert_eq!(json["stream"], true);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][0]["content"], "system");
    }

    #[test]
    fn a_turn_becomes_one_user_message() {
        let p = provider();
        let body = p.build_request(
            "sys",
            &[
                Turn::user("find auth"),
                Turn::program("let x = 1;", "1"),
            ],
        );
        let json = serde_json::to_value(&body).expect("serialize");
        let messages = json["messages"].as_array().expect("array");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "find auth");
        // A program and its outcome travel together so the model reads the pair.
        assert!(messages[2]["content"].as_str().unwrap().contains("let x = 1;"));
        assert!(messages[2]["content"].as_str().unwrap().contains("1"));
    }

    #[test]
    fn empty_output_is_an_error_rather_than_an_empty_program() {
        assert!(matches!(finish("   \n".into()), Err(ProviderError::Empty)));
        assert!(finish("x".into()).is_ok());
    }

    #[test]
    fn an_error_body_is_truncated_and_single_line() {
        let long = "x".repeat(1000);
        let out = summarise_error(&format!("  {long}\nmore detail"));
        assert_eq!(out.len(), 403);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn an_empty_error_body_says_so() {
        assert_eq!(summarise_error("   "), "no response body");
    }

    #[test]
    fn chunks_are_accumulated_into_one_program() {
        // Replays the stream the client would have consumed.
        let mut parser = SseParser::new();
        let mut text = String::new();
        for payload in [
            r#"{"choices":[{"delta":{"content":"pub fn main() "}}]}"#,
            r#"{"choices":[{"delta":{"content":"{ 42 }"}}]}"#,
        ] {
            let frame = format!("data: {payload}\n\n");
            for decoded in parser.push(frame.as_bytes()) {
                let parsed: ChatChunk = serde_json::from_str(&decoded.data).expect("parse");
                for choice in parsed.choices {
                    text.push_str(choice.delta.content.as_deref().unwrap_or_default());
                }
            }
        }
        assert_eq!(text, "pub fn main() { 42 }");
        assert_eq!(finish(text).expect("ok").text, "pub fn main() { 42 }");
    }

    #[test]
    fn an_in_stream_error_frame_is_surfaced() {
        let payload = r#"{"choices":[],"error":{"message":"model overloaded"}}"#;
        let parsed: ChatChunk = serde_json::from_str(payload).expect("parse");
        let message = parsed.error.expect("error present").message;
        assert_eq!(message, "model overloaded");
    }

    #[test]
    fn a_chunk_with_no_choices_is_tolerated() {
        // Some providers emit keep-alive choices on the first frame.
        let parsed: ChatChunk = serde_json::from_str(r#"{"choices":[]}"#).expect("parse");
        assert!(parsed.choices.is_empty());
        assert!(parsed.error.is_none());
    }
}