//! Repository capabilities: search, read, and list.
//!
//! These are the read-only half of the capability surface. Every path goes
//! through [`CapabilityCtx`], so containment, authority, and the budget are
//! already settled by the time an implementation here runs.
//!
//! Search walks with the `ignore` crate, so `.gitignore` is respected and a
//! search does not wander into `target/` or `node_modules/`. That matters for
//! both cost and quality: a model asking where authentication lives wants
//! source, not build output.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::{CapabilityCtx, CapabilityError};
use crate::capability::CapabilityErrorKind;

/// Bytes scanned before a file is assumed to be binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Largest number of lines a single read returns by default.
const DEFAULT_READ_LINES: usize = 2000;

/// `repo.find(pattern, path?, glob?, max_results?)`
///
/// Returns an array of `{path, line, text}`, ordered by path then line.
pub fn find(
    ctx: &CapabilityCtx,
    pattern: &str,
    path: &str,
    glob: Option<&str>,
    max_results: usize,
    case_sensitive: bool,
) -> Result<serde_json::Value, CapabilityError> {
    ctx.enter("repo.find")?;
    let root = if path.trim().is_empty() {
        ctx.guard.root().to_path_buf()
    } else {
        ctx.resolve_read("filesystem.read", path)?
    };
    if !root.is_dir() {
        return Err(CapabilityError::new(
            CapabilityErrorKind::InvalidArgument,
            format!("{} is not a directory", root.display()),
        ));
    }

    let needle = if case_sensitive {
        pattern.to_string()
    } else {
        pattern.to_lowercase()
    };
    let mut matches: Vec<serde_json::Value> = Vec::new();
    let mut scanned = 0usize;
    let mut truncated = false;

    'walk: for entry in walk(root, glob) {
        let path = match entry {
            Ok(p) => p,
            // An unreadable directory should not abort the whole search; the
            // model asked a broad question and partial answers are useful.
            Err(_) => continue,
        };
        if !path.is_file() {
            continue;
        }
        let Ok(bytes) = ctx.read_file(&path) else {
            continue;
        };
        scanned += 1;
        if looks_binary(&bytes) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        for (index, line) in text.lines().enumerate() {
            let haystack = if case_sensitive {
                line.to_string()
            } else {
                line.to_lowercase()
            };
            if !haystack.contains(&needle) {
                continue;
            }
            if matches.len() >= max_results {
                truncated = true;
                break 'walk;
            }
            matches.push(json!({
                "path": relative_to(ctx, &path),
                "line": index + 1,
                "text": line.trim_end().chars().take(400).collect::<String>(),
            }));
        }
        if scanned >= 20_000 {
            truncated = true;
            break;
        }
    }

    Ok(json!({
        "matches": matches,
        "truncated": truncated,
        "files_scanned": scanned,
    }))
}

/// `repo.read(path, offset?, limit?)`
///
/// `offset` is a 1-based line number. The result carries enough metadata for a
/// program to page through a long file without re-reading it.
pub fn read(
    ctx: &CapabilityCtx,
    path: &str,
    offset: usize,
    limit: usize,
) -> Result<serde_json::Value, CapabilityError> {
    ctx.enter("repo.read")?;
    let resolved = ctx.resolve_read("filesystem.read", path)?;
    let bytes = ctx.read_file(&resolved)?;

    if looks_binary(&bytes) {
        return Err(CapabilityError::new(
            CapabilityErrorKind::InvalidArgument,
            format!(
                "{} looks like a binary file; use repo.find or dex.git instead",
                relative_to(ctx, &resolved)
            ),
        ));
    }

    let text = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = text.lines().collect();
    let total = all.len();
    let start = offset.saturating_sub(1).min(total);
    let take = if limit == 0 { DEFAULT_READ_LINES } else { limit };
    let end = (start + take).min(total);
    let body: String = all[start..end].iter().map(|l| format!("{l}\n")).collect();

    let mut body = body.into_bytes();
    let truncated = ctx.budget.clamp_output(&mut body);
    Ok(json!({
        "path": relative_to(ctx, &resolved),
        "content": String::from_utf8_lossy(&body),
        "start_line": start + 1,
        "end_line": end,
        "total_lines": total,
        "truncated": truncated,
        "binary": false,
    }))
}

