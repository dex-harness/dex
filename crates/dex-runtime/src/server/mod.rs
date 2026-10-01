//! The IPC server.
//!
//! A Unix socket carrying typed binary frames. A connection is a subscriber: it
//! forwards requests to the session manager and writes events back. It owns no
//! session state, so dropping a connection cannot corrupt a running turn.
//!
//! The socket is the only thing between a frontend and the runtime, and the
//! two-repository split exists to keep it that way.

pub mod dispatch;
pub mod frame;
pub mod listener;

pub use dispatch::Dispatcher;
pub use frame::{check_socket_path, Frame, FrameReader, FrameWriter};
pub use listener::{bind_socket, cleanup};

use std::sync::Arc;

use crate::session::SessionManager;

/// Serve until `shutdown` resolves.
///
/// Each accepted connection gets its own task, so a frontend that disappears
/// mid-turn costs nothing beyond its socket.
pub async fn serve(
    listener: tokio::net::UnixListener,
    manager: Arc<SessionManager>,
    shutdown: impl std::future::Future<Output = ()> + Send,
) {
    let dispatcher = Dispatcher::new(manager);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let dispatcher = dispatcher.clone();
                        tokio::spawn(async move {
                            dispatcher.handle(stream).await;
                        });
                    }
                    // A failed accept is not fatal: back off and try again, so
                    // a transient condition does not end the runtime.
                    Err(e) => {
                        tracing::warn!(error = %e, "could not accept a connection");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }
}