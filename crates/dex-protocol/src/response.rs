//! Runtime-to-client responses.

use crate::error::ErrorPayload;
use crate::event::{EventFrame, SessionStatus};
use crate::ids::{RequestId, SessionId};
use serde::{Deserialize, Serialize};

/// A granted authority, as reported by `ListCapabilities`.
///
/// The frontend shows this so a user can see exactly what a session is
/// permitted to do; the runtime enforces it independently.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct GrantedAuthority {
    /// e.g. `filesystem.write`
    pub capability: String,
    /// Scope glob, or `*` for any resource.
    pub scope: String,
}

/// One entry of the capability surface, so a frontend (and the system prompt
/// derived from it) can describe what exists without a round trip.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CapabilityDescriptor {
    /// Fully qualified name, e.g. `repo.find`.
    pub name: String,
    /// One-line description used to build the model's capability reference.
    pub summary: String,
    /// Authority this capability requires.
    pub required_authority: String,
    /// True when a call can change the working tree.
    pub mutating: bool,
}

/// A successful acknowledgement.
///
/// Externally tagged, so declaration order is part of the wire format.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ack {
    CreateSession {
        session_id: SessionId,
        status: SessionStatus,
        /// Model actually in use for this session, after default resolution.
        model: String,
    },
    /// The request was accepted; the turn is now streaming events.
    Accepted,
    /// The session reached a terminal state for this turn.
    Finished {
        status: SessionStatus,
    },
    Closed,
    Capabilities {
        capabilities: Vec<CapabilityDescriptor>,
        authorities: Vec<GrantedAuthority>,
    },
    Ok,
}

impl Ack {
    pub fn kind(&self) -> &'static str {
        match self {
            Ack::CreateSession { .. } => "create_session",
            Ack::Accepted => "accepted",
            Ack::Finished { .. } => "finished",
            Ack::Closed => "closed",
            Ack::Capabilities { .. } => "capabilities",
            Ack::Ok => "ok",
        }
    }
}

/// Everything the runtime sends. A response is always either an acknowledgement
/// of one request, or an unsolicited event.
///
/// Externally tagged, so declaration order is part of the wire format.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerResponse {
    Ack {
        id: RequestId,
        ok: Ack,
    },
    Err {
        id: RequestId,
        error: ErrorPayload,
    },
    Event(EventFrame),
}

impl ServerResponse {
    pub fn ack(id: RequestId, ok: Ack) -> Self {
        ServerResponse::Ack { id, ok }
    }

    pub fn err(id: RequestId, error: ErrorPayload) -> Self {
        ServerResponse::Err { id, error }
    }
}
