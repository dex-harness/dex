//! The agent loop.
//!
//! This is the part the whole architecture exists for: the model does not call
//! tools, it writes a program, and the runtime runs it. A turn is therefore a
//! loop of *programs*, not of tool calls, and the model keeps writing until it
//! answers.
//!
//! ```text
//! user
//!   ↓
//! model → program  ──▶ runtime ──▶ capabilities ──▶ result
//!   ↑                                                   │
//!   └────────────── model sees the result ──────────────┘
//! ```
//!
//! The loop stops when the program calls `dex::respond`, when the model runs out
//! of rounds, or when the user cancels. None of those end the session: the
//! runtime is left ready for the next message.

use std::sync::Arc;

use dex_protocol::{CallId, Event, SessionStatus};

use super::extract;
use super::Session;
use crate::auth::Authority;
use crate::budget::ExecutionBudget;
use crate::memory::MemoryStore;
use crate::provider::{CompletionRequest, Provider, Turn};
use crate::script::{ScriptError, ScriptRuntime, UiHandle};

/// Drives programs for a session.
pub struct Agent {
    pub provider: Arc<dyn Provider>,
    pub script: Arc<dyn ScriptRuntime>,
    pub memory: Arc<MemoryStore>,
    pub authority: Arc<Authority>,
    pub system: String,
    pub max_rounds: u32,
    pub limits: ExecutionBudget,
}

impl Agent {
    /// Run one user turn.
    pub async fn run(&self, session: &Arc<Session>) -> SessionStatus {
        let mut round = 0u32;
        // Programs already run this turn, and what they produced. A model stuck
        // in a loop would otherwise spend every remaining round re-running the
        // same code and getting the same failure.
        let mut attempted: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        loop {
            if session.cancel.is_cancelled() {
                session.events.emit(Event::SessionFinished {
                    status: SessionStatus::Cancelled,
                });
                return SessionStatus::Cancelled;
            }

            if round >= self.max_rounds {
                // Running out of rounds is not a failure. The work done so far
                // is real; what is missing is the model's summary of it.
                session.events.emit(Event::Answer {
                    text: format!(
                        "Stopped after {round} programs without a final answer. \
                         The work completed so far is described above."
                    ),
                });
                return SessionStatus::Completed;
            }

            let Some(outcome) = self.one_round(session, round, &mut attempted).await else {
                return SessionStatus::Failed;
            };
            round += 1;

            match outcome {
                Round::Answered => return SessionStatus::Completed,
                Round::Failed => return SessionStatus::Failed,
                Round::Continue => {}
            }
        }
    }

