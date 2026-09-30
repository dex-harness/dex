//! Git capabilities.
//!
//! Git is exposed as a small set of named operations rather than as a way to
//! run arbitrary git commands, because git's argument surface is large and
//! surprising: `--upload-pack`, `--config`, and `-c` all change what a command
//! does. Each capability here builds its own argv from validated inputs and
//! passes `--` before any path, so a branch name or path can never be read as
//! an option.

use serde_json::json;

use super::process::{self, ProcessOutcome};
use super::{CapabilityCtx, CapabilityError};
use crate::capability::CapabilityErrorKind;

/// Authority for read-only git operations.
pub const READ: &str = "git.read";
/// Authority for operations that change the working tree or history.
pub const WRITE: &str = "git.write";

/// `git.status()`
pub async fn status(ctx: &CapabilityCtx) -> Result<serde_json::Value, CapabilityError> {
    ctx.enter("git.status")?;
    ctx.authorize(READ, &ctx.working_dir().to_string_lossy())?;
    let outcome = process::run(ctx, "git", &args(&["status", "--porcelain=v1", "-z"]), default_limit(ctx)).await?;
    check(&outcome, "git.status")?;

    let mut entries = Vec::new();
    for record in outcome.stdout.split('\0').filter(|r| !r.is_empty()) {
        // Porcelain v1: two status chars, a space, then the path.
        if record.len() < 4 {
            continue;
        }
        let code = &record[..2];
        let path = record[3..].to_string();
        entries.push(json!({
            "path": path,
            "status": match code {
                " M" => "modified",
                "M " => "staged",
                "MM" => "staged_and_modified",
                "??" => "untracked",
                "A " => "added",
                "D " => "deleted",
                _ => "other",
            },
        }));
    }
    Ok(json!({ "entries": entries, "clean": entries.is_empty() }))
}

/// `git.diff(path?)`
pub async fn diff(ctx: &CapabilityCtx, path: Option<&str>) -> Result<serde_json::Value, CapabilityError> {
    ctx.enter("git.diff")?;
    ctx.authorize(READ, &ctx.working_dir().to_string_lossy())?;
    let mut argv = vec!["diff".to_string(), "--no-color".to_string(), "--unified=3".to_string()];
    if let Some(path) = path.filter(|p| !p.trim().is_empty()) {
        // Resolve first so a diff cannot name a path outside the session.
        let resolved = ctx.resolve_read(READ, path)?;
        argv.push("--".to_string());
        argv.push(resolved.display().to_string());
    }
    let outcome = process::run(ctx, "git", &argv, default_limit(ctx)).await?;
    check(&outcome, "git.diff")?;
    Ok(json!({
        "diff": outcome.stdout,
        "truncated": outcome.truncated,
    }))
}

/// `git.log(n?)` — one string per commit, most recent first.
pub async fn log(ctx: &CapabilityCtx, count: usize) -> Result<serde_json::Value, CapabilityError> {
    ctx.enter("git.log")?;
    ctx.authorize(READ, &ctx.working_dir().to_string_lossy())?;
    let count = count.clamp(1, 200);
    let outcome = process::run(
        ctx,
        "git",
        &args(&[
            "log",
            &format!("-{count}"),
            "--no-color",
            "--pretty=format:%h%x1f%an%x1f%s",
        ]),
        default_limit(ctx),
    )
    .await?;
    check(&outcome, "git.log")?;

    let commits: Vec<_> = outcome
        .stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let mut parts = line.split('\u{1f}');
            json!({
                "hash": parts.next().unwrap_or_default(),
                "author": parts.next().unwrap_or_default(),
                "subject": parts.next().unwrap_or_default(),
            })
        })
        .collect();
    Ok(json!({ "commits": commits, "truncated": outcome.truncated }))
}

/// `git.checkout(branch)` — the one mutating git capability.
pub async fn checkout(ctx: &CapabilityCtx, branch: &str) -> Result<serde_json::Value, CapabilityError> {
    ctx.enter("git.checkout")?;
    ctx.authorize(WRITE, &ctx.working_dir().to_string_lossy())?;

    let branch = branch.trim();
    validate_ref(branch)?;
    let outcome = process::run(
        ctx,
        "git",
        &args(&["checkout", "--quiet", branch]),
        default_limit(ctx),
    )
    .await?;
    check(&outcome, "git.checkout")?;
    Ok(json!({ "branch": branch, "ok": true }))
}

