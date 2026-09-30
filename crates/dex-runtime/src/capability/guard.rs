//! Working-directory containment.
//!
//! Every filesystem-shaped capability resolves its paths through a `PathGuard`.
//! The guard answers one question: does this path, after all symlinks are
//! resolved, still live inside the session's working directory?
//!
//! The check is done on the *canonicalized* path rather than the textual one,
//! which is what makes symlink escapes fail. A textual `starts_with` check would
//! happily accept `root/link` where `link` points at `/etc`, and would reject
//! a legitimate path reached through a symlink that stays inside the root.
//!
//! This is a containment guarantee, not a sandbox. A granted capability can
//! still do whatever its implementation does; what it cannot do is name a path
//! outside the session root.

use std::path::{Path, PathBuf};

use dex_protocol::CapabilityErrorKind;

use super::CapabilityError;

/// A canonicalized working directory that paths are resolved against.
#[derive(Clone, Debug)]
pub struct PathGuard {
    root: PathBuf,
}

impl PathGuard {
    /// Create a guard for `root`, canonicalizing it once up front.
    ///
    /// Canonicalizing at construction means every later check compares two
    /// already-canonical paths, so no per-call `canonicalize` on the root is
    /// needed and a root that later becomes a symlink cannot move the boundary.
    pub fn new(root: &Path) -> Result<Self, CapabilityError> {
        let canonical = root.canonicalize().map_err(|e| {
            CapabilityError::new(
                CapabilityErrorKind::InvalidArgument,
                format!("working directory {} is unusable: {e}", root.display()),
            )
        })?;
        Ok(Self { root: canonical })
    }

    /// The canonical session root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a path that is expected to exist, then confirm containment.
    pub fn resolve_existing(&self, raw: &str) -> Result<PathBuf, CapabilityError> {
        let joined = self.join(raw);
        let canonical = joined.canonicalize().map_err(|e| {
            CapabilityError::new(
                CapabilityErrorKind::ResourceNotFound,
                format!("{}: {e}", joined.display()),
            )
        })?;
        self.contain(&canonical, raw)?;
        Ok(canonical)
    }

    /// Resolve a path that may not exist yet, as for a create or an edit.
    ///
    /// When the target exists it is canonicalized directly, so writing *through*
    /// a symlink out of the tree is rejected. When it does not exist, the
    /// nearest existing ancestor is canonicalized instead: a new file may name
    /// parent directories that do not exist yet, and the containment check has
    /// to anchor on something that does. The remaining components are then
    /// re-appended, so `a/b/c.txt` resolves correctly whether or not `a/b`
    /// exists, and an escape via `..` is still caught at the anchor.
    pub fn resolve_for_write(&self, raw: &str) -> Result<PathBuf, CapabilityError> {
        let joined = self.join(raw);
        if joined.exists() {
            return self.resolve_existing(raw);
        }

        // Climb from the full path up to the closest component that exists,
        // remembering each one so the result can be rebuilt downwards.
        let mut anchor: &Path = &joined;
        let mut trailing: Vec<std::ffi::OsString> = Vec::new();
        let canonical_anchor = loop {
            match anchor.canonicalize() {
                Ok(canonical) => break canonical,
                Err(_) => {
                    let (Some(name), Some(parent)) = (anchor.file_name(), anchor.parent()) else {
                        return Err(CapabilityError::new(
                            CapabilityErrorKind::InvalidArgument,
                            format!(
                                "{} does not name a path under the working directory",
                                joined.display()
                            ),
                        ));
                    };
                    trailing.push(name.to_os_string());
                    anchor = parent;
                }
            }
        };

        // Containment is decided at the anchor: this is what catches a `..`
        // climb, because canonicalizing `root/../..` lands outside the root.
        self.contain(&canonical_anchor, raw)?;

        let mut resolved = canonical_anchor;
        for name in trailing.iter().rev() {
            if name == ".." {
                return Err(CapabilityError::new(
                    CapabilityErrorKind::PermissionDenied,
                    format!("{raw:?} resolves outside the session working directory"),
                ));
            }
            resolved.push(name);
        }
        Ok(resolved)
    }

