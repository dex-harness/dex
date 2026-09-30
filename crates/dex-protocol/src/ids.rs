//! Identifiers used on the wire.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Opaque 128-bit session identifier.
///
/// Rendered as 32 lowercase hex characters so it is readable in logs and in a
/// hex dump of a captured frame. The bytes are otherwise meaningless to a
/// client: a session id is only ever echoed back.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SessionId(pub [u8; 16]);

impl SessionId {
    pub fn new() -> Self {
        // A session id only needs to be unique among the live sessions of a
        // single runtime process, which a monotonic counter makes trivial and
        // keeps ids stable and cheap to compare.
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut bytes = [0u8; 16];
        bytes[..8].copy_from_slice(&n.to_be_bytes());
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SessionId({self})")
    }
}

/// Client-chosen request id, echoed back on the matching acknowledgement.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(pub u64);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RequestId({})", self.0)
    }
}

/// Correlates the start/output/finish events of a single capability invocation.
///
/// A program may fan out many concurrent invocations, so this is allocated per
/// call rather than per program.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CallId(pub u64);

impl fmt::Display for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "c{}", self.0)
    }
}

impl fmt::Debug for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CallId({})", self.0)
    }
}
