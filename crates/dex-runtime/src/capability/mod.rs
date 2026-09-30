//! The capability layer.
//!
//! A capability is a typed operation a generated program can invoke. There is
//! deliberately no generic `exec` or `shell`: the surface contains only
//! operations someone chose to expose, and each one names the authority it
//! needs.
//!
//! Every call goes through [`CapabilityCtx`], in this order:
//!
//! 1. charge the budget and check for cancellation,
//! 2. resolve the path and confirm it is inside the session working directory,
//! 3. confirm a matching authority was granted for that resource,
//! 4. do the work,
//! 5. emit the resulting events.
//!
//! Putting the prologue in one place is what makes the guarantees hold: a
//! capability implementation cannot forget to authorize, because the only way
//! to reach the filesystem is a `CapabilityCtx` method that already did.

pub mod filesystem;
pub mod git;
pub mod guard;
pub mod process;
pub mod repo;
pub mod testing;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dex_protocol::{
    CallId, CapabilityErrorKind, CapabilityErrorKind as ErrorKind, Event, OutputStream,
};
use tokio_util::sync::CancellationToken;

use crate::auth::{Authority, Denied};
use crate::budget::{BudgetError, BudgetMeter};
use crate::events::EventSink;
use crate::memory::MemoryStore;

pub use guard::PathGuard;
/// A capability failure, carrying the reason a program branches on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityError {
    pub kind: ErrorKind,
    pub message: String,
}

impl CapabilityError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidArgument, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::ResourceNotFound, message)
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::OperationFailed, message)
    }

    pub fn denied(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::PermissionDenied, message)
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Cancelled, message)
    }
}

impl std::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for CapabilityError {}

impl From<BudgetError> for CapabilityError {
    fn from(e: BudgetError) -> Self {
        Self::new(CapabilityErrorKind::BudgetExceeded, e.to_string())
    }
}

impl From<Denied> for CapabilityError {
    fn from(e: Denied) -> Self {
        Self::new(CapabilityErrorKind::PermissionDenied, e.to_string())
    }
}

impl From<std::io::Error> for CapabilityError {
    fn from(e: std::io::Error) -> Self {
        Self::new(ErrorKind::OperationFailed, e.to_string())
    }
}

/// Everything a capability implementation is allowed to touch.
#[derive(Clone)]
pub struct CapabilityCtx {
    pub guard: PathGuard,
    pub authority: Arc<Authority>,
    pub budget: BudgetMeter,
    pub events: EventSink,
    pub memory: Arc<MemoryStore>,
    pub cancel: CancellationToken,
    pub call: CallId,
}

impl CapabilityCtx {
    pub fn new(
        guard: PathGuard,
        authority: Arc<Authority>,
        budget: BudgetMeter,
        events: EventSink,
        memory: Arc<MemoryStore>,
        cancel: CancellationToken,
        call: CallId,
    ) -> Self {
        Self {
            guard,
            authority,
            budget,
            events,
            memory,
            cancel,
            call,
        }
    }

    pub fn working_dir(&self) -> &Path {
        self.guard.root()
    }

    /// Step 1 of the prologue: charge the budget, then check cancellation.
    ///
    /// The budget is charged before authorization so a program cannot spin on a
    /// capability it is not allowed to call without spending its allowance.
    pub fn enter(&self, capability: &str) -> Result<(), CapabilityError> {
        self.budget.charge_capability()?;
        if self.cancel.is_cancelled() {
            return Err(CapabilityError::cancelled(format!(
                "{capability}: cancelled before it started"
            )));
        }
        Ok(())
    }

    /// Step 3 for a non-path resource.
    pub fn authorize(&self, authority: &str, resource: &str) -> Result<(), CapabilityError> {
        self.authority
            .check(authority, resource)
            .map_err(CapabilityError::from)
    }

    /// Steps 2 and 3 for a path that must already exist.
    pub fn resolve_read(&self, authority: &str, path: &str) -> Result<PathBuf, CapabilityError> {
        let resolved = self.guard.resolve_existing(path)?;
        self.authority.check_path(authority, &resolved)?;
        Ok(resolved)
    }

    /// Steps 2 and 3 for a path that may not exist yet.
    pub fn resolve_write(&self, authority: &str, path: &str) -> Result<PathBuf, CapabilityError> {
        let resolved = self.guard.resolve_for_write(path)?;
        self.authority.check_path(authority, &resolved)?;
        Ok(resolved)
    }

    /// Read a file, charging the read budget for its size.
    pub fn read_file(&self, path: &Path) -> Result<Vec<u8>, CapabilityError> {
        let bytes = std::fs::read(path)?;
        self.budget.charge_read(bytes.len() as u64)?;
        Ok(bytes)
    }

    /// Write a file, charging the write budget and emitting a `FileChanged`
    /// event describing the change.
    pub fn write_file(&self, path: &Path, bytes: &[u8]) -> Result<(), CapabilityError> {
        self.budget.charge_write(bytes.len() as u64)?;
        let existed = path.exists();
        std::fs::write(path, bytes)?;
        self.events.emit(Event::FileChanged {
            path: path.display().to_string(),
            change: if existed {
                dex_protocol::FileChange::Modified
            } else {
                dex_protocol::FileChange::Created
            },
        });
        Ok(())
    }