/// `repo.list(path?, depth?, glob?)`
pub fn list(
    ctx: &CapabilityCtx,
    path: &str,
    depth: usize,
    glob: Option<&str>,
) -> Result<serde_json::Value, CapabilityError> {
    ctx.enter("repo.list")?;
    let root = if path.trim().is_empty() {
        ctx.guard.root().to_path_buf()
    } else {
        ctx.resolve_read("filesystem.read", path)?
    };
    if !root.is_dir() {
        return Err(CapabilityError::new(
            CapabilityErrorKind::InvalidArgument,
            format!("{} is not a directory", root.display()),
        ));
    }

    let depth = depth.clamp(1, 8);
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut truncated = false;
    for found in walk(root, glob) {
        if entries.len() >= 2000 {
            truncated = true;
            break;
        }
        let Ok(found) = found else { continue };
        let relative = relative_to(ctx, &found);
        // A depth-limited walk still descends; trim to the requested depth here
        // so the result is exactly what was asked for.
        if depth_of(ctx, &found) > depth {
            continue;
        }
        let kind = if found.is_dir() {
            "dir"
        } else {
            "file"
        };
        let size = if found.is_file() {
            found.metadata().map(|m| m.len()).unwrap_or(0)
        } else {
            0
        };
        entries.push(json!({ "path": relative, "kind": kind, "size": size }));
    }
    entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));

    Ok(json!({ "entries": entries, "truncated": truncated }))
}

/// Walk `root`, honouring ignore files and an optional glob filter.
fn walk(
    root: PathBuf,
    glob: Option<&str>,
) -> impl Iterator<Item = Result<PathBuf, ignore::Error>> {
    let mut builder = ignore::WalkBuilder::new(&root);
    builder
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .parents(true)
        // `ignore` only honours `.gitignore` inside a git repository by
        // default. A session working directory need not be a checkout, and
        // ignoring that would let a search wander into `target/` in exactly
        // the directories an ignore file exists to exclude.
        .require_git(false);
    if let Some(pattern) = glob.filter(|g| !g.is_empty()) {
        if let Ok(glob) = globset::Glob::new(pattern) {
            let matcher = glob.compile_matcher();
            builder.filter_entry(move |entry| matcher.is_match(entry.path()));
        }
    }
    builder
        .build()
        // The walk root itself has depth 0 and is not an entry of interest;
        // skipping it keeps an empty relative path out of every result.
        .filter(|entry| !matches!(entry, Ok(dir) if dir.depth() == 0))
        .map(|entry| entry.map(|d| d.into_path()))
}

fn looks_binary(bytes: &[u8]) -> bool {
    let window = bytes.len().min(BINARY_SNIFF_BYTES);
    bytes[..window].contains(&0)
}

fn relative_to(ctx: &CapabilityCtx, path: &Path) -> String {
    path.strip_prefix(ctx.guard.root())
        .unwrap_or(path)
        .display()
        .to_string()
}

