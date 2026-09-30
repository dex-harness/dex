//! Structured runtime events.
//!
//! Every meaningful activity in a session produces an `Event`. Events are the
//! only thing a frontend needs in order to render a session, and they are
//! defined here rather than in the runtime so that a frontend never has to
//! guess at a shape.
//!
//! The session id and timestamp live on the envelope ([`EventFrame`]) rather
//! than on every variant, so adding a variant does not touch each one.

use crate::error::ErrorPayload;
use crate::ids::{CallId, SessionId};
use serde::{Deserialize, Serialize};

/// Monotonic-ish wall clock in milliseconds since the Unix epoch.
pub type Millis = u64;

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct EventFrame {
    pub session_id: SessionId,
    pub ts_ms: Millis,
    pub event: Event,
}

impl EventFrame {
    pub fn new(session_id: SessionId, ts_ms: Millis, event: Event) -> Self {
        Self {
            session_id,
            ts_ms,
            event,
        }
    }
}

/// Which stream a chunk of process output came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// How a file changed, as observed by the runtime.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileChange {
    Created,
    Modified,
    Deleted,
}

/// Session lifecycle, as reported by `SessionFinished` and by the
/// `CreateSession` acknowledgement.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum SessionStatus {
    Created,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
}

/// Structured runtime activity.
///
/// Externally tagged: the variant is encoded as a discriminant followed by its
/// fields. `postcard` writes the discriminant as an index, so the *declaration
/// order of these variants is part of the wire format*. The golden fixtures in
/// `tests/golden.rs` exist to catch a reordering that would otherwise change the
/// encoding silently.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum Event {
    // ---- session lifecycle -------------------------------------------------
    SessionStarted {
        working_dir: String,
        model: String,
    },
    SessionFinished {
        status: SessionStatus,
    },

    // ---- conversation ------------------------------------------------------
    UserMessage {
        text: String,
    },
    /// A model request is about to be issued. DEX never sends a tool schema.
    ModelStarted,
    /// A fragment of generated text, which is program source, not prose.
    ModelDelta {
        text: String,
    },

    // ---- program execution -------------------------------------------------
    /// A new round of the turn: the model produced a program to run.
    ProgramStarted {
        call_id: CallId,
        round: u32,
        source: String,
    },
    ProgramFinished {
        call_id: CallId,
        duration_ms: u64,
    },
    /// The program raised, failed to compile, or exceeded a budget.
    ProgramFailed {
        call_id: CallId,
        error: ErrorPayload,
        /// Compiler or evaluator diagnostics, when there are any.
        diagnostics: Option<String>,
    },

    // ---- capabilities ------------------------------------------------------
    CapabilityStarted {
        call_id: CallId,
        /// Fully qualified capability name, e.g. `repo.find`.
        capability: String,
        /// Rendered argument summary. Never contains file contents.
        args: String,
    },
    CapabilityOutput {
        call_id: CallId,
        chunk: String,
    },
    CapabilityFinished {
        call_id: CallId,
        capability: String,
        ok: bool,
        /// One-line outcome for the CLI to render.
        summary: String,
    },

    // ---- observable side effects ------------------------------------------
    FileChanged {
        path: String,
        change: FileChange,
    },
    ProcessStarted {
        call_id: CallId,
        /// Allowlisted tool name, e.g. `cargo`.
        target: String,
        args: Vec<String>,
    },
    ProcessOutput {
        call_id: CallId,
        stream: OutputStream,
        chunk: String,
    },
    ProcessFinished {
        call_id: CallId,
        exit_code: Option<i32>,
        duration_ms: u64,
        truncated: bool,
    },
    MemoryWrite {
        key: String,
        kind: String,
    },
    /// The program asked the human something (`ui.ask`).
    UiPrompt {
        call_id: CallId,
        message: String,
    },

    // ---- outcomes ----------------------------------------------------------
    /// The program produced the turn's answer (`ui.respond`).
    Answer {
        text: String,
    },
    Error {
        error: ErrorPayload,
    },
}
