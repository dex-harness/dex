//! Filesystem capabilities: write, delete, delete_tree, exists, stat.
//!
//! Destructive operations exist as explicit, named capabilities rather than as
//! a general delete: `filesystem.delete` removes one path, `filesystem.delete_tree`
//! removes a directory, and each needs its own authority. A model that can
//! express "delete this tree" can be denied it without also losing the ability
//! to read a file.

use serde_json::json;

use super::{CapabilityCtx, CapabilityError};
use crate::capability::CapabilityErrorKind;

/// Authority required by the non-destructive operations.
pub const READ: &str = "filesystem.read";
/// Authority required to create or modify a file.
pub const WRITE: &str = "filesystem.write";
/// Authority required to remove a file.
pub const DELETE: &str = "filesystem.delete";

/// `filesystem.write(path, content)`
pub fn write(ctx: &CapabilityCtx, path: &str, content: &str) -> Result<serde_json::Value, CapabilityError> {
    ctx.enter("filesystem.write")?;
    let resolved = ctx.resolve_write(WRITE, path)?;
    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent)?;
    }
    ctx.write_file(&resolved, content.as_bytes())?;
    Ok(json!({
        "path": display(ctx, &resolved),
        "bytes_written": content.len(),
    }))
}

/// `filesystem.delete(path)` — one file or empty directory.
pub fn delete(ctx: &CapabilityCtx, path: &str) -> Result<serde_json::Value, CapabilityError> {
    ctx.enter("filesystem.delete")?;
    let resolved = ctx.resolve_read(DELETE, path)?;
    let meta = std::fs::metadata(&resolved)?;
    if meta.is_dir() {
        // Refuse rather than recurse: deleting a tree is a different, separately
        // authorised operation, and quietly escalating here would make the
        // narrower grant meaningless.
        return Err(CapabilityError::new(
            CapabilityErrorKind::InvalidArgument,
            format!(
                "{} is a directory; use filesystem.delete_tree to remove a tree",
                display(ctx, &resolved)
            ),
        ));
    }
    std::fs::remove_file(&resolved)?;
    ctx.events.emit(dex_protocol::Event::FileChanged {
        path: display(ctx, &resolved),
        change: dex_protocol::FileChange::Deleted,
    });
    Ok(json!({ "path": display(ctx, &resolved), "deleted": true }))
}

/// `filesystem.delete_tree(path)` — a directory and everything under it.
pub fn delete_tree(ctx: &CapabilityCtx, path: &str) -> Result<serde_json::Value, CapabilityError> {
    ctx.enter("filesystem.delete_tree")?;
    let resolved = ctx.resolve_read(DELETE, path)?;
    if !resolved.is_dir() {
        return Err(CapabilityError::new(
            CapabilityErrorKind::InvalidArgument,
            format!("{} is not a directory", display(ctx, &resolved)),
        ));
    }
    // Count before removing so the result reports real work done.
    let mut removed = 0u64;
    for entry in ignore::WalkBuilder::new(&resolved).build().flatten() {
        if entry.path().is_file() {
            removed += 1;
        }
    }
    std::fs::remove_dir_all(&resolved)?;
    ctx.events.emit(dex_protocol::Event::FileChanged {
        path: display(ctx, &resolved),
        change: dex_protocol::FileChange::Deleted,
    });
    Ok(json!({
        "path": display(ctx, &resolved),
        "files_removed": removed,
    }))
}

/// `filesystem.exists(path)`
pub fn exists(ctx: &CapabilityCtx, path: &str) -> Result<serde_json::Value, CapabilityError> {
    ctx.enter("filesystem.exists")?;
    // Resolve for write so a not-yet-existing path is answerable rather than a
    // not-found error: "does this file exist" has a false answer, not a failure.
    let resolved = ctx.resolve_write(READ, path)?;
    Ok(json!({ "path": display(ctx, &resolved), "exists": resolved.exists() }))
}

/// `filesystem.stat(path)`
pub fn stat(ctx: &CapabilityCtx, path: &str) -> Result<serde_json::Value, CapabilityError> {
    ctx.enter("filesystem.stat")?;
    let resolved = ctx.resolve_read(READ, path)?;
    let meta = std::fs::metadata(&resolved)?;
    let kind = if meta.is_dir() { "dir" } else { "file" };
    let modified_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    Ok(json!({
        "path": display(ctx, &resolved),
        "kind": kind,
        "size": meta.len(),
        "modified_ms": modified_ms,
        "read_only": meta.permissions().readonly(),
    }))
}