/// Reject anything that is not a plain ref name.
///
/// A branch name beginning with `-` would be parsed as an option, and
/// `..`/`@{` sequences have meaning to git's revision syntax. Both are the
/// kind of thing a model can produce accidentally while guessing, so they are
/// refused rather than passed through.
fn validate_ref(branch: &str) -> Result<(), CapabilityError> {
    if branch.is_empty() {
        return Err(CapabilityError::invalid("branch name is empty"));
    }
    if branch.starts_with('-') {
        return Err(CapabilityError::invalid(format!(
            "branch name {branch:?} starts with `-`, which git would read as an option"
        )));
    }
    if branch.contains("..") || branch.contains("@{") {
        return Err(CapabilityError::invalid(format!(
            "branch name {branch:?} contains git revision syntax"
        )));
    }
    if !branch
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.'))
    {
        return Err(CapabilityError::invalid(format!(
            "branch name {branch:?} contains unsupported characters"
        )));
    }
    Ok(())
}

fn default_limit(ctx: &CapabilityCtx) -> std::time::Duration {
    ctx.budget.limits().command
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Turn a non-zero exit into a capability error carrying the tool's own
/// message, so a model reading the error learns what actually went wrong.
fn check(outcome: &ProcessOutcome, what: &str) -> Result<(), CapabilityError> {
    if outcome.succeeded() {
        return Ok(());
    }
    let detail = if outcome.stderr.trim().is_empty() {
        outcome.stdout.trim()
    } else {
        outcome.stderr.trim()
    };
    Err(CapabilityError::new(
        CapabilityErrorKind::OperationFailed,
        format!("{what} failed: {detail}"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::Authority;
    use crate::budget::{BudgetMeter, ExecutionBudget};
    use crate::capability::PathGuard;
    use crate::events::EventSink;
    use crate::memory::MemoryStore;
    use dex_protocol::{CallId, SessionId};
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn repo(authority: &str) -> (tempfile::TempDir, CapabilityCtx) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("git")
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "dex@example.invalid"]);
        git(&["config", "user.name", "dex"]);
        std::fs::write(root.join("a.txt"), "one\n").expect("write");
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "first commit"]);
        std::fs::write(root.join("a.txt"), "two\n").expect("write");

        let ctx = CapabilityCtx::new(
            PathGuard::new(root).expect("guard"),
            Arc::new(Authority::parse(authority).expect("authority")),
            BudgetMeter::new(ExecutionBudget::default()),
            EventSink::new(SessionId::new()),
            Arc::new(MemoryStore::new(root.join(".dex-memory"))),
            CancellationToken::new(),
            CallId(1),
        );
        (dir, ctx)
    }

    #[tokio::test]
    async fn status_reports_uncommitted_changes() {
        let (_d, ctx) = repo("git.read=*");
        let result = status(&ctx).await.expect("status");
        assert_eq!(result["clean"], false);
        let entries = result["entries"].as_array().unwrap();
        assert!(entries.iter().any(|e| e["path"] == "a.txt" && e["status"] == "modified"));
    }

    #[tokio::test]
    async fn diff_returns_the_change() {
        let (_d, ctx) = repo("git.read=*");
        let result = diff(&ctx, None).await.expect("diff");
        let text = result["diff"].as_str().unwrap();
        assert!(text.contains("-one"), "got {text}");
        assert!(text.contains("+two"), "got {text}");
    }

    #[tokio::test]
    async fn log_recent_commits() {
        let (_d, ctx) = repo("git.read=*");
        let result = log(&ctx, 5).await.expect("log");
        let commits = result["commits"].as_array().unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0]["subject"], "first commit");
    }

    #[tokio::test]
    async fn read_capabilities_need_read_authority_only() {
        let (_d, ctx) = repo("git.write=*");
        assert_eq!(
            status(&ctx).await.unwrap_err().kind,
            CapabilityErrorKind::PermissionDenied
        );
    }

    #[tokio::test]
    async fn checkout_needs_write_authority() {
        let (_d, ctx) = repo("git.read=*");
        assert_eq!(
            checkout(&ctx, "main").await.unwrap_err().kind,
            CapabilityErrorKind::PermissionDenied
        );
    }

    #[tokio::test]
    async fn checkout_moves_the_working_tree() {
        let (_d, ctx) = repo("git.write=*");
        let result = checkout(&ctx, "main").await.expect("checkout");
        assert_eq!(result["ok"], true);
    }

    #[test]
    fn option_like_and_revision_syntax_refs_are_refused() {
        // These are the shapes that would let a model-supplied string change
        // what git actually does.
        for bad in ["--upload-pack=touch /tmp/pwn", "-x", "a..b", "a@{0}", "a b", "a;b"] {
            assert!(
                validate_ref(bad).is_err(),
                "{bad:?} should have been refused"
            );
        }
        for good in ["main", "feature/login", "v1.2.3", "release-2"] {
            assert!(validate_ref(good).is_ok(), "{good:?} should be accepted");
        }
    }
}
