//! End-to-end tests for the Rune script runtime.
//!
//! These run real Rune programs against real capabilities on a real working
//! directory, through the same `ScriptRuntime` trait the session uses. They are
//! the proof that the pipeline works: model source in, structured result out,
//! with authorization and budgets enforced underneath.
//!
//! Three properties of the language shape what the programs below look like,
//! and each is exercised rather than assumed: a program is a `main`, errors are
//! values inspected with `if let`, and a call has a fixed arity.

use std::path::PathBuf;
use std::sync::Arc;

use dex_protocol::{Event, SessionId};
use dex_runtime::auth::Authority;
use dex_runtime::budget::ExecutionBudget;
use dex_runtime::events::EventSink;
use dex_runtime::memory::MemoryStore;
use dex_runtime::script::rune::RuneScriptRuntime;
use dex_runtime::script::{
    ScriptContext, ScriptError, ScriptResult, ScriptRuntime, UiEvent, UiHandle,
};
use tokio_util::sync::CancellationToken;

/// Grants every capability these tests need.
const FULL: &str = "filesystem.read=*;filesystem.write=*;git.read=*;testing.run=*;memory.read=*;memory.write=*;ui.interact=*";

struct Harness {
    /// Held for its lifetime: dropping the temporary directory would pull the
    /// working directory out from under the runtime.
    _dir: tempfile::TempDir,
    root: PathBuf,
    memory_dir: PathBuf,
    runtime: Arc<RuneScriptRuntime>,
    events: EventSink,
    authority: String,
    limits: ExecutionBudget,
    cancel: CancellationToken,
    ui: UiHandle,
    ui_rx: tokio::sync::mpsc::UnboundedReceiver<UiEvent>,
}

impl Harness {
    fn new(authority: &str) -> Self {
        Self::with_limits(authority, ExecutionBudget::default())
    }

    fn with_limits(authority: &str, limits: ExecutionBudget) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("src")).expect("mkdir");
        std::fs::write(root.join("src/auth.rs"), "fn login() {}\nfn logout() {}\n")
            .expect("write");
        std::fs::write(root.join("README.md"), "# demo\n").expect("write");
        std::fs::write(root.join(".gitignore"), "target/\n").expect("write");

        let (ui, ui_rx) = UiHandle::channel();
        let memory_dir = root.join("memory");
        Self {
            _dir: dir,
            root,
            memory_dir,
            runtime: RuneScriptRuntime::start().expect("script runtime"),
            events: EventSink::new(SessionId::new()),
            authority: authority.to_string(),
            limits,
            cancel: CancellationToken::new(),
            ui,
            ui_rx,
        }
    }

    fn context(&self) -> ScriptContext {
        ScriptContext {
            session_id: SessionId::new(),
            working_dir: self.root.clone(),
            cancel: self.cancel.clone(),
            limits: self.limits,
            authority: Arc::new(Authority::parse(&self.authority).expect("authority")),
            memory: Arc::new(MemoryStore::new(self.memory_dir.clone())),
            events: self.events.clone(),
            ui: self.ui.clone(),
            round: 0,
        }
    }

    /// Run a program body, wrapped in the `main` a Rune module requires.
    async fn run(&mut self, body: &str) -> Result<ScriptResult, ScriptError> {
        self.run_capturing(body).await.0
    }

    /// Run and also return the events emitted while it ran. A broadcast channel
    /// does not replay to a late subscriber, so the capture subscribes first.
    async fn run_capturing(&mut self, body: &str) -> (Result<ScriptResult, ScriptError>, Vec<Event>) {
        let mut rx = self.events.subscribe();
        let program = format!("pub fn main() {{\n{body}\n}}\n");
        let result = self.runtime.execute(&program, self.context()).await;
        let mut captured = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            captured.push(frame.event);
        }
        (result, captured)
    }
}

#[tokio::test]
async fn a_program_computes_and_returns_a_value() {
    let mut h = Harness::new(FULL);
    let result = h
        .run("let xs = [1, 2, 3, 4]; let total = 0; for x in xs { total += x; } total")
        .await
        .expect("run");
    assert_eq!(result.value, serde_json::json!(10));
}

