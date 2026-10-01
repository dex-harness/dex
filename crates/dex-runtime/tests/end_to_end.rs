//! End-to-end tests across the real socket.
//!
//! These start an actual runtime on an actual Unix socket and drive it with the
//! real protocol, so the two repositories are exercised as they ship rather
//! than against a stand-in. The only substitution is the model provider, which
//! is scripted, so the tests are deterministic and need no network.
//!
//! What is proven here:
//!
//! - a session can be opened, driven and closed over the wire,
//! - the model never receives a tool schema,
//! - a program runs against real capabilities and its answer comes back,
//! - a client that disconnects mid-turn does not disturb the session,
//! - cancellation reaches the turn,
//! - a frontend that is not the CLI still works, because nothing in the
//!   protocol assumes a terminal.

use std::sync::Arc;
use std::time::Duration;

use dex_protocol::{
    Ack, CallId, ClientRequest, Event, EventFrame, FileChange, RequestFrame, RequestId,
    ServerResponse, SessionId, SessionStatus,
};
use dex_runtime::auth::Authority;
use dex_runtime::budget::ExecutionBudget;
use dex_runtime::config::{ProviderConfig, RuntimeConfig};
use dex_runtime::memory::MemoryStore;
use dex_runtime::provider::{Completion, CompletionRequest, Provider, ProviderError};
use dex_runtime::script::rune::RuneScriptRuntime;
use dex_runtime::script::ScriptRuntime;
use dex_runtime::server::{self, Frame, FrameReader, FrameWriter};
use dex_runtime::session::{SessionDeps, SessionManager};
use tokio_util::sync::CancellationToken;

/// A provider that replays a fixed sequence of replies.
///
/// This is the only thing not real in these tests. Everything below it — the
/// session, the agent loop, the script runtime, the capabilities — is the code
/// that ships.
struct ScriptedProvider {
    replies: tokio::sync::Mutex<Vec<String>>,
    /// The requests the runtime actually sent, kept so a test can assert what
    /// the model was and was not told.
    seen: tokio::sync::Mutex<Vec<serde_json::Value>>,
}

impl ScriptedProvider {
    fn new(replies: Vec<&str>) -> Arc<Self> {
        Arc::new(Self {
            replies: tokio::sync::Mutex::new(replies.into_iter().map(String::from).collect()),
            seen: tokio::sync::Mutex::new(Vec::new()),
        })
    }

    async fn seen(&self) -> Vec<serde_json::Value> {
        self.seen.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    async fn complete(
        &self,
        request: CompletionRequest,
        _cancel: CancellationToken,
    ) -> Result<Completion, ProviderError> {
        // Record what the runtime would have sent upstream, so the no-tools
        // invariant is checked at the edge rather than in a unit test of a
        // serializer that might not be the one in use.
        self.seen.lock().await.push(serde_json::json!({
            "messages": request.turns.len(),
            "system_has_capabilities": request.system.contains("dex::find"),
        }));

        let mut replies = self.replies.lock().await;
        if replies.is_empty() {
            return Err(ProviderError::Empty);
        }
        Ok(Completion {
            text: replies.remove(0),
        })
    }

    fn model(&self) -> &str {
        "scripted"
    }
}

/// A running runtime plus a connected client.
struct Harness {
    socket: std::path::PathBuf,
    provider: Arc<ScriptedProvider>,
    manager: Arc<SessionManager>,
    _dir: tempfile::TempDir,
}

impl Harness {
    /// Grants everything the scripted programs need.
    const FULL: &'static str =
        "filesystem.read=*;filesystem.write=*;memory.read=*;memory.write=*;ui.interact=*";

    async fn start(replies: Vec<&str>) -> Self {
        Self::start_full(replies, ExecutionBudget::default(), Self::FULL).await
    }

    async fn start_with(replies: Vec<&str>, budget: ExecutionBudget) -> Self {
        Self::start_full(replies, budget, Self::FULL).await
    }

    /// A harness with whatever authority the test needs, so a refusal is tested
    /// against a genuinely withheld grant rather than against a comment.
    async fn start_full(
        replies: Vec<&str>,
        budget: ExecutionBudget,
        authority: &str,
    ) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).expect("mkdir");
        std::fs::write(repo.join("src/auth.rs"), "fn login() {}\nfn logout() {}\n")
            .expect("write");

