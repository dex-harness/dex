//! Turning requests into session work.
//!
//! A connection holds no state: it reads a request, hands it to the session
//! manager, and writes back events until the turn ends or the client goes away.
//! That is what makes a disconnect harmless — dropping a subscriber cancels
//! nothing, because the turn was never owned by the subscriber.

use std::sync::Arc;

use dex_protocol::{
    Ack, ClientRequest, ErrorKind, ErrorPayload, Event, EventFrame, RequestFrame, RequestId,
    ServerResponse,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::Mutex;

use crate::session::{SessionConfig, SessionManager};

use super::frame::{Frame, FrameReader, FrameWriter};

#[derive(Clone)]
pub struct Dispatcher {
    manager: Arc<SessionManager>,
    /// Guards the "subscribe, then acknowledge" pair. Without it a fast client
    /// could miss events emitted between the two.
    subscribe_gate: Arc<Mutex<()>>,
}

impl Dispatcher {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            manager,
            subscribe_gate: Arc::new(Mutex::new(())),
        }
    }

    /// Drive one connection until it closes.
    pub async fn handle<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        let mut reader = FrameReader::new(reader);
        let mut writer = FrameWriter::new(writer);

        // Sessions this connection is watching, and the task pumping their
        // events into the socket.
        let mut attached: Option<Attached> = None;

        loop {
            tokio::select! {
                // Events for whatever session this connection is watching. The
                // future only completes when there is something to send.
                result = forward(&mut attached, &mut writer) => {
                    if result.is_err() {
                        // The client is gone or the socket failed; there is
                        // nothing useful left to do for it.
                        break;
                    }
                }
                incoming = reader.next::<RequestFrame>() => {
                    match incoming {
                        Ok(Frame::Complete(request)) => {
                            if !self.respond(request, &mut attached, &mut writer).await {
                                break;
                            }
                        }
                        Ok(Frame::Closed) => break,
                        Err(e) => {
                            // A malformed frame is reported and the connection
                            // ends: the stream position is no longer trustworthy.
                            let _ = writer
                                .send(&ServerResponse::err(
                                    RequestId(0),
                                    ErrorPayload::new(ErrorKind::Ipc, e.to_string()),
                                ))
                                .await;
                            break;
                        }
                    }
                }
            }
        }

        if let Some(attached) = attached {
            // Unsubscribe without cancelling: the turn keeps running for any
            // other subscriber, and for the session's own state.
            drop(attached);
        }
    }

    /// Handle one request. Returns false when the connection should close.
    async fn respond<W>(
        &self,
        request: RequestFrame,
        attached: &mut Option<Attached>,
        writer: &mut FrameWriter<W>,
    ) -> bool
    where
        W: AsyncWrite + Unpin,
    {
        let id = request.id;
        match request.request {
            ClientRequest::CreateSession { working_dir, model } => {
                match self
                    .manager
                    .create(SessionConfig {
                        working_dir: working_dir.into(),
                        model,
                    })
                    .await
                {
                    Ok(session) => {
                        let response = ServerResponse::ack(
                            id,
                            Ack::CreateSession {
                                session_id: session.id,
                                status: session.status(),
                                model: session.model.clone(),
                            },
                        );
                        if writer.send(&response).await.is_err() {
                            return false;
                        }
                        *attached = Some(Attached::new(&session));
                        true
                    }
                    Err(e) => {
                        let _ = writer.send(&ServerResponse::err(id, e.payload())).await;
                        true
                    }
                }
            }

            ClientRequest::SendMessage { session_id, text } => {
                let Some(session) = self.manager.get(session_id).await else {
                    let _ = writer
                        .send(&ServerResponse::err(id, unknown_session(session_id)))
                        .await;
                    return true;
                };

                // Re-subscribe before acknowledging, so the acknowledgement is
                // never observed before the events it precedes.
                let _gate = self.subscribe_gate.lock().await;
                drop(attached.replace(Attached::new(&session)));

                if writer.send(&ServerResponse::ack(id, Ack::Accepted)).await.is_err() {
                    return false;
                }
                drop(_gate);

                let manager = self.manager.clone();
                // The turn runs on its own task so the connection can keep
                // reading: a frontend that sends a second request, or
                // disconnects, must not stop the work already under way.
                tokio::spawn(async move {
                    let status = manager.run_turn(session, text).await;
                    tracing::debug!(%session_id, ?status, "turn finished");
                });
                true
            }

            ClientRequest::Attach { session_id } => {
                let Some(session) = self.manager.get(session_id).await else {
                    let _ = writer
                        .send(&ServerResponse::err(id, unknown_session(session_id)))
                        .await;
                    return true;
                };
                *attached = Some(Attached::new(&session));
                let _ = writer.send(&ServerResponse::ack(id, Ack::Ok)).await;
                true
            }

            ClientRequest::Cancel { session_id } => {
                let Some(session) = self.manager.get(session_id).await else {
                    let _ = writer
                        .send(&ServerResponse::err(id, unknown_session(session_id)))
                        .await;
                    return true;
                };
                // Cancelling is idempotent and safe from any state.
                session.cancel.cancel();
                let _ = writer.send(&ServerResponse::ack(id, Ack::Ok)).await;
                true
            }

            ClientRequest::CloseSession { session_id } => {
                match self.manager.close(session_id).await {
                    Ok(()) => {
                        *attached = None;
                        let _ = writer.send(&ServerResponse::ack(id, Ack::Closed)).await;
                    }
                    Err(e) => {
                        let _ = writer.send(&ServerResponse::err(id, e.payload())).await;
                    }
                }
                true
            }

            ClientRequest::ListCapabilities => {
                let ack = crate::session::granted_for(self.manager.authority());
                let _ = writer.send(&ServerResponse::ack(id, ack)).await;
                true
            }
        }
    }
}

/// The session a connection is watching.
struct Attached {
    events: tokio::sync::broadcast::Receiver<EventFrame>,
}

impl Attached {
    fn new(session: &crate::session::Session) -> Self {
        Self {
            events: session.events.subscribe(),
        }
    }
}

/// Write the next event for the attached session.
///
/// Returns a future that never completes when nothing is attached, so the
/// connection's select waits on the reader alone instead of waking on a timer.
async fn forward<W>(attached: &mut Option<Attached>, writer: &mut FrameWriter<W>) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let Some(attached) = attached.as_mut() else {
        return std::future::pending().await;
    };
    let frame = attached.events.recv().await;
    match frame {
        Ok(frame) => writer.send(&ServerResponse::Event(frame)).await,
        Err(RecvError::Closed) => {
            // The session's bus went away, which means the runtime is shutting
            // down. Closing the connection is the honest response.
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "the runtime is shutting down"))
        }
        Err(RecvError::Lagged(missed)) => {
            // A slow client is told rather than allowed to block the runtime or
            // silently miss part of the turn.
            writer
                    .send(&ServerResponse::Event(EventFrame::new(
                        dex_protocol::SessionId::new(),
                        crate::events::now_millis(),
                        Event::Error {
                            error: ErrorPayload::new(
                                ErrorKind::Ipc,
                                format!("this client fell behind and missed {missed} events"),
                            ),
                        },
                    )))
                    .await
        }
    }
}

fn unknown_session(session_id: dex_protocol::SessionId) -> ErrorPayload {
    ErrorPayload::new(ErrorKind::Invalid, format!("no such session: {session_id}"))
}

use std::io;