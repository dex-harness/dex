//! The DEX wire protocol.
//!
//! This crate is the single source of truth for everything that crosses a
//! process boundary in DEX. It is deliberately free of tokio and of any runtime
//! internal type, so a frontend can depend on it without pulling in the
//! runtime. The transport lives in `dex-runtime::server::frame`; what lives
//! here is the vocabulary and the codec primitives.
//!
//! # Encoding
//!
//! Frames on the IPC path are binary, not JSON: a `u32` little-endian length
//! prefix followed by a `postcard`-encoded payload. JSON appears only on the
//! provider side of the system, where it is the wire format of the upstream
//! API and not something DEX chooses.

pub mod error;
pub mod event;
pub mod ids;
pub mod request;
pub mod response;

pub use error::{CapabilityErrorKind, ErrorKind, ErrorPayload};
pub use event::{
    Event, EventFrame, FileChange, Millis, OutputStream, SessionStatus,
};
pub use ids::{CallId, RequestId, SessionId};
pub use request::{ClientRequest, RequestFrame};
pub use response::{Ack, CapabilityDescriptor, GrantedAuthority, ServerResponse};

use serde::de::DeserializeOwned;
use serde::Serialize;
use thiserror::Error;

/// Bumped only for incompatible changes. A frame whose leading version byte
/// does not match is rejected before decoding, so a stale frontend fails with a
/// clear message instead of a deserialization error.
pub const PROTOCOL_VERSION: u16 = 1;

/// Largest accepted frame payload. Generous for a program plus its events,
/// small enough that a corrupt length prefix cannot make the runtime allocate
/// without bound.
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("frame payload is {size} bytes, which exceeds the {max} byte limit")]
    TooLarge { size: usize, max: usize },
    #[error("frame payload is empty")]
    Empty,
    #[error("protocol version mismatch: runtime speaks {expected}, frame speaks {found}")]
    VersionMismatch { expected: u16, found: u16 },
    #[error("malformed frame: {0}")]
    Decode(#[from] postcard::Error),
    #[error("frame encoding failed: {0}")]
    Encode(String),
}

/// Serialize `value` into a length-prefixed frame, ready to write to a socket.
///
/// The result is `len.to_le_bytes() || postcard(value)`.
pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    let mut body = postcard::to_allocvec(value).map_err(|e| CodecError::Encode(e.to_string()))?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(CodecError::TooLarge {
            size: body.len(),
            max: MAX_FRAME_BYTES,
        });
    }
    let len = u32::try_from(body.len()).map_err(|_| CodecError::TooLarge {
        size: body.len(),
        max: MAX_FRAME_BYTES,
    })?;
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&len.to_le_bytes());
    out.append(&mut body);
    Ok(out)
}

/// Encode without the length prefix. Used by the fixture tests, which want to
/// compare payloads rather than transport framing.
pub fn encode_payload<T: Serialize>(value: &T) -> Result<Vec<u8>, CodecError> {
    postcard::to_allocvec(value).map_err(|e| CodecError::Encode(e.to_string()))
}

/// Read the length prefix and return the payload slice.
///
/// Returns `Ok(None)` when `buf` does not yet hold a complete frame, so a
/// streaming reader can accumulate and retry without a partial-read state
/// machine.
pub fn split_frame(buf: &[u8]) -> Result<Option<(&[u8], &[u8])>, CodecError> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(CodecError::TooLarge {
            size: len,
            max: MAX_FRAME_BYTES,
        });
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    Ok(Some((&buf[..4], &buf[4..4 + len])))
}

/// Decode a payload produced by [`encode_payload`] or by the second half of
/// [`split_frame`].
pub fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, CodecError> {
    if payload.is_empty() {
        return Err(CodecError::Empty);
    }
    postcard::from_bytes(payload).map_err(CodecError::from)
}
