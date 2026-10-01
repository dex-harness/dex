//! Sessions.
//!
//! A session owns a conversation and the running of programs on its behalf. The
//! manager owns the sessions, so a connection is only ever a subscriber: a
//! frontend that disconnects drops a receiver and nothing else, and the session
//! keeps running. That is what makes "a client disconnect must not corrupt the
//! session" true by construction rather than by careful sequencing.

pub mod agent;
pub mod extract;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use dex_protocol::{Ack, SessionId, SessionStatus};
use tokio::sync::Mutex;

use crate::auth::Authority;
use crate::budget::ExecutionBudget;
use crate::config::RuntimeConfig;
use crate::events::EventSink;
use crate::memory::MemoryStore;
use crate::provider::{Provider, Turn};
use crate::script::{ScriptRuntime, UiEvent, UiHandle};
use tokio_util::sync::CancellationToken;

use self::agent::Agent;

/// Configuration a session is created with.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub working_dir: PathBuf,
    /// Overrides the runtime default when set.
    pub model: Option<String>,
}

/// Everything a session needs that is shared across sessions.
#[derive(Clone)]
pub struct SessionDeps {
    pub config: RuntimeConfig,
    pub provider: Arc<dyn Provider>,
    pub script: Arc<dyn ScriptRuntime>,
    pub memory: Arc<MemoryStore>,
}

impl SessionDeps {
    fn limits(&self) -> ExecutionBudget {
        self.config.budget
    }
}

/// One conversation, and the agent that drives it.
pub struct Session {
    pub id: SessionId,
    pub working_dir: PathBuf,
    pub model: String,
    /// Interior mutable: a session is shared between the agent task and every
    /// subscriber, so its status cannot be a plain field.
    status: std::sync::Mutex<SessionStatus>,
    pub events: EventSink,
    /// The cancel token for the turn in flight. Cancelling it stops the model
    /// request and the program it started.
    pub cancel: CancellationToken,
    /// The conversation so far. Held behind a lock because the agent task and
    /// an introspection request can both reach it.
    conversation: Arc<std::sync::Mutex<Vec<Turn>>>,
    /// The channel the agent uses to deliver answers and prompts.
    pub ui_tx: tokio::sync::mpsc::UnboundedSender<UiEvent>,
    /// A turn already in flight. Two turns at once would interleave their
    /// programs into one conversation.
    running: Arc<std::sync::Mutex<bool>>,
    agent: Arc<Agent>,
}

impl Session {
    /// A working context for a fresh context generation.
    pub fn context(&self, round: u32, ui: UiHandle) -> crate::script::ScriptContext {
        crate::script::ScriptContext {
            session_id: self.id,
            working_dir: self.working_dir.clone(),
            cancel: self.cancel.clone(),
            limits: self.agent.limits,
            authority: self.agent.authority.clone(),
            memory: self.agent.memory.clone(),
            events: self.events.clone(),
            ui,
            round,
        }
    }

    pub fn conversation(&self) -> Vec<Turn> {
        self.conversation.lock().map(|c| c.clone()).unwrap_or_default()
    }

    pub fn push_turn(&self, turn: Turn) {
        if let Ok(mut guard) = self.conversation.lock() {
            guard.push(turn);
        }
    }

    /// Claim the turn, returning false when one is already running.
    pub fn begin_turn(&self) -> bool {
        match self.running.try_lock() {
            Ok(mut running) => {
                if *running {
                    return false;
                }
                *running = true;
                true
            }
            // A turn is already in flight, so the lock is held: treat that as
            // "busy" rather than failing the caller.
            Err(_) => false,
        }
    }

    pub fn end_turn(&self) {
        if let Ok(mut running) = self.running.try_lock() {
            *running = false;
        }
    }

    pub fn is_running(&self) -> bool {
        // A held lock means a turn is running.
        self.running.try_lock().is_err()
    }

    /// The session's current status.
    pub fn status(&self) -> SessionStatus {
        self.status.lock().map(|s| *s).unwrap_or(SessionStatus::Failed)
    }

    /// Record the status the turn ended in.
    pub fn set_status(&self, status: SessionStatus) {
        if let Ok(mut slot) = self.status.lock() {
            *slot = status;
        }
    }

    /// How many turns the conversation holds, for an overflow check.
    pub fn turn_count(&self) -> usize {
        self.conversation.try_lock().map(|c| c.len()).unwrap_or_default()
    }
}

/// Owns every live session.
pub struct SessionManager {
    deps: SessionDeps,
    sessions: Mutex<HashMap<SessionId, Arc<Session>>>,
    max_sessions: usize,
}

impl SessionManager {
    pub fn new(deps: SessionDeps) -> Arc<Self> {
        let max_sessions = deps.config.max_sessions;
        Arc::new(Self {
            deps,
            sessions: Mutex::new(HashMap::new()),
            max_sessions,
        })
    }

