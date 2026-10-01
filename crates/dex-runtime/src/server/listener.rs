//! Binding the socket.

use std::io;
use std::path::{Path, PathBuf};

use crate::server::frame::check_socket_path;

/// Where the runtime listens, and whether it created the file.
#[derive(Debug)]
pub struct Bound {
    pub path: PathBuf,
    /// True when this call created the socket file, so the caller removes it on
    /// exit. False when an existing, live socket was adopted.
    pub created: bool,
}

/// Bind `path`, handling a socket left behind by a previous run.
///
/// Returns the listener alongside the path, so the caller can move the listener
/// into the serving task while keeping the path for cleanup on exit.
///
/// A stale socket is probed before being removed: if something is still
/// listening, the runtime refuses to start rather than stealing another
/// process's socket and leaving two runtimes fighting over one file.
pub fn bind_socket(path: &Path) -> io::Result<(tokio::net::UnixListener, Bound)> {
    check_socket_path(path)?;

    if path.exists() {
        if is_live(path) {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!(
                    "{} is already served by a running runtime",
                    path.display()
                ),
            ));
        }
        tracing::info!(path = %path.display(), "removing a stale socket");
        std::fs::remove_file(path)?;
    }

    if let Some(parent) = path.parent() {
        // The socket is reachable by anyone who can reach the directory, so the
        // directory is created private.
        std::fs::create_dir_all(parent)?;
        restrict_directory(parent)?;
    }

    let listener = tokio::net::UnixListener::bind(path)?;
    restrict_socket(path)?;
    Ok((
        listener,
        Bound {
            path: path.to_path_buf(),
            created: true,
        },
    ))
}

/// Whether something is still accepting on `path`.
fn is_live(path: &Path) -> bool {
    // Connecting succeeds only when something is still listening. Anything else
    // means the socket is stale.
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

fn restrict_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

fn restrict_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Remove a socket this process created.
pub fn cleanup(bound: &Bound) {
    if bound.created {
        let _ = std::fs::remove_file(&bound.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_stale_socket_is_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dex.sock");

        // A socket file with nothing listening is stale.
        let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        drop(listener);
        assert!(path.exists(), "the socket file should outlive its listener");

        let (_listener, bound) = bind_socket(&path).expect("stale socket should be replaced");
        assert!(bound.created);
        assert!(path.exists());
        cleanup(&bound);
        assert!(!path.exists(), "cleanup removes what it created");
    }

    #[tokio::test]
    async fn a_live_socket_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("dex.sock");
        let _live = std::os::unix::net::UnixListener::bind(&path).expect("bind");

        let err = bind_socket(&path).expect_err("must refuse a live socket");
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert!(err.to_string().contains("already served"), "got {err}");
    }

    #[tokio::test]
    async fn the_socket_is_not_world_accessible() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("dex.sock");
        let (_listener, bound) = bind_socket(&path).expect("bind");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o077, 0, "socket mode should be private, got {mode:o}");
        cleanup(&bound);
    }

    #[test]
    fn an_over_long_path_is_refused_before_binding() {
        let long = std::env::temp_dir().join("x".repeat(200));
        let err = bind_socket(&long).expect_err("must refuse");
        assert!(err.to_string().contains("under 100"), "got {err}");
    }
}