fn display(ctx: &CapabilityCtx, path: &std::path::Path) -> String {
    path.strip_prefix(ctx.guard.root())
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Authority;
    use crate::budget::{BudgetMeter, ExecutionBudget};
    use crate::capability::PathGuard;
    use crate::events::EventSink;
    use crate::memory::MemoryStore;
    use dex_protocol::{CallId, Event, SessionId};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    use crate::script::UiHandle;

    fn fixture(authority: &str) -> (tempfile::TempDir, CapabilityCtx) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("src/a.rs"), "old").expect("write");
        let ctx = CapabilityCtx::new(
            PathGuard::new(dir.path()).expect("guard"),
            Arc::new(Authority::parse(authority).expect("authority")),
            BudgetMeter::new(ExecutionBudget::default()),
            EventSink::new(SessionId::new()),
            Arc::new(MemoryStore::new(dir.path().join("memory"))),
            CancellationToken::new(),
            CallId(1),
            UiHandle::channel().0,
        );
        (dir, ctx)
    }

    #[test]
    fn write_creates_a_file_and_creates_missing_parents() {
        let (_d, ctx) = fixture("filesystem.write=*");
        let result = write(&ctx, "deep/nested/new.rs", "fn main() {}").expect("write");
        assert_eq!(result["bytes_written"], 12);
        let target = ctx.guard.root().join("deep/nested/new.rs");
        assert!(target.exists());
    }

    #[test]
    fn write_without_authority_is_denied() {
        let (_d, ctx) = fixture("filesystem.read=*");
        assert_eq!(
            write(&ctx, "new.rs", "x").unwrap_err().kind,
            CapabilityErrorKind::PermissionDenied
        );
    }

    #[test]
    fn write_cannot_escape_the_working_directory() {
        let (_d, ctx) = fixture("filesystem.write=*");
        assert_eq!(
            write(&ctx, "../escaped.rs", "x").unwrap_err().kind,
            CapabilityErrorKind::PermissionDenied
        );
    }

    #[test]
    fn delete_refuses_a_directory_so_the_narrow_grant_stays_meaningful() {
        let (_d, ctx) = fixture("filesystem.delete=*");
        let err = delete(&ctx, "src").expect_err("must refuse");
        assert_eq!(err.kind, CapabilityErrorKind::InvalidArgument);
        assert!(err.message.contains("delete_tree"));
    }

    #[test]
    fn delete_removes_a_file_and_reports_it() {
        let (_d, ctx) = fixture("filesystem.delete=*");
        let result = delete(&ctx, "src/a.rs").expect("delete");
        assert_eq!(result["deleted"], true);
        assert!(!ctx.guard.root().join("src/a.rs").exists());
    }

    #[test]
    fn delete_tree_removes_a_directory_and_counts_files() {
        let (_d, ctx) = fixture("filesystem.delete=*");
        std::fs::create_dir_all(ctx.guard.root().join("t/x")).expect("mkdir");
        std::fs::write(ctx.guard.root().join("t/one"), "1").expect("w");
        std::fs::write(ctx.guard.root().join("t/x/two"), "2").expect("w");

        let result = delete_tree(&ctx, "t").expect("delete_tree");
        assert_eq!(result["files_removed"], 2);
        assert!(!ctx.guard.root().join("t").exists());
    }

    #[test]
    fn delete_authority_is_independent_of_write() {
        // A session that may write but not delete keeps both file operations
        // and loses only the destructive one.
        let (_d, ctx) = fixture("filesystem.read=*;filesystem.write=*");
        assert!(write(&ctx, "ok.rs", "x").is_ok());
        assert_eq!(
            delete(&ctx, "src/a.rs").unwrap_err().kind,
            CapabilityErrorKind::PermissionDenied
        );
    }

    #[test]
    fn exists_answers_false_rather_than_failing() {
        let (_d, ctx) = fixture("filesystem.read=*");
        assert_eq!(exists(&ctx, "src/a.rs").expect("exists")["exists"], true);
        assert_eq!(exists(&ctx, "absent.rs").expect("exists")["exists"], false);
    }

    #[test]
    fn stat_reports_kind_and_size() {
        let (_d, ctx) = fixture("filesystem.read=*");
        let result = stat(&ctx, "src/a.rs").expect("stat");
        assert_eq!(result["kind"], "file");
        assert_eq!(result["size"], 3);
    }

    #[test]
    fn a_delete_emits_a_file_changed_event() {
        let (_d, ctx) = fixture("filesystem.delete=*");
        let mut events = ctx.events.subscribe();
        delete(&ctx, "src/a.rs").expect("delete");
        let saw = std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|f| match f.event {
                Event::FileChanged { change, .. } => Some(change),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(saw, vec![dex_protocol::FileChange::Deleted]);
    }
}
