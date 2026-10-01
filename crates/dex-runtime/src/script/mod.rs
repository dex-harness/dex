//! The script runtime boundary.
//!
//! This is the seam the architecture depends on. Everything above it —
//! capabilities, authorization, memory, sessions, the provider, events, the CLI
//! — is written against [`ScriptRuntime`] and knows nothing about Rune. Only
//! `script::rune` imports the Rune crate, which is what makes the language an
//! implementation detail rather than a dependency.
//!
//! The model does not call tools here. It hands over a program; the runtime
//! executes it and hands back a result. A program may do any amount of work —
//! searching, filtering, editing, testing — before returning, and none of that
//! costs a model round trip.

pub mod rune;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use dex_protocol::CapabilityErrorKind;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::auth::Authority;
use crate::budget::ExecutionBudget;
use crate::events::EventSink;
use crate::memory::MemoryStore;
use dex_protocol::SessionId;

/// Everything one program execution is allowed to reach.
#[derive(Clone)]
pub struct ScriptContext {
    pub session_id: SessionId,
    /// The session's working directory, already canonicalized.
    pub working_dir: PathBuf,
    pub cancel: CancellationToken,
    pub limits: ExecutionBudget,
    pub authority: Arc<Authority>,
    pub memory: Arc<MemoryStore>,
    pub events: EventSink,
    pub ui: UiHandle,
    /// Zero-based index of this program within the current turn.
    pub round: u32,
}

impl ScriptContext {
    /// Build the per-program context the capability layer uses.
    ///
    /// Each program gets one, and each capability call within it takes a
    /// further [`crate::capability::CapabilityCtx::scoped`] copy carrying its
    /// own call id. The budget meter is created here, so a program's spend is
    /// counted against that program alone and the next one starts fresh.
    pub fn capability_ctx(&self) -> Result<crate::capability::CapabilityCtx, ScriptError> {
        let guard = crate::capability::PathGuard::new(&self.working_dir).map_err(|e| {
            ScriptError::Unavailable(format!("the session working directory is unusable: {e}"))
        })?;
        Ok(crate::capability::CapabilityCtx::new(
            guard,
            self.authority.clone(),
            crate::budget::BudgetMeter::new(self.limits),
            self.events.clone(),
            self.memory.clone(),
            self.cancel.clone(),
            dex_protocol::CallId(0),
            self.ui.clone(),
        ))
    }
}

/// A capability the program actually invoked, for observability and for
/// reporting what a stored program needs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityUse {
    pub name: String,
    /// Authority the call required, or `None` for one that needed none.
    pub required_authority: Option<String>,
    pub mutating: bool,
}

/// What one program produced.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ScriptResult {
    /// The program's return value, converted to JSON.
    pub value: serde_json::Value,
    /// Set when the program called `ui.respond`. Its presence ends the turn.
    pub response: Option<String>,
    pub capabilities_used: Vec<CapabilityUse>,
    pub duration_ms: u64,
}

impl ScriptResult {
    pub fn new(value: serde_json::Value) -> Self {
        Self {
            value,
            ..Default::default()
        }
    }
}

/// Why a program did not produce a result.
#[derive(Clone, Debug, thiserror::Error)]
pub enum ScriptError {
    /// The program did not parse. `diagnostics` carries the compiler's own
    /// report, which is what lets a model fix its own code.
    #[error("program did not compile: {message}")]
    Compile {
        message: String,
        diagnostics: Option<String>,
    },
    /// The program raised an uncaught error.
    #[error("program raised: {message}")]
    Raised { message: String },
    /// The wall-clock budget elapsed.
    #[error("program exceeded its {}ms budget", .0.as_millis())]
    Timeout(Duration),
    #[error("program was cancelled")]
    Cancelled,
    /// A capability failed in a way the program did not handle.
    #[error("{kind}: {message}")]
    Capability {
        kind: CapabilityErrorKind,
        message: String,
    },
    /// The runtime could not run the program at all.
    #[error("script runtime unavailable: {0}")]
    Unavailable(String),
}