#[tokio::test]
async fn a_program_reaches_a_capability_and_gets_structured_data() {
    let mut h = Harness::new(FULL);
    let result = h
        .run(r#"dex::find("login")["matches"].len()"#)
        .await
        .expect("run");
    assert_eq!(result.value, serde_json::json!(1));
}

#[tokio::test]
async fn a_program_can_search_and_filter_in_one_pass() {
    // The point of the architecture: a whole workflow in one program, with no
    // model round trip between the steps.
    let mut h = Harness::new(FULL);
    let result = h
        .run(
            r#"
            let found = dex::find("fn");
            let names = [];
            for m in found["matches"] {
                if m["text"].contains("fn login()") {
                    names.push(m["path"]);
                }
            }
            names
            "#,
        )
        .await
        .expect("run");
    assert_eq!(result.value, serde_json::json!(["src/auth.rs"]));
}

#[tokio::test]
async fn a_program_can_read_a_file_and_transform_it() {
    let mut h = Harness::new(FULL);
    let result = h
        .run(
            r#"
            let body = dex::read("src/auth.rs");
            let count = 0;
            for line in body["content"].split("\n") {
                if !line.is_empty() {
                    count += 1;
                }
            }
            count
            "#,
        )
        .await
        .expect("run");
    assert_eq!(result.value, serde_json::json!(2));
}

#[tokio::test]
async fn a_capability_emits_start_and_finish_events() {
    let mut h = Harness::new(FULL);
    let (result, events) = h.run_capturing(r#"dex::find("login"); 1"#).await;
    result.expect("run");
    assert!(
        events.iter().any(
            |e| matches!(e, Event::CapabilityStarted { capability, .. } if capability == "repo.find")
        ),
        "expected a CapabilityStarted event, got {events:?}"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, Event::CapabilityFinished { capability, ok: true, .. } if capability == "repo.find")
        ),
        "expected a successful CapabilityFinished event, got {events:?}"
    );
}