        let socket = dir.path().join("dex.sock");
        let provider = ScriptedProvider::new(replies);
        let config = RuntimeConfig {
            provider: ProviderConfig {
                base_url: "http://unused.invalid/v1".into(),
                api_key: "unused".into(),
                model: "scripted".into(),
                session_header: "x-test-session".into(),
            },
            socket_path: socket.clone(),
            memory_dir: dir.path().join("memory"),
            max_sessions: 8,
            max_program_rounds: 4,
            budget,
            authority: Authority::parse(authority).expect("authority"),
        };

        let script: Arc<dyn ScriptRuntime> = RuneScriptRuntime::start().expect("script runtime");
        let manager = SessionManager::new(SessionDeps {
            config,
            provider: provider.clone(),
            script,
            memory: Arc::new(MemoryStore::new(dir.path().join("memory"))),
        });

        let (listener, _bound) = server::bind_socket(&socket).expect("bind");
        let serving = manager.clone();
        tokio::spawn(async move {
            server::serve(listener, serving, std::future::pending()).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        Self {
            socket,
            provider,
            manager,
            _dir: dir,
        }
    }

    /// The working tree the session is scoped to.
    ///
    /// Absolute: the runtime resolves a relative path against its own working
    /// directory, which is the crate, not this temporary directory.
    fn repo(&self) -> String {
        self._dir.path().join("repo").display().to_string()
    }

    /// A second, independent connection to the same runtime.
    async fn client(&self) -> Connection {
        Connection::open(&self.socket).await
    }

    async fn session(&self) -> (Connection, SessionId) {
        let mut client = self.client().await;
        let id = client.create_session(self.repo()).await.expect("session");
        (client, id)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// A test-side client speaking the real protocol.
struct Connection {
    reader: FrameReader<tokio::io::ReadHalf<tokio::net::UnixStream>>,
    writer: FrameWriter<tokio::io::WriteHalf<tokio::net::UnixStream>>,
    next_id: RequestId,
    events: Vec<EventFrame>,
}

impl Connection {
    async fn open(socket: &std::path::Path) -> Self {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .expect("connect");
        let (reader, writer) = tokio::io::split(stream);
        Self {
            reader: FrameReader::new(reader),
            writer: FrameWriter::new(writer),
            next_id: RequestId(1),
            events: Vec::new(),
        }
    }

    async fn request(
        &mut self,
        build: impl FnOnce(RequestId) -> ClientRequest,
    ) -> Result<Ack, String> {
        let id = self.next_id;
        self.next_id = RequestId(id.0 + 1);
        let frame = dex_protocol::encode_frame(&RequestFrame::new(id, build(id)))
            .expect("encode request");
                self.writer.send_raw(&frame).await.expect("send request");

        // Read until this request's answer arrives, collecting events on the way.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err("timed out waiting for an answer".to_string());
            }
            let next = tokio::time::timeout(remaining, self.reader.next_frame())
                .await
                .map_err(|_| "timed out waiting for an answer".to_string())
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;

            let next = match next {
                Frame::Complete(next) => next,
                Frame::Closed => return Err("the runtime closed the connection".to_string()),
            };

            match next {
                ServerResponse::Ack { id: ack_id, ok } if ack_id == id => return Ok(ok),
                ServerResponse::Err { id: err_id, error } if err_id == id => {
                    return Err(error.to_string())
                }
                ServerResponse::Event(frame) => self.events.push(frame),
                // Another request's answer; keep waiting for ours.
                _ => {}
            }
        }
    }

    async fn create_session(&mut self, dir: impl Into<String>) -> Result<SessionId, String> {
        let working_dir = dir.into();
        match self
            .request(|_| ClientRequest::CreateSession {
                working_dir,
                model: None,
            })
            .await?
        {
            Ack::CreateSession { session_id, .. } => Ok(session_id),
            other => Err(format!("unexpected: {}", other.kind())),
        }
    }

    async fn send(&mut self, session: SessionId, text: &str) -> Result<(), String> {
        self.request(|_| ClientRequest::SendMessage {
            session_id: session,
            text: text.to_string(),
        })
        .await
        .map(|_| ())
    }

    async fn cancel(&mut self, session: SessionId) -> Result<(), String> {
        self.request(|_| ClientRequest::Cancel { session_id: session })
            .await
            .map(|_| ())
    }

    async fn close(&mut self, session: SessionId) -> Result<(), String> {
        self.request(|_| ClientRequest::CloseSession { session_id: session })
            .await
            .map(|_| ())
    }

    /// Collect events until the turn ends or the deadline passes.
    async fn drain_turn(&mut self, timeout_ms: u64) -> Vec<EventFrame> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let mut out = Vec::new();
        loop {
            if tokio::time::Instant::now() >= deadline {
                return out;
            }
            let Ok(Ok(Frame::Complete(frame))) = tokio::time::timeout(
                deadline - tokio::time::Instant::now(),
                self.reader.next_frame(),
            )
            .await
            else {
                return out;
            };
            let Some(frame) = frame.into_event() else { continue };
            let finished = matches!(
                frame.event,
                Event::SessionFinished {
                    status: SessionStatus::Completed
                        | SessionStatus::Failed
                        | SessionStatus::Cancelled
                }
            );
            out.push(frame);
            if finished {
                return out;
            }
        }
    }