    /// One model request, one program, one execution.
    ///
    /// `None` means the turn is over and cannot continue.
    async fn one_round(
        &self,
        session: &Arc<Session>,
        round: u32,
        attempted: &mut std::collections::HashMap<String, String>,
    ) -> Option<Round> {
        // A fresh UI handle per program: its answer slot belongs to this run.
        let ui = UiHandle::new(session.ui_tx.clone());

        session.events.emit(Event::ModelStarted);
        let request = CompletionRequest {
            system: self.system.clone(),
            turns: session.conversation(),
            session_id: session.id,
        };

        let completion = match self.provider.complete(request, session.cancel.clone()).await {
            Ok(completion) => completion,
            // A cancelled request ends the turn quietly rather than as a
            // failure the user did not cause.
            Err(crate::provider::ProviderError::Cancelled) => {
                tracing::debug!("model request cancelled");
                return None;
            }
            Err(e) => {
                session.events.emit(Event::Error {
                    error: dex_protocol::ErrorPayload::new(dex_protocol::ErrorKind::Model, e.to_string()),
                });
                return Some(Round::Failed);
            }
        };

        let Some((program, _how)) = extract::extract(&completion.text) else {
            session.events.emit(Event::Error {
                error: dex_protocol::ErrorPayload::new(
                    dex_protocol::ErrorKind::Model,
                    "the model did not produce a program",
                ),
            });
            return Some(Round::Failed);
        };

        // Running the same program again would produce the same result, so say
        // so plainly instead of spending a round on it. Two repeats ends the
        // turn: the model is not making progress.
        if let Some(previous) = attempted.get(&program) {
            let repeats = attempted.len();
            session.push_turn(Turn::program(
                program.clone(),
                format!(
                    "You already ran this exact program and it produced:\n\n{previous}\n\n\
                     It will produce the same thing again. Write a different program, or \
                     answer with what you have."
                ),
            ));
            if repeats >= 2 {
                session.events.emit(Event::Answer {
                    text: format!(
                        "Stopped: the same program was attempted {repeats} times with no \
                         progress. The last result is shown above."
                    ),
                });
                return Some(Round::Answered);
            }
            return Some(Round::Continue);
        }

        let call_id = CallId(round as u64 + 1);
        session.events.emit(Event::ProgramStarted {
            call_id,
            round,
            source: program.clone(),
        });

        let outcome = self
            .script
            .execute(&program, session.context(round, ui.clone()))
            .await;

        let outcome = match outcome {
            Ok(result) => {
                let rendered = render_result(&result.value);
                session.events.emit(Event::ProgramFinished {
                    call_id,
                    duration_ms: result.duration_ms,
                });
                if let Some(response) = ui.take_response() {
                    session.events.emit(Event::Answer { text: response.clone() });
                    session.push_turn(Turn::program(program, format!("{rendered}\n\nYou answered: {response}")));
                    return Some(Round::Answered);
                }
                rendered
            }
            Err(e) => {
                let message = describe_error(&e);
                session.events.emit(Event::ProgramFailed {
                    call_id,
                    error: e.to_payload(),
                    diagnostics: diagnostics_of(&e),
                });
                message
            }
        };

        attempted.insert(program.clone(), outcome.clone());
        // The model reads its own program and what running it produced. That
        // pair is the only thing carried forward, which is what keeps a long
        // turn from filling the context with intermediate chatter.
        session.push_turn(Turn::program(program, outcome));
        Some(Round::Continue)
    }
}

/// What one program achieved.
enum Round {
    /// The program answered, and the turn is over.
    Answered,
    /// The turn cannot continue.
    Failed,
    /// More work to do.
    Continue,
}

/// Render a program's value for the model.
///
/// JSON, indented, because the model is reading it to decide the next step and
/// structure carries meaning.
fn render_result(value: &serde_json::Value) -> String {
    match serde_json::to_string_pretty(value) {
        Ok(text) => text,
        Err(_) => value.to_string(),
    }
}

fn describe_error(error: &ScriptError) -> String {
    match error {
        // The compiler's own report is far more useful to a model than a
        // summary, so it is passed through whole when there is one.
        ScriptError::Compile {
            diagnostics: Some(diagnostics),
            ..
        } => format!("The program did not compile. Fix it and try again.\n\n{diagnostics}"),
        ScriptError::Compile { message, .. } => {
            format!("The program did not compile: {message}")
        }
        ScriptError::Raised { message } => format!(
            "The program ran but did not finish: {message}\n\n\
             A capability that is refused ends the program. Do not retry the \
             same call; do something else."
        ),
        ScriptError::Timeout(limit) => format!(
            "The program exceeded its {}ms budget. Do less work per program.",
            limit.as_millis()
        ),
        ScriptError::Cancelled => "The program was cancelled.".to_string(),
        ScriptError::Capability { kind, message } => {
            format!("The program was refused: {kind}. {message}")
        }
        #[allow(unreachable_patterns)]
        ScriptError::Unavailable(message) => format!("The runtime could not run it: {message}"),
    }
}

fn diagnostics_of(error: &ScriptError) -> Option<String> {
    match error {
        ScriptError::Compile { diagnostics, .. } => diagnostics.clone(),
        _ => None,
    }
}