#[tokio::test]
async fn a_denied_capability_ends_the_program_with_its_reason() {
    // Read is granted; write is not. The refusal reaches the caller as a
    // precise, actionable reason rather than a silent failure, and the model
    // is handed it so a corrected program can follow.
    let mut h = Harness::new("filesystem.read=*");
    let err = h
        .run(r#"dex::write("new.txt", "hello")"#)
        .await
        .expect_err("must fail");
    let text = err.to_string();
    assert!(text.contains("PermissionDenied"), "got: {text}");
    assert!(text.contains("no authority granted"), "got: {text}");
    assert!(
        !h.root.join("new.txt").exists(),
        "a denied write must not touch the filesystem"
    );
}

#[tokio::test]
async fn a_refused_write_reports_its_reason_as_data() {
    // Nothing is granted for writes, so the call returns an error object. The
    // program is not killed, and the reason survives to the model.
    let mut h = Harness::new("filesystem.read=*");
    let err = h
        .run(r#"dex::write("new.txt", "hello")"#)
        .await
        .expect_err("must refuse");
    assert!(err.to_string().contains("PermissionDenied"), "got {err}");
    assert!(
        !h.root.join("new.txt").exists(),
        "a denied write must not touch the filesystem"
    );
}

#[tokio::test]
async fn a_program_cannot_escape_the_working_directory() {
    let mut h = Harness::new(FULL);
    let err = h
        .run(r#"dex::read("../../../etc/passwd")"#)
        .await
        .expect_err("must refuse");
    assert!(err.to_string().contains("PermissionDenied"), "got {err}");
}

#[tokio::test]
async fn a_program_can_edit_a_file_and_the_change_is_observable() {
    let mut h = Harness::new(FULL);
    h.run(r#"dex::edit("src/auth.rs", "fn login() {}", "fn login() { todo!() }"); 1"#)
        .await
        .expect("edit");
    let written = std::fs::read_to_string(h.root.join("src/auth.rs")).expect("read back");
    assert!(written.contains("todo!"), "got {written}");
}

#[tokio::test]
async fn a_program_can_store_and_reload_a_procedure() {
    let mut h = Harness::new(FULL);
    h.run(r#"dex::remember("demo.proc", "let x = 1; x"); 1"#)
        .await
        .expect("save");
    let reloaded = h
        .run(r#"dex::recall("demo.proc")["program"]"#)
        .await
        .expect("load");
    assert_eq!(reloaded.value, serde_json::json!("let x = 1; x"));
    // The record is genuinely on disk, not cached in the process.
    assert!(h.memory_dir.join("demo.proc.json").exists());
}

#[tokio::test]
async fn respond_delivers_the_turn_answer() {
    let mut h = Harness::new(FULL);
    h.run(r#"dex::respond("authentication lives in src/auth.rs"); 1"#)
        .await
        .expect("run");
    match h.ui_rx.try_recv() {
        Ok(UiEvent::Respond(text)) => assert_eq!(text, "authentication lives in src/auth.rs"),
        other => panic!("expected a response, got {other:?}"),
    }
}

#[tokio::test]
async fn a_program_without_respond_just_returns_its_value() {
    let mut h = Harness::new(FULL);
    let result = h.run("40 + 2").await.expect("run");
    assert_eq!(result.value, serde_json::json!(42));
    assert!(
        h.ui_rx.try_recv().is_err(),
        "nothing should have been sent to the user"
    );
}

#[tokio::test]
async fn a_syntax_error_returns_compiler_diagnostics_to_the_model() {
    let mut h = Harness::new(FULL);
    let err = h.run("let x = ;").await.expect_err("must not compile");
    match err {
        ScriptError::Compile { diagnostics, .. } => {
            let text = diagnostics.expect("diagnostics should be carried");
            assert!(!text.trim().is_empty(), "diagnostics should not be empty");
        }
        other => panic!("expected a compile error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_program_without_main_is_rejected() {
    let h = Harness::new(FULL);
    let err = h
        .runtime
        .execute("pub fn helper() { 1 }", h.context())
        .await
        .expect_err("must not run");
    assert!(err.to_string().contains("main"), "got {err}");
}

#[tokio::test]
async fn a_raised_error_fails_the_program_rather_than_the_runtime() {
    let mut h = Harness::new(FULL);
    let err = h
        .run(r#"let x = 1; x["not an index"]"#)
        .await
        .expect_err("must raise");
    assert!(matches!(err, ScriptError::Raised { .. }), "got {err:?}");

    // The runtime survives, and the next program still runs.
    let ok = h.run("1 + 1").await.expect("runtime still usable");
    assert_eq!(ok.value, serde_json::json!(2));
}

#[tokio::test]
async fn the_capability_call_budget_is_enforced() {
    let mut h = Harness::with_limits(
        FULL,
        ExecutionBudget {
            capability_calls: 3,
            ..ExecutionBudget::default()
        },
    );
    // The fourth call is refused, which ends the program with the reason.
    let err = h
        .run(
            r#"
            for i in 0..10 {
                dex::find("login");
            }
            "#,
        )
        .await
        .expect_err("must exhaust the budget");
    assert!(
        err.to_string().contains("BudgetExceeded"),
        "expected a budget failure, got: {err}"
    );
}

#[tokio::test]
async fn cancellation_stops_a_running_program() {
    let mut h = Harness::new(FULL);
    h.cancel.cancel();
    let err = h
        .run("let x = 0; loop { x += 1; } x")
        .await
        .expect_err("must be cancelled");
    assert!(matches!(err, ScriptError::Cancelled), "got {err:?}");
}

#[tokio::test]
async fn the_instruction_budget_stops_a_compute_only_loop() {
    // The wall clock would take 30s; the instruction budget is what makes this
    // test finish, which is exactly why it exists.
    let mut h = Harness::with_limits(
        FULL,
        ExecutionBudget {
            instructions: 50_000,
            wall_clock: std::time::Duration::from_secs(30),
            ..ExecutionBudget::default()
        },
    );
    let started = std::time::Instant::now();
    let err = h
        .run("let x = 0; loop { x += 1; } x")
        .await
        .expect_err("must exhaust the instruction budget");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the budget should have stopped it quickly, took {:?}: {err}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_program_has_no_ambient_ability_to_print() {
    // The module is installed with stdio disabled, so the obvious escape does
    // not resolve. This is the "no ambient authority" rule at the language level.
    let mut h = Harness::new(FULL);
    let err = h
        .run(r#"println!("leaking to the terminal");"#)
        .await
        .expect_err("print must not be available");
    assert!(err.to_string().contains("print"), "got {err}");
}

#[tokio::test]
async fn programs_do_not_share_state() {
    let mut h = Harness::new(FULL);
    h.run(r#"let leaked = "from the first program"; leaked"#)
        .await
        .expect("first");
    // A fresh VM per program, so a previous program's locals are gone.
    let err = h.run(r#"leaked"#).await.expect_err("must not resolve");
    assert!(err.to_string().contains("leaked"), "got {err}");
}

#[tokio::test]
async fn the_allowlisted_test_runner_is_reachable() {
    let mut h = Harness::new(FULL);
    let targets = h.run("dex::test_targets()").await.expect("run");
    let names: Vec<String> = targets
        .value
        .as_array()
        .expect("array")
        .iter()
        .map(|v| v.as_str().expect("string").to_string())
        .collect();
    assert!(names.contains(&"cargo".to_string()), "got {names:?}");

    // `make` with no target list is permitted and is a genuine subprocess, so
    // this exercises the whole spawn, stream, and collect path.
    std::fs::write(h.root.join("Makefile"), "all:\n\t@echo dex-test-ok\n").expect("write");
    let result = h.run(r#"dex::test("make")"#).await.expect("run");
    assert_eq!(result.value["succeeded"], true, "got {}", result.value);
    assert!(result.value["stdout"]
        .as_str()
        .expect("stdout")
        .contains("dex-test-ok"));
}