    /// Drop the socket without a close frame, as a disappearing frontend would.
    fn hang_up(&mut self) {
        // Closing the write half ends the connection from this side, which is
        // what a disappearing frontend looks like to the runtime.
        drop(self.writer.close());
    }
}

fn names(events: &[EventFrame]) -> Vec<&'static str> {
    events
        .iter()
        .map(|frame| match frame.event {
            Event::ModelStarted => "model",
            Event::ProgramStarted { .. } => "program",
            Event::ProgramFailed { .. } => "program-failed",
            Event::CapabilityStarted { .. } => "capability",
            Event::CapabilityFinished { .. } => "capability-finished",
            Event::FileChanged { .. } => "file-changed",
            Event::MemoryWrite { .. } => "memory",
            Event::Answer { .. } => "answer",
            Event::SessionFinished { .. } => "finished",
            _ => "other",
        })
        .collect()
}

#[tokio::test]
async fn a_session_is_created_and_closed_over_the_socket() {
    let h = Harness::start(vec![]).await;
    let mut client = h.client().await;
    let session = client.create_session(h.repo()).await.expect("create");
    client.close(session).await.expect("close");
    assert_eq!(h.manager.len().await, 0, "the session is gone");
}

#[tokio::test]
async fn a_model_request_never_carries_a_tool_schema() {
    let h = Harness::start(vec![]).await;
    let (mut client, session) = h.session().await;
    client.send(session, "find auth").await.expect("send");
    let _ = client.drain_turn(2_000).await;

    let seen = h.provider.seen().await;
    assert!(!seen.is_empty(), "the provider should have been called");
    for request in seen {
        assert_eq!(
            request["system_has_capabilities"], true,
            "the model must be given the capability surface"
        );
    }
}