    /// Join against the root. A leading `/` is *not* treated as absolute here:
    /// the root is the session's world, and a model naming an absolute path
    /// almost always means "relative to the repository".
    fn join(&self, raw: &str) -> PathBuf {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return self.root.clone();
        }
        let candidate = Path::new(trimmed);
        let relative = candidate.strip_prefix("/").unwrap_or(candidate);
        if relative.as_os_str().is_empty() {
            self.root.clone()
        } else {
            self.root.join(relative)
        }
    }

    fn contain(&self, canonical: &Path, raw: &str) -> Result<(), CapabilityError> {
        if canonical.starts_with(&self.root) {
            return Ok(());
        }
        Err(CapabilityError::new(
            CapabilityErrorKind::PermissionDenied,
            format!(
                "{raw:?} resolves to {} which is outside the session working directory {}",
                canonical.display(),
                self.root.display()
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn guard(root: &Path) -> PathGuard {
        PathGuard::new(root).expect("guard")
    }

    #[test]
    fn a_relative_path_resolves_inside_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        fs::write(dir.path().join("src/a.rs"), "fn a() {}").expect("write");

        let guard = guard(dir.path());
        let resolved = guard.resolve_existing("src/a.rs").expect("resolve");
        assert_eq!(resolved, dir.path().join("src/a.rs").canonicalize().unwrap());
    }

    #[test]
    fn an_absolute_path_naming_an_inside_path_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.rs"), "x").expect("write");
        let guard = guard(dir.path());
        assert!(guard.resolve_existing("/a.rs").is_ok());
    }

    #[test]
    fn dot_dot_traversal_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guard = guard(dir.path());
        let err = guard
            .resolve_existing("../../../etc/passwd")
            .expect_err("must reject");
        assert_eq!(err.kind, CapabilityErrorKind::PermissionDenied);
    }

    #[test]
    fn a_symlink_pointing_outside_the_root_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        let secret = outside.path().join("secret.txt");
        fs::write(&secret, "classified").expect("write");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, dir.path().join("link")).expect("symlink");
        #[cfg(unix)]
        {
            let guard = guard(dir.path());
            // The textual path is inside the root; only canonicalization reveals
            // the escape. This is the case a naive prefix check gets wrong.
            let err = guard.resolve_existing("link").expect_err("must reject");
            assert_eq!(err.kind, CapabilityErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn a_symlink_staying_inside_the_root_is_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("real")).expect("mkdir");
        fs::write(dir.path().join("real/a.rs"), "x").expect("write");

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link"))
                .expect("symlink");
            let guard = guard(dir.path());
            assert!(guard.resolve_existing("link/a.rs").is_ok());
        }
    }

    #[test]
    fn a_missing_file_is_not_found_rather_than_denied() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guard = guard(dir.path());
        let err = guard.resolve_existing("nope.rs").expect_err("must fail");
        assert_eq!(err.kind, CapabilityErrorKind::ResourceNotFound);
    }

    #[test]
    fn a_new_file_may_be_written_when_its_parent_is_inside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guard = guard(dir.path());
        let resolved = guard.resolve_for_write("new.rs").expect("resolve");
        assert_eq!(resolved, dir.path().join("new.rs"));
    }

    #[test]
    fn a_new_file_may_not_be_written_through_an_escaping_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guard = guard(dir.path());
        // `resolve_for_write` canonicalizes the parent, so a traversal that
        // climbs out of the root is caught even though the file does not exist
        // yet.
        let err = guard
            .resolve_for_write("../../planted.rs")
            .expect_err("must reject");
        assert_eq!(err.kind, CapabilityErrorKind::PermissionDenied);
    }

    #[test]
    fn an_absolute_path_is_read_as_root_relative() {
        // A model naming `/src/auth.rs` means "from the repository root", not
        // "from the filesystem root". Pinning this because it is a deliberate
        // choice: it means a model cannot reach outside by prefixing a path
        // with `/`, and it keeps the common case of writing what looks like an
        // absolute path working.
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("src")).expect("mkdir");
        fs::write(dir.path().join("src/auth.rs"), "x").expect("write");

        let guard = guard(dir.path());
        assert_eq!(
            guard.resolve_existing("/src/auth.rs").expect("resolve"),
            dir.path().join("src/auth.rs").canonicalize().unwrap()
        );
    }

    #[test]
    fn writing_through_a_symlink_out_of_the_tree_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("outside");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), dir.path().join("escape"))
                .expect("symlink");
            let guard = guard(dir.path());
            let err = guard
                .resolve_for_write("escape/planted.rs")
                .expect_err("must reject");
            assert_eq!(err.kind, CapabilityErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn an_empty_path_means_the_root_itself() {
        let dir = tempfile::tempdir().expect("tempdir");
        let guard = guard(dir.path());
        assert_eq!(guard.resolve_existing("").expect("root"), guard.root());
    }
}
