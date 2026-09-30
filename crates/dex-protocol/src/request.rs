//! Client-to-runtime requests.

use crate::ids::RequestId;
use serde::{Deserialize, Serialize};

/// Every request a frontend may send.
///
/// Externally tagged, so the declaration order below is part of the wire
/// format. The runtime rejects a frame it cannot decode rather than guessing.
///
/// Requests are plain data: no callbacks, no streams, no runtime types. The
/// runtime answers with a [`crate::response::ServerResponse`], which is either
/// an acknowledgement of one of these or an unsolicited event.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientRequest {
    /// Open a session scoped to `working_dir`.
    CreateSession {
        working_dir: String,
        /// Overrides the runtime default when present.
        model: Option<String>,
    },
    /// Append a user turn and drive it to completion, streaming events.
    SendMessage {
        session_id: crate::ids::SessionId,
        text: String,
    },
    /// Re-subscribe to an existing session after reconnecting. Safe to call at
    /// any time; a session the runtime no longer knows about is an error.
    Attach {
        session_id: crate::ids::SessionId,
    },
    /// Request cancellation. Idempotent, and safe from any session state.
    Cancel {
        session_id: crate::ids::SessionId,
    },
    CloseSession {
        session_id: crate::ids::SessionId,
    },
    /// Introspect the capability surface and the authorities actually granted.
    ListCapabilities,
}

/// A request paired with the id the frontend chose for it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct RequestFrame {
    pub id: RequestId,
    pub request: ClientRequest,
}

impl RequestFrame {
    pub fn new(id: RequestId, request: ClientRequest) -> Self {
        Self { id, request }
    }
}