    /// Emit the start of a capability invocation.
    pub fn emit_started(&self, capability: &str, args: impl Into<String>) {
        self.events.emit(Event::CapabilityStarted {
            call_id: self.call,
            capability: capability.to_string(),
            args: args.into(),
        });
    }

    pub fn emit_finished(&self, capability: &str, ok: bool, summary: impl Into<String>) {
        self.events.emit(Event::CapabilityFinished {
            call_id: self.call,
            capability: capability.to_string(),
            ok,
            summary: summary.into(),
        });
    }

    pub fn emit_output(&self, chunk: impl Into<String>) {
        self.events.emit(Event::CapabilityOutput {
            call_id: self.call,
            chunk: chunk.into(),
        });
    }

    pub fn emit_process_started(&self, target: &str, args: &[String]) {
        self.events.emit(Event::ProcessStarted {
            call_id: self.call,
            target: target.to_string(),
            args: args.to_vec(),
        });
    }

    pub fn emit_process_output(&self, stream: OutputStream, chunk: impl Into<String>) {
        self.events.emit(Event::ProcessOutput {
            call_id: self.call,
            stream,
            chunk: chunk.into(),
        });
    }

    pub fn emit_process_finished(
        &self,
        exit_code: Option<i32>,
        duration_ms: u64,
        truncated: bool,
    ) {
        self.events.emit(Event::ProcessFinished {
            call_id: self.call,
            exit_code,
            duration_ms,
            truncated,
        });
    }
}

impl std::fmt::Debug for CapabilityCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityCtx")
            .field("working_dir", &self.guard.root())
            .field("call", &self.call)
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::ExecutionBudget;
    use dex_protocol::SessionId;

    fn ctx(root: &Path, authority: &str, budget: ExecutionBudget) -> CapabilityCtx {
        let _ = SessionId::new();
        CapabilityCtx::new(
            PathGuard::new(root).expect("guard"),
            Arc::new(Authority::parse(authority).expect("authority")),
            BudgetMeter::new(budget),
            EventSink::new(SessionId::new()),
            Arc::new(MemoryStore::new(root.join("memory"))),
            CancellationToken::new(),
            CallId(1),
        )
    }

    #[test]
    fn a_call_without_authority_is_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "x").expect("write");
        let ctx = ctx(dir.path(), "", ExecutionBudget::default());
        assert_eq!(
            ctx.resolve_read("filesystem.read", "a.rs").unwrap_err().kind,
            ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn authority_and_containment_must_both_hold() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "x").expect("write");
        let ctx = ctx(
            dir.path(),
            "filesystem.read=/other/**",
            ExecutionBudget::default(),
        );
        // Inside the working directory, but the grant does not cover it.
        assert_eq!(
            ctx.resolve_read("filesystem.read", "a.rs").unwrap_err().kind,
            ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn reading_charges_the_read_budget() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "0123456789").expect("write");
        let ctx = ctx(
            dir.path(),
            "filesystem.read=*",
            ExecutionBudget {
                read_bytes: 4,
                ..ExecutionBudget::default()
            },
        );
        let path = ctx.resolve_read("filesystem.read", "a.rs").expect("resolve");
        assert_eq!(
            ctx.read_file(&path).unwrap_err().kind,
            ErrorKind::BudgetExceeded
        );
    }

    #[test]
    fn writing_charges_the_write_budget_and_emits_a_file_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx(
            dir.path(),
            "filesystem.write=*",
            ExecutionBudget::default(),
        );
        let mut events = ctx.events.subscribe();
        let path = ctx.resolve_write("filesystem.write", "new.rs").expect("resolve");
        ctx.write_file(&path, b"hello").expect("write");
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");

        let mut saw_created = false;
        while let Ok(frame) = events.try_recv() {
            if let Event::FileChanged { change, .. } = frame.event {
                assert_eq!(change, dex_protocol::FileChange::Created);
                saw_created = true;
            }
        }
        assert!(saw_created, "expected a FileChanged event");
    }

    #[test]
    fn a_second_write_of_the_same_file_reports_modified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx(
            dir.path(),
            "filesystem.write=*",
            ExecutionBudget::default(),
        );
        let path = ctx.resolve_write("filesystem.write", "f.txt").expect("resolve");
        ctx.write_file(&path, b"one").expect("first");
        let mut events = ctx.events.subscribe();
        ctx.write_file(&path, b"two").expect("second");
        let changes: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|f| match f.event {
                Event::FileChanged { change, .. } => Some(change),
                _ => None,
            })
            .collect();
        assert_eq!(changes, vec![dex_protocol::FileChange::Modified]);
    }

    #[test]
    fn a_cancelled_context_refuses_before_doing_work() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx(dir.path(), "filesystem.read=*", ExecutionBudget::default());
        ctx.cancel.cancel();
        assert_eq!(
            ctx.enter("repo.read").unwrap_err().kind,
            ErrorKind::Cancelled
        );
    }

    #[test]
    fn entering_charges_the_budget_even_when_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx(
            dir.path(),
            "",
            ExecutionBudget {
                capability_calls: 1,
                ..ExecutionBudget::default()
            },
        );
        ctx.enter("repo.read").expect("first");
        assert_eq!(
            ctx.enter("repo.read").unwrap_err().kind,
            ErrorKind::BudgetExceeded
        );
    }
}