fn depth_of(ctx: &CapabilityCtx, path: &Path) -> usize {
    path.strip_prefix(ctx.guard.root())
        .map(|p| p.components().count())
        .unwrap_or(0)
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

    fn fixture() -> (tempfile::TempDir, CapabilityCtx) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        std::fs::write(dir.path().join("src/auth.rs"), "fn login() {}\nfn logout() {}\n").expect("write");
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").expect("write");
        std::fs::write(dir.path().join(".gitignore"), "target/\n").expect("write");
        std::fs::create_dir_all(dir.path().join("target/debug")).expect("mkdir");
        std::fs::write(dir.path().join("target/debug/junk.rs"), "fn login() {}\n").expect("write");

        let ctx = CapabilityCtx::new(
            PathGuard::new(dir.path()).expect("guard"),
            Arc::new(Authority::parse("filesystem.read=*").expect("authority")),
            BudgetMeter::new(ExecutionBudget::default()),
            EventSink::new(SessionId::new()),
            Arc::new(MemoryStore::new(dir.path().join("memory"))),
            CancellationToken::new(),
            CallId(1),
        );
        (dir, ctx)
    }

    #[test]
    fn find_locates_matches_with_positions() {
        let (_d, ctx) = fixture();
        let result = find(&ctx, "login", "", None, 100, true).expect("find");
        let matches = result["matches"].as_array().expect("array");
        assert_eq!(matches.len(), 1, "got {matches:?}");
        assert_eq!(matches[0]["path"], "src/auth.rs");
        assert_eq!(matches[0]["line"], 1);
        assert!(matches[0]["text"].as_str().unwrap().contains("login"));
    }

    #[test]
    fn find_respects_gitignore() {
        let (_d, ctx) = fixture();
        let result = find(&ctx, "login", "", None, 100, true).expect("find");
        // target/ is gitignored, so the copy there must not appear.
        let paths: Vec<_> = result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(paths, vec!["src/auth.rs".to_string()]);
        assert!(!paths.iter().any(|p| p.contains("target")));
    }

    #[test]
    fn find_is_case_insensitive_by_default() {
        let (_d, ctx) = fixture();
        assert!(find(&ctx, "LOGIN", "", None, 100, false).expect("find")["matches"]
            .as_array()
            .unwrap()
            .len()
            == 1);
        assert!(find(&ctx, "LOGIN", "", None, 100, true).expect("find")["matches"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn find_reports_truncation_rather_than_silently_stopping() {
        let (_d, ctx) = fixture();
        let result = find(&ctx, "fn", "", None, 1, true).expect("find");
        assert_eq!(result["truncated"], true);
        assert_eq!(result["matches"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn read_returns_content_with_paging_metadata() {
        let (_d, ctx) = fixture();
        let result = read(&ctx, "src/auth.rs", 1, 0).expect("read");
        assert!(result["content"].as_str().unwrap().contains("fn login"));
        assert_eq!(result["total_lines"], 2);
        assert_eq!(result["start_line"], 1);
        assert_eq!(result["end_line"], 2);
        assert_eq!(result["binary"], false);
    }

    #[test]
    fn read_pages_from_an_offset() {
        let (_d, ctx) = fixture();
        let result = read(&ctx, "src/auth.rs", 2, 1).expect("read");
        assert_eq!(result["start_line"], 2);
        assert!(result["content"].as_str().unwrap().contains("logout"));
        assert!(!result["content"].as_str().unwrap().contains("login"));
    }

    #[test]
    fn read_refuses_binary_files_instead_of_dumping_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("blob.bin"), [0u8, 1, 2, 3]).expect("write");
        let ctx = CapabilityCtx::new(
            PathGuard::new(dir.path()).expect("guard"),
            Arc::new(Authority::parse("filesystem.read=*").expect("authority")),
            BudgetMeter::new(ExecutionBudget::default()),
            EventSink::new(SessionId::new()),
            Arc::new(MemoryStore::new(dir.path().join("memory"))),
            CancellationToken::new(),
            CallId(1),
        );
        let err = read(&ctx, "blob.bin", 1, 0).expect_err("must refuse");
        assert_eq!(err.kind, CapabilityErrorKind::InvalidArgument);
    }

    #[test]
    fn read_of_a_missing_file_is_not_found() {
        let (_d, ctx) = fixture();
        assert_eq!(
            read(&ctx, "src/absent.rs", 1, 0).unwrap_err().kind,
            CapabilityErrorKind::ResourceNotFound
        );
    }

    #[test]
    fn list_is_depth_limited_and_sorted() {
        let (_d, ctx) = fixture();
        let result = list(&ctx, "", 1, None).expect("list");
        let entries = result["entries"].as_array().expect("array");
        // Depth 1 means direct children only, and target/ stays hidden.
        let paths: Vec<_> = entries
            .iter()
            .map(|e| e["path"].as_str().unwrap().to_string())
            .collect();
        assert!(paths.contains(&"src".to_string()), "got {paths:?}");
        assert!(!paths.iter().any(|p| p.contains("target")));
    }

    #[test]
    fn a_deeper_walk_reaches_nested_files() {
        let (_d, ctx) = fixture();
        let result = list(&ctx, "", 3, None).expect("list");
        let paths: Vec<_> = result["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap().to_string())
            .collect();
        assert!(paths.contains(&"src/auth.rs".to_string()), "got {paths:?}");
    }

    #[test]
    fn a_glob_filters_the_walk() {
        let (_d, ctx) = fixture();
        let result = list(&ctx, "", 3, Some("*.rs")).expect("list");
        let paths: Vec<_> = result["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap().to_string())
            .collect();
        assert!(paths.iter().all(|p| p.ends_with(".rs")), "got {paths:?}");
    }

    #[test]
    fn capabilities_deny_without_authority() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.rs"), "x").expect("write");
        let ctx = CapabilityCtx::new(
            PathGuard::new(dir.path()).expect("guard"),
            Arc::new(Authority::deny_all()),
            BudgetMeter::new(ExecutionBudget::default()),
            EventSink::new(SessionId::new()),
            Arc::new(MemoryStore::new(dir.path().join("memory"))),
            CancellationToken::new(),
            CallId(1),
        );
        assert_eq!(
            read(&ctx, "a.rs", 1, 0).unwrap_err().kind,
            CapabilityErrorKind::PermissionDenied
        );
    }
}