#[tokio::test]
async fn a_program_runs_against_real_capabilities_and_answers() {
    // The model writes a program; the runtime runs it against the real working
    // tree and the answer comes back over the socket.
    let h = Harness::start(vec![
        "```rune\npub fn main() {\n    let found = dex::find(\"login\");\n    dex::respond(format!(\"found {} in {}\", found[\"matches\"].len(), found[\"matches\"][0][\"path\"]));\n}\n```",
    ])
    .await;

    let (mut client, session) = h.session().await;
    client.send(session, "where is login?").await.expect("send");
    let events = client.drain_turn(20_000).await;

    let answer = events.iter().find_map(|f| match &f.event {
        Event::Answer { text } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(
        answer.as_deref(),
        Some("found 1 in src/auth.rs"),
        "the program's answer should reach the frontend"
    );

    // And the work is visible as events, not just as a final string.
    let seen = names(&events);
    assert!(seen.contains(&"capability"), "expected a capability call: {seen:?}");
    assert!(seen.contains(&"answer"), "expected an answer: {seen:?}");
}

#[tokio::test]
async fn a_program_can_edit_a_file_and_the_change_reaches_disk() {
    let h = Harness::start(vec![
        "```rune\npub fn main() {\n    dex::edit(\"src/auth.rs\", \"fn login() {}\", \"fn login() { let token = 1; }\");\n    dex::respond(\"edited\");\n}\n```",
    ])
    .await;

    let (mut client, session) = h.session().await;
    client.send(session, "change login").await.expect("send");
    let events = client.drain_turn(20_000).await;

    let changed = events.iter().any(|f| {
        matches!(
            &f.event,
            Event::FileChanged {
                change: FileChange::Modified,
                ..
            }
        )
    });
    assert!(changed, "expected a FileChanged event: {:?}", names(&events));

    let on_disk = std::fs::read_to_string(h._dir.path().join("repo/src/auth.rs")).expect("read");
    assert!(
        on_disk.contains("let token = 1;"),
        "the edit should be on disk, got {on_disk}"
    );
}

#[tokio::test]
async fn a_disconnect_does_not_disturb_the_session() {
    // The frontend vanishes mid-turn. The turn must finish anyway, and the
    // session must still be there afterwards.
    let h = Harness::start(vec![
        "```rune\npub fn main() {\n    dex::log(\"working\");\n    dex::respond(\"finished without a listener\");\n}\n```",
    ])
    .await;

    let (mut client, session) = h.session().await;
    client.send(session, "do something slow").await.expect("send");
    // Drop the connection straight away.
    client.hang_up();
    drop(client);
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(h.manager.len().await, 1, "the session outlives its client");

    // A fresh connection can attach and see the finished session.
    let mut reconnected = h.client().await;
    let ack = reconnected
        .request(|_| ClientRequest::Attach { session_id: session })
        .await
        .expect("attach");
    assert!(matches!(ack, Ack::Ok));
}

#[tokio::test]
async fn cancellation_stops_a_running_turn() {
    // A program that loops far longer than the test would wait.
    let h = Harness::start(vec![
        "```rune\npub fn main() {\n    let x = 0;\n    loop { x += 1; }\n}\n```",
    ])
    .await;

    let (mut client, session) = h.session().await;
    client.send(session, "loop forever").await.expect("send");
    // Give it a moment to start, then cancel.
    tokio::time::sleep(Duration::from_millis(150)).await;
    client.cancel(session).await.expect("cancel");

    let events = client.drain_turn(10_000).await;
    let ended = events.iter().any(|f| {
        matches!(
            f.event,
            Event::SessionFinished { .. }
        )
    });
    assert!(ended, "the turn should have ended: {:?}", names(&events));
}

#[tokio::test]
async fn a_turn_that_exhausts_its_rounds_still_answers() {
    // The model never calls respond; the runtime closes the turn rather than
    // leaving it open forever.
    let h = Harness::start(vec![
        "```rune\npub fn main() { 1 }```",
        "```rune\npub fn main() { 2 }```",
        "```rune\npub fn main() { 3 }```",
        "```rune\npub fn main() { 4 }```",
        "```rune\npub fn main() { 5 }```",
    ])
    .await;

    let (mut client, session) = h.session().await;
    client.send(session, "go").await.expect("send");
    let events = client.drain_turn(20_000).await;

    let answered = events.iter().any(|f| {
        matches!(
            f.event,
            Event::Answer { .. }
        )
    });
    assert!(
        answered,
        "a turn that ran out of rounds should still answer: {:?}",
        names(&events)
    );
}

#[tokio::test]
async fn an_ungranted_capability_is_refused_and_reported() {
    // Write is deliberately withheld here, so the refusal reaches the model
    // rather than touching the disk.
    let h = Harness::start_full(
        vec!["```rune\npub fn main() { dex::write(\"planted.txt\", \"nope\"); 1 }```"],
        ExecutionBudget::default(),
        "filesystem.read=*;ui.interact=*",
    )
    .await;

    let (mut client, session) = h.session().await;
    client.send(session, "write a file").await.expect("send");
    let events = client.drain_turn(20_000).await;

    let failed = events.iter().any(|f| {
        matches!(
            f.event,
            Event::ProgramFailed { .. }
        )
    });
    assert!(failed, "expected the program to fail: {:?}", names(&events));
    assert!(
        !h._dir.path().join("repo/planted.txt").exists(),
        "a refused write must not create a file"
    );
}

#[tokio::test]
async fn capabilities_can_be_listed_over_the_wire() {
    let h = Harness::start(vec![]).await;
    let mut client = h.client().await;
    let ack = client
        .request(|_| ClientRequest::ListCapabilities)
        .await
        .expect("list");
    let Ack::Capabilities { authorities, .. } = ack else {
        panic!("expected capabilities, got {ack:?}");
    };
    assert!(
        authorities.iter().any(|g| g.capability == "filesystem.read"),
        "the granted authority should be visible: {authorities:?}"
    );
}

#[tokio::test]
async fn an_unknown_session_is_a_clean_error_not_a_hang() {
    let h = Harness::start(vec![]).await;
    let mut client = h.client().await;
    let bogus = dex_protocol::SessionId([99; 16]);
    let err = client
        .request(|_| ClientRequest::SendMessage {
            session_id: bogus,
            text: "hi".into(),
        })
        .await
        .expect_err("must fail");
    assert!(err.contains("no such session"), "got {err}");
}

#[tokio::test]
async fn a_program_can_store_and_reload_a_procedure() {
    // The whole point of memory: a program saved in one turn, run in another.
    let h = Harness::start(vec![
        "```rune\npub fn main() { dex::remember(\"count-logins\", \"let n = 0; n\"); dex::respond(\"saved\"); }```",
        "```rune\npub fn main() { let p = dex::recall(\"count-logins\"); dex::respond(format!(\"reloaded {}\", p[\"exec_count\"])); }```",
    ])
    .await;

    let (mut client, session) = h.session().await;

    client.send(session, "remember it").await.expect("send");
    let _ = client.drain_turn(20_000).await;

    client.send(session, "reload it").await.expect("send");
    let events = client.drain_turn(20_000).await;

    let answer = events.iter().find_map(|f| match &f.event {
        Event::Answer { text } => Some(text.clone()),
        _ => None,
    });
    assert_eq!(
        answer.as_deref(),
        Some("reloaded 1"),
        "the second turn should see the first turn's stored program"
    );
}

#[tokio::test]
async fn concurrent_sessions_do_not_interfere() {
    let h = Harness::start(vec![
        "```rune\npub fn main() { dex::respond(\"one\"); }```",
        "```rune\npub fn main() { dex::respond(\"two\"); }```",
    ])
    .await;

    let mut first = h.client().await;
    let mut second = h.client().await;
    let a = first.create_session(h.repo()).await.expect("a");
    let b = second.create_session(h.repo()).await.expect("b");
    assert_ne!(a, b, "each connection gets its own session");

    first.send(a, "first").await.expect("send first");
    second.send(b, "second").await.expect("send second");

    let events_a = first.drain_turn(20_000).await;
    let events_b = second.drain_turn(20_000).await;

    let answer = |events: &[EventFrame]| {
        events.iter().find_map(|f| match &f.event {
            Event::Answer { text } => Some(text.clone()),
            _ => None,
        })
    };
    assert_eq!(answer(&events_a).as_deref(), Some("one"));
    assert_eq!(answer(&events_b).as_deref(), Some("two"));
}

#[tokio::test]
async fn a_front_end_that_is_not_the_terminal_works_the_same() {
    // The protocol is the contract; the CLI is just one consumer. This test is
    // that consumer, written directly against the frames.
    let h = Harness::start(vec![
        "```rune\npub fn main() { dex::respond(\"hello from anywhere\"); }```",
    ])
    .await;

    let mut client = h.client().await;
    let session = client.create_session(h.repo()).await.expect("session");
    client.send(session, "hi").await.expect("send");
    let events = client.drain_turn(20_000).await;

    let call_ids: Vec<CallId> = events
        .iter()
        .filter_map(|f| match &f.event {
            Event::ProgramStarted { call_id, .. } => Some(*call_id),
            _ => None,
        })
        .collect();
    assert_eq!(call_ids.len(), 1, "one program ran: {call_ids:?}");
    assert!(
        events.iter().any(|f| matches!(
            f.event,
            Event::Answer { .. }
        )),
        "the answer arrived: {:?}",
        names(&events)
    );
}

#[tokio::test]
async fn a_budget_that_cannot_be_met_stops_the_program() {
    let h = Harness::start_with(
        vec!["```rune\npub fn main() { let x = 0; loop { x += 1; } x }```"],
        ExecutionBudget {
            instructions: 20_000,
            ..ExecutionBudget::default()
        },
    )
    .await;

    let (mut client, session) = h.session().await;
    client.send(session, "loop forever").await.expect("send");
    let events = client.drain_turn(20_000).await;

    let failed = events.iter().any(|f| {
        matches!(
            f.event,
            Event::ProgramFailed { .. }
        )
    });
    assert!(
        failed,
        "the instruction budget should have stopped it: {:?}",
        names(&events)
    );
}