impl ScriptError {
    /// Flatten into the wire error, keeping the capability reason when there is
    /// one so a frontend can render it and a model can branch on it.
    pub fn to_payload(&self) -> dex_protocol::ErrorPayload {
        use dex_protocol::ErrorKind;
        let plain = |kind, message: String| dex_protocol::ErrorPayload::new(kind, message);
        match self {
            ScriptError::Compile { message, .. } => plain(ErrorKind::Script, message.clone()),
            ScriptError::Raised { message } => plain(ErrorKind::Script, message.clone()),
            ScriptError::Timeout(_) => plain(ErrorKind::Timeout, self.to_string()),
            ScriptError::Cancelled => plain(ErrorKind::Cancelled, self.to_string()),
            ScriptError::Capability { kind, message } => {
                dex_protocol::ErrorPayload::capability(*kind, message.clone())
            }
            ScriptError::Unavailable(message) => plain(ErrorKind::Script, message.clone()),
        }
    }
}

/// A language that can execute model-generated programs.
#[async_trait]
pub trait ScriptRuntime: Send + Sync {
    /// Compile and run `program`.
    ///
    /// Implementations must honour `ctx.cancel` and `ctx.limits.wall_clock`.
    /// Cancellation is best-effort at instruction granularity: a language with
    /// no execution budget can only be stopped at an await point or when the
    /// driving future is dropped at the deadline.
    async fn execute(
        &self,
        program: &str,
        ctx: ScriptContext,
    ) -> Result<ScriptResult, ScriptError>;

    /// The language's name and dialect, for the system prompt and `dex.about`.
    fn describe(&self) -> ScriptLanguage;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptLanguage {
    pub name: &'static str,
    pub version: &'static str,
}

/// Interaction with the human, as a capability rather than a channel of its own.
///
/// The model still reaches the user by writing code; this is just another
/// capability the program can call.
#[derive(Clone)]
pub struct UiHandle {
    out: mpsc::UnboundedSender<UiEvent>,
    /// Written by `respond` so the agent can read the answer synchronously.
    response: Arc<std::sync::Mutex<Option<String>>>,
}

/// The program-facing half of a UI channel.
impl UiHandle {
    /// Build a handle over an existing sender.
    pub fn new(out: mpsc::UnboundedSender<UiEvent>) -> Self {
        Self {
            out,
            response: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Pair a handle with the receiving end the session drains.
    pub fn channel() -> (UiHandle, mpsc::UnboundedReceiver<UiEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (UiHandle::new(tx), rx)
    }

    /// Deliver the turn's answer and end the turn.
    ///
    /// The answer is also written to a slot the caller can read synchronously.
    /// The channel alone would leave the agent guessing whether the forwarder
    /// task had run yet; the slot is written by the same call that sends, so
    /// reading it after the program returns is deterministic.
    pub fn respond(&self, message: impl Into<String>) {
        let message = message.into();
        if let Ok(mut slot) = self.response.lock() {
            *slot = Some(message.clone());
        }
        let _ = self.out.send(UiEvent::Respond(message));
    }

    /// Take the answer if `respond` was called, clearing it.
    pub fn take_response(&self) -> Option<String> {
        self.response.lock().ok().and_then(|mut slot| slot.take())
    }

    /// Ask the human something and wait for a reply.
    pub async fn ask(&self, message: impl Into<String>) -> Result<String, ScriptError> {
        let (tx, rx) = oneshot::channel();
        self.out
            .send(UiEvent::Ask {
                prompt_id: next_prompt_id(),
                message: message.into(),
                reply: tx,
            })
            .map_err(|_| ScriptError::Unavailable("the session is no longer listening".into()))?;
        rx.await.map_err(|_| {
            ScriptError::Unavailable("the question was never answered".into())
        })
    }
}

/// What a program asked the session to do on the user's behalf.
pub enum UiEvent {
    Respond(String),
    Ask {
        /// Echoed back by the frontend when the human replies.
        prompt_id: u64,
        message: String,
        reply: oneshot::Sender<String>,
    },
}

impl std::fmt::Debug for UiEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UiEvent::Respond(text) => write!(f, "Respond({text:?})"),
            UiEvent::Ask {
                prompt_id, message, ..
            } => write!(f, "Ask({prompt_id}, {message:?})"),
        }
    }
}

