//! Error taxonomy shared by the runtime, the capability layer and the CLI.
//!
//! The runtime distinguishes failure *kinds* (where the failure came from) and
//! capability errors carry a narrower *reason* that programs are expected to
//! branch on. Both are stable on the wire because a frontend renders them and a
//! model program matches on them.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Why a request or a turn failed.
///
/// Variants are serialized as PascalCase, matching the event naming convention.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ErrorKind {
    /// The provider request or stream failed.
    Model,
    /// A capability failed, or was denied.
    Tool,
    /// A spawned process failed or could not be started.
    Process,
    /// Transport-level failure between a frontend and the runtime.
    Ipc,
    /// The request was malformed, unknown, or violated a protocol contract.
    Invalid,
    /// Work was cancelled by the user or by session teardown.
    Cancelled,
    /// A deadline elapsed.
    Timeout,
    /// A configured resource budget was exhausted.
    Budget,
    /// A memory store operation failed.
    Memory,
    /// A Rune program failed to compile or raised an uncaught error.
    Script,
    /// The session itself failed.
    Session,
}

impl ErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorKind::Model => "Model",
            ErrorKind::Tool => "Tool",
            ErrorKind::Process => "Process",
            ErrorKind::Ipc => "Ipc",
            ErrorKind::Invalid => "Invalid",
            ErrorKind::Cancelled => "Cancelled",
            ErrorKind::Timeout => "Timeout",
            ErrorKind::Budget => "Budget",
            ErrorKind::Memory => "Memory",
            ErrorKind::Script => "Script",
            ErrorKind::Session => "Session",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a capability call did not produce a value.
///
/// A Rune program branches on these by name, so the set is deliberately small
/// and the strings are part of the public surface.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum CapabilityErrorKind {
    /// The capability is not registered in this runtime.
    CapabilityUnavailable,
    /// No granted authority matches this capability and resource.
    PermissionDenied,
    /// The target does not exist.
    ResourceNotFound,
    /// Arguments were missing, malformed, or out of range.
    InvalidArgument,
    /// The underlying operation failed for a capability-specific reason.
    OperationFailed,
    /// A per-capability deadline elapsed.
    Timeout,
    /// The call was cancelled.
    Cancelled,
    /// A configured budget (calls, bytes, output) was exhausted.
    BudgetExceeded,
}

impl CapabilityErrorKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CapabilityErrorKind::CapabilityUnavailable => "CapabilityUnavailable",
            CapabilityErrorKind::PermissionDenied => "PermissionDenied",
            CapabilityErrorKind::ResourceNotFound => "ResourceNotFound",
            CapabilityErrorKind::InvalidArgument => "InvalidArgument",
            CapabilityErrorKind::OperationFailed => "OperationFailed",
            CapabilityErrorKind::Timeout => "Timeout",
            CapabilityErrorKind::Cancelled => "Cancelled",
            CapabilityErrorKind::BudgetExceeded => "BudgetExceeded",
        }
    }
}

impl fmt::Display for CapabilityErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<CapabilityErrorKind> for ErrorKind {
    fn from(kind: CapabilityErrorKind) -> Self {
        match kind {
            CapabilityErrorKind::Timeout => ErrorKind::Timeout,
            CapabilityErrorKind::Cancelled => ErrorKind::Cancelled,
            CapabilityErrorKind::BudgetExceeded => ErrorKind::Budget,
            _ => ErrorKind::Tool,
        }
    }
}

/// A rendered error. `message` is the human/model-facing text; the kind fields
/// are the stable part a program or CLI branches on.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub kind: ErrorKind,
    /// Set when this error originated in a capability, so the model can catch
    /// the specific reason rather than a generic failure.
    ///
    /// Always encoded, including when `None`: the wire format is positional, so
    /// omitting a field would shift every field after it.
    pub capability: Option<CapabilityErrorKind>,
    pub message: String,
}

impl ErrorPayload {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            capability: None,
            message: message.into(),
        }
    }

    pub fn capability(kind: CapabilityErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            capability: Some(kind),
            message: message.into(),
        }
    }
}

impl fmt::Display for ErrorPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.capability {
            Some(capability) => write!(f, "{capability}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for ErrorPayload {}