    /// Open a session scoped to a working directory.
    pub async fn create(&self, config: SessionConfig) -> Result<Arc<Session>, SessionError> {
        let working_dir = config.working_dir.canonicalize().map_err(|e| {
            SessionError::Invalid(format!(
                "{} is not a usable working directory: {e}",
                config.working_dir.display()
            ))
        })?;

        let count = self.sessions.lock().await.len();
        if count >= self.max_sessions {
            return Err(SessionError::Invalid(format!(
                "the runtime already holds {count} sessions, its limit"
            )));
        }

        let id = SessionId::new();
        let events = EventSink::new(id);
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let model = config
            .model
            .unwrap_or_else(|| self.deps.config.provider.model.clone());

        let limits = self.deps.limits();
        let agent = Arc::new(Agent {
            provider: self.deps.provider.clone(),
            script: self.deps.script.clone(),
            memory: self.deps.memory.clone(),
            authority: Arc::new(self.deps.config.authority.clone()),
            system: crate::provider::prompt::system_prompt(
                &self.deps.config,
                self.deps.script.describe(),
            ),
            max_rounds: self.deps.config.max_program_rounds,
            limits,
        });

        let session = Arc::new(Session {
            id,
            working_dir,
            model,
            status: std::sync::Mutex::new(SessionStatus::Created),
            events: events.clone(),
            cancel: CancellationToken::new(),
            conversation: Arc::new(std::sync::Mutex::new(Vec::new())),
            ui_tx,
            running: Arc::new(std::sync::Mutex::new(false)),
            agent,
        });

        // The UI channel is drained for the life of the session: `dex::respond`
        // and `dex::ask` publish into it, and this is what turns those into
        // events and into answers.
        let forward = session.clone();
        tokio::spawn(async move {
            while let Some(event) = ui_rx.recv().await {
                match event {
                    UiEvent::Respond(text) => {
                        forward
                            .events
                            .emit(dex_protocol::Event::Answer { text });
                    }
                    UiEvent::Ask { prompt_id, message, reply } => {
                        // Answering is a frontend responsibility; until one
                        // exists, the question is reported as an event and the
                        // program is told nobody answered.
                        forward
                            .events
                            .emit(dex_protocol::Event::UiPrompt {
                                call_id: dex_protocol::CallId(prompt_id),
                                message: message.clone(),
                            });
                        let _ = reply.send(String::new());
                    }
                }
            }
        });

        events.emit(dex_protocol::Event::SessionStarted {
            working_dir: session.working_dir.display().to_string(),
            model: session.model.clone(),
        });

        self.sessions.lock().await.insert(id, session.clone());
        Ok(session)
    }

    pub async fn get(&self, id: SessionId) -> Option<Arc<Session>> {
        self.sessions.lock().await.get(&id).cloned()
    }

    pub async fn close(&self, id: SessionId) -> Result<(), SessionError> {
        let removed = self.sessions.lock().await.remove(&id);
        let Some(session) = removed else {
            return Err(SessionError::Unknown(id));
        };
        // Cancel before announcing the end, so a turn in flight stops.
        session.cancel.cancel();
        session
            .events
            .emit(dex_protocol::Event::SessionFinished {
                status: SessionStatus::Cancelled,
            });
        Ok(())
    }

    pub async fn len(&self) -> usize {
        self.sessions.lock().await.len()
    }

    /// Whether the runtime currently holds no sessions.
    pub async fn is_empty(&self) -> bool {
        self.sessions.lock().await.is_empty()
    }

    /// The authority every session runs under.
    pub fn authority(&self) -> &Authority {
        &self.deps.config.authority
    }

    /// Run one user turn to completion.
    pub async fn run_turn(&self, session: Arc<Session>, text: String) -> SessionStatus {
        if !session.begin_turn() {
            session.events.emit(dex_protocol::Event::Error {
                error: dex_protocol::ErrorPayload::new(
                    dex_protocol::ErrorKind::Invalid,
                    "a turn is already running in this session",
                ),
            });
            return session.status();
        }

        session.push_turn(Turn::user(text.clone()));
        session
            .events
            .emit(dex_protocol::Event::UserMessage { text });

        let status = session.agent.run(&session).await;

        session.end_turn();
        session.set_status(status);
        session.events.emit(dex_protocol::Event::SessionFinished { status });
        status
    }
}

/// Describe what a session is permitted to do, for `ListCapabilities`.
pub fn granted_for(authority: &Authority) -> Ack {
    Ack::Capabilities {
        capabilities: Vec::new(),
        authorities: authority.describe(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("no such session: {0}")]
    Unknown(SessionId),
    #[error("invalid request: {0}")]
    Invalid(String),
}

impl SessionError {
    /// Flatten onto the wire, where a missing session is the client's problem.
    pub fn payload(&self) -> dex_protocol::ErrorPayload {
        use dex_protocol::{ErrorKind, ErrorPayload};
        match self {
            SessionError::Unknown(_) => ErrorPayload::new(ErrorKind::Invalid, self.to_string()),
            SessionError::Invalid(_) => ErrorPayload::new(ErrorKind::Invalid, self.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Authority;

    #[test]
    fn prose_only_replies_produce_no_program() {
        // The compiler would otherwise report a syntax error pointing at the
        // model's own prose.
        assert!(extract::extract("I found nothing matching that.").is_none());
    }

    #[test]
    fn a_fenced_program_is_found_among_prose() {
        let reply = "Sure.\n\n```rune\npub fn main() { 1 }\n```";
        let (code, _) = extract::extract(reply).expect("extracted");
        assert_eq!(code, "pub fn main() { 1 }");
    }

    #[test]
    fn the_authority_description_lists_every_grant() {
        let authority =
            Authority::parse("filesystem.read=/work/**;memory.write=*").expect("parse");
        let ack = granted_for(&authority);
        let Ack::Capabilities { authorities, .. } = ack else {
            panic!("expected capabilities");
        };
        assert_eq!(authorities.len(), 2);
    }
}