/// Monotonic identifier for an outstanding `ui.ask`.
static PROMPT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn next_prompt_id() -> u64 {
    PROMPT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dex_protocol::ErrorKind;

    /// A runtime that is not Rune, proving the boundary is language-neutral.
    struct StubRuntime;

    #[async_trait]
    impl ScriptRuntime for StubRuntime {
        async fn execute(
            &self,
            _program: &str,
            _ctx: ScriptContext,
        ) -> Result<ScriptResult, ScriptError> {
            Ok(ScriptResult::new(serde_json::json!({"stub": true})))
        }

        fn describe(&self) -> ScriptLanguage {
            ScriptLanguage {
                name: "stub",
                version: "0",
            }
        }
    }

    #[tokio::test]
    async fn the_trait_is_usable_without_knowing_the_language() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ScriptContext {
            session_id: SessionId::new(),
            working_dir: dir.path().to_path_buf(),
            cancel: CancellationToken::new(),
            limits: ExecutionBudget::default(),
            authority: Arc::new(Authority::deny_all()),
            memory: Arc::new(MemoryStore::new(dir.path().join("m"))),
            events: EventSink::new(SessionId::new()),
            ui: UiHandle::new(tx),
            round: 0,
        };

        // Boxed as a trait object: the caller never names a concrete language.
        let runtime: Arc<dyn ScriptRuntime> = Arc::new(StubRuntime);
        let result = runtime.execute("anything", ctx).await.expect("run");
        assert_eq!(result.value["stub"], true);
        assert!(rx.try_recv().is_err(), "a stub should not have used the UI");
    }

    #[tokio::test]
    async fn respond_is_observable_by_the_session() {
        let (tx, mut rx) = UiHandle::channel();
        tx.respond("the answer");
        match rx.recv().await.expect("event") {
            UiEvent::Respond(text) => assert_eq!(text, "the answer"),
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ask_waits_for_a_reply() {
        let (tx, mut rx) = UiHandle::channel();
        let handle = tx;
        let waiting = tokio::spawn(async move { handle.ask("proceed?").await });

        match rx.recv().await.expect("event") {
            UiEvent::Ask { message, reply, .. } => {
                assert_eq!(message, "proceed?");
                reply.send("yes".into()).expect("deliver");
            }
            other => panic!("expected a question, got {other:?}"),
        }
        assert_eq!(waiting.await.expect("join").expect("reply"), "yes");
    }

    #[tokio::test]
    async fn an_unanswered_ask_fails_rather_than_hanging_forever() {
        let (tx, mut rx) = UiHandle::channel();
        let handle = tx;
        let waiting = tokio::spawn(async move { handle.ask("proceed?").await });
        // The session drops the sender without answering.
        match rx.recv().await.expect("event") {
            UiEvent::Ask { reply, .. } => drop(reply),
            other => panic!("expected a question, got {other:?}"),
        }
        assert!(waiting.await.expect("join").is_err());
    }

    #[test]
    fn script_errors_map_onto_the_wire_taxonomy() {
        assert_eq!(
            ScriptError::Timeout(Duration::from_secs(30))
                .to_payload()
                .kind,
            ErrorKind::Timeout
        );
        assert_eq!(ScriptError::Cancelled.to_payload().kind, ErrorKind::Cancelled);
        // A capability failure keeps its specific reason so a program can
        // branch on it after the fact.
        let denied = ScriptError::Capability {
            kind: CapabilityErrorKind::PermissionDenied,
            message: "nope".into(),
        }
        .to_payload();
        assert_eq!(denied.kind, ErrorKind::Tool);
        assert_eq!(denied.capability, Some(CapabilityErrorKind::PermissionDenied));
    }
}
