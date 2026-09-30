//! Session event fan-out.
//!
//! The bus is a broadcast channel owned by a session. Producers emit; any number
//! of frontends may subscribe, and a subscriber that falls behind is told so
//! rather than being allowed to block the runtime.
//!
//! A connection is only ever a subscriber. Session state is owned elsewhere, so
//! dropping a connection drops a receiver and nothing else — which is what makes
//! "a client disconnect must not corrupt the session" true by construction
//! rather than by careful sequencing.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dex_protocol::{Event, EventFrame, SessionId};
use tokio::sync::broadcast;

/// Buffered events per session. Sized so a frontend that stops reading briefly
/// does not lose the turn, while a wedged frontend cannot grow memory without
/// bound.
const CHANNEL_CAPACITY: usize = 1024;

/// Cloneable handle used by everything that produces events.
#[derive(Clone)]
pub struct EventSink {
    session_id: SessionId,
    tx: broadcast::Sender<EventFrame>,
}

impl EventSink {
    /// Create a bus for a session. The returned value is the producer handle;
    /// call [`EventSink::subscribe`] for each consumer.
    pub fn new(session_id: SessionId) -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self { session_id, tx }
    }

    pub fn session_id(&self) -> SessionId {
        self.session_id
    }

    /// Publish an event. Never blocks and never fails: a session with no
    /// subscribers, or with subscribers that cannot keep up, still runs.
    pub fn emit(&self, event: Event) {
        let frame = EventFrame::new(self.session_id, now_millis(), event);
        // An `Err` here only means there are no receivers, which is normal for
        // a session nobody is watching. The send result is intentionally
        // ignored.
        let _ = self.tx.send(frame);
    }

    /// Subscribe. A session may have several subscribers at once, which is what
    /// allows a second frontend to `Attach`.
    pub fn subscribe(&self) -> broadcast::Receiver<EventFrame> {
        self.tx.subscribe()
    }

    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

impl std::fmt::Debug for EventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSink")
            .field("session_id", &self.session_id)
            .field("receivers", &self.tx.receiver_count())
            .finish()
    }
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// A bus shared across a runtime, so infrastructure outside any one session can
/// still report (for example, a rejected connection).
#[derive(Clone, Debug)]
pub struct RuntimeBus {
    inner: Arc<RuntimeBusInner>,
}

#[derive(Debug)]
struct RuntimeBusInner {
    tx: broadcast::Sender<EventFrame>,
}

impl RuntimeBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(RuntimeBusInner { tx }),
        }
    }

    pub fn emit(&self, session_id: SessionId, event: Event) {
        let _ = self
            .inner
            .tx
            .send(EventFrame::new(session_id, now_millis(), event));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EventFrame> {
        self.inner.tx.subscribe()
    }
}

impl Default for RuntimeBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dex_protocol::SessionStatus;
    use tokio::sync::broadcast::error::TryRecvError;

    #[test]
    fn events_reach_a_subscriber() {
        let sink = EventSink::new(SessionId::new());
        let mut rx = sink.subscribe();
        sink.emit(Event::ModelStarted);
        let frame = rx.try_recv().expect("event");
        assert!(matches!(frame.event, Event::ModelStarted));
        assert_eq!(frame.session_id, sink.session_id());
    }

    #[test]
    fn emitting_with_no_subscriber_is_not_an_error() {
        let sink = EventSink::new(SessionId::new());
        sink.emit(Event::ModelStarted);
        assert_eq!(sink.receiver_count(), 0);
    }

    #[test]
    fn several_frontends_can_subscribe_to_one_session() {
        let sink = EventSink::new(SessionId::new());
        let mut first = sink.subscribe();
        let mut second = sink.subscribe();
        sink.emit(Event::Answer {
            text: "done".into(),
        });
        assert!(matches!(first.try_recv().unwrap().event, Event::Answer { .. }));
        assert!(matches!(second.try_recv().unwrap().event, Event::Answer { .. }));
    }

    #[test]
    fn a_slow_subscriber_is_told_it_lagged_rather_than_blocking() {
        let sink = EventSink::new(SessionId::new());
        let mut rx = sink.subscribe();
        // Overflow the buffer.
        for _ in 0..(CHANNEL_CAPACITY + 10) {
            sink.emit(Event::ModelStarted);
        }
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Lagged(_))));
    }

    #[test]
    fn frames_carry_a_usable_timestamp() {
        let sink = EventSink::new(SessionId::new());
        let mut rx = sink.subscribe();
        sink.emit(Event::SessionFinished {
            status: SessionStatus::Completed,
        });
        let frame = rx.try_recv().expect("event");
        assert!(frame.ts_ms > 1_600_000_000_000, "ts_ms was {}", frame.ts_ms);
    }
}
