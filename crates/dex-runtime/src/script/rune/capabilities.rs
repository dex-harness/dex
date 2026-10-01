//! Rune bindings for the DEX capability API.
//!
//! Everything a generated program can reach is registered here. There is no
//! generic entry point: each function is a named operation, and each one goes
//! through [`CapabilityCtx`], which has already charged the budget, checked
//! cancellation, resolved the path, and confirmed authority.
//!
//! The module is installed with stdio disabled, so a program cannot print or
//! read the terminal. `dex::log` is the only way to emit output, and it becomes
//! a structured event rather than a write to a stream the runtime does not own.
//!
//! # Three constraints of the language shape this surface
//!
//! **No optional arguments.** Rune fixes a function's arity, so a program must
//! pass every parameter. Capabilities are therefore named for the case they
//! serve rather than taking a bag of defaults: `dex::find(query)` alongside
//! `dex::find_in(query, path)`, `dex::read(path)` alongside
//! `dex::read_lines(path, offset, limit)`. A program writes one call per
//! intent instead of filling in placeholders.
//!
//! **No `try`/`catch` blocks.** Errors are returned as values, which is what
//! makes them inspectable: a program either propagates with `?` or examines one
//! with `if let Err(e) = ...` and `dex::error_kind(e)`. The kind leads the
//! message, so the reason survives the trip back to the model.
//!
//! **No `mut`.** Everything is mutable by default.
//!
//! # The value bridge
//!
//! Both directions come from Rune itself: `serde_json::Value` deserialises into
//! a Rune value, and a Rune value implements `Serialize`. A capability returns
//! plain JSON and the program receives an ordinary Rune map or list it can
//! index and iterate, with no bespoke mapping to keep in step with the language.

// Each binding body is a closure that is called immediately. That is not
// incidental: Rune reads a `Result` return type as a Result *value* and a
// `VmResult` return type as the raise channel, so a body that wants `?` has
// to produce a `Result` and convert on the way out.
#![allow(clippy::redundant_closure_call)]

use rune::runtime::{Value, VmError, VmResult};
use rune::{Any, Context, ContextError, Module};

use super::{block_on, current_ctx};
use crate::capability::{filesystem, git, repo, testing, CapabilityCtx, CapabilityError};
use crate::memory::{MemoryKind, MemoryRecord};
use dex_protocol::{CapabilityErrorKind, Event};

/// Install the `dex` module into `context`.
pub fn install(context: &mut Context) -> Result<(), ContextError> {
    context.install(dex()?)
}

/// A failure a program can inspect.
///
/// Rune has no typed error values, so the kind leads the message and
/// `dex::error_kind` parses it back out.
#[derive(Debug, Any)]
pub struct DexError {
    message: String,
}

impl DexError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for DexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<CapabilityError> for DexError {
    fn from(e: CapabilityError) -> Self {
        DexError::new(format!("{}: {}", e.kind, e.message))
    }
}

/// Map a memory failure onto the capability error a program reads.
fn memory_error(e: crate::memory::MemoryError) -> CapabilityError {
    let kind = match e {
        crate::memory::MemoryError::NotFound(_) => CapabilityErrorKind::ResourceNotFound,
        crate::memory::MemoryError::InvalidKey(_)
        | crate::memory::MemoryError::InvalidKind(_) => CapabilityErrorKind::InvalidArgument,
        _ => CapabilityErrorKind::OperationFailed,
    };
    CapabilityError::new(kind, e.to_string())
}

type RuneResult<T> = Result<T, DexError>;

/// Convert the body's `Result` into the `VmResult` a binding declares.
///
/// Only the success path is expected to occur: a capability failure becomes an
/// `{"error": ...}` value rather than an error here, so a program always
/// receives data. The conversion exists because Rune reads a `Result` return
/// type as a Result value, which is the behaviour this design is avoiding.
trait IntoVmResult {
    type Output;
    fn into_vm(self) -> VmResult<Self::Output>;
}

impl<T> IntoVmResult for RuneResult<T> {
    type Output = T;
    fn into_vm(self) -> VmResult<T> {
        match self {
            Ok(value) => VmResult::Ok(value),
            Err(error) => VmResult::Err(VmError::panic(error.to_string())),
        }
    }
}

/// Authority, and the error a program reads.
///
/// Rune has no `try`/`catch`, so a capability cannot raise something the program
/// chooses to handle. Instead every capability returns the value it produced, or
/// an `{"error": {...}}` object. A program therefore reads a result the same way
/// either way, and an unhandled failure is still data the model can be shown
/// rather than a dead turn.
fn missing_context() -> DexError {
    DexError::new("the script runtime has no capability context installed")
}

/// Convert a capability's JSON result into a Rune value.
fn to_rune(json: &serde_json::Value) -> RuneResult<Value> {
    let text = serde_json::to_string(json)
        .map_err(|e| DexError::new(format!("could not encode a capability result: {e}")))?;
    serde_json::from_str(&text)
        .map_err(|e| DexError::new(format!("could not decode a capability result: {e}")))
}

/// Emit the finish event and shape the outcome for the program.
///
/// On success the capability's own value passes through unchanged. On failure
/// the program receives `{"error": {"kind": ..., "message": ...}}`, which it can
/// branch on with `dex::error_kind` and the model can be shown verbatim.
fn finish<T: serde::Serialize>(
    ctx: CapabilityCtx,
    name: &str,
    result: Result<T, CapabilityError>,
) -> RuneResult<Value> {
    match result {
        Ok(value) => {
            ctx.emit_finished(name, true, "ok");
            let json = serde_json::to_value(&value)
                .map_err(|e| DexError::new(format!("could not encode a {name} result: {e}")))?;
            to_rune(&json)
        }
        Err(e) => {
            ctx.emit_finished(name, false, e.to_string());
            // A capability failure ends the program with a precise reason. The
            // runtime hands that reason to the model, which writes a corrected
            // program: the adjustment happens where the decision is made, and
            // the reason is never lost in an intermediate representation.
            Err(DexError::new(format!("{}: {}", e.kind, e.message)))
        }
    }
}

/// The `dex` module, addressed from Rune as `dex::...`.
#[rune::module(::dex)]
pub fn dex() -> Result<Module, ContextError> {
    let mut m = Module::from_meta(module_meta)?;
    m.function_meta(find)?;
    m.function_meta(find_in)?;
    m.function_meta(read)?;
    m.function_meta(read_lines)?;
    m.function_meta(list)?;
    m.function_meta(edit)?;
    m.function_meta(write)?;
    m.function_meta(git_status)?;
    m.function_meta(git_diff)?;
    m.function_meta(git_diff_path)?;
    m.function_meta(git_log)?;
    m.function_meta(git_checkout)?;
    m.function_meta(delete)?;
    m.function_meta(delete_tree)?;
    m.function_meta(exists)?;
    m.function_meta(stat)?;
    m.function_meta(test)?;
    m.function_meta(test_args)?;
    m.function_meta(test_targets)?;
    m.function_meta(remember)?;
    m.function_meta(recall)?;
    m.function_meta(recalls)?;
    m.function_meta(forget)?;
    m.function_meta(respond)?;
    m.function_meta(ask)?;
    m.function_meta(log)?;
    Ok(m)
}


/// Search the whole working tree.
#[rune::function]
fn find(query: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    (|| -> RuneResult<Value> {

    find_inner(query, "")
    })()
    
    })()
    .into_vm()
}

/// Search below a path, which is how a program narrows a broad search.
#[rune::function]
fn find_in(query: &str, path: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    (|| -> RuneResult<Value> {

    find_inner(query, path)
    })()
    
    })()
    .into_vm()
}

/// The shared body of `find` and `find_in`. A binding cannot call another
/// binding, because the macro rewrites the signature it exposes.
fn find_inner(query: &str, path: &str) -> RuneResult<Value> {
    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("repo.find", format!("{query:?}"));
    let json = repo::find(&ctx, query, path, None, 200, false);
    finish(ctx, "repo.find", json)
}

/// Read a whole file.
#[rune::function]
fn read(path: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    (|| -> RuneResult<Value> {

    read_inner(path, 1, 0)
    })()
    
    })()
    .into_vm()
}

/// Read a line range. `offset` is 1-based; a `limit` below 1 reads to the end.
#[rune::function]
fn read_lines(path: &str, offset: i64, limit: i64) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    (|| -> RuneResult<Value> {

    read_inner(path, offset, limit)
    })()
    
    })()
    .into_vm()
}

fn read_inner(path: &str, offset: i64, limit: i64) -> RuneResult<Value> {
    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("repo.read", format!("{path:?}"));
    let json = repo::read(
        &ctx,
        path,
        if offset <= 0 { 1 } else { offset as usize },
        if limit < 0 { 0 } else { limit as usize },
    );
    finish(ctx, "repo.read", json)
}

/// List the entries directly below a path.
#[rune::function]
fn list(path: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("repo.list", String::new());
    let json = repo::list(&ctx, path, 1, None);
    finish(ctx, "repo.list", json)
    })()
    .into_vm()
}

/// Replace an exact string in a file.
///
/// Refuses when the text is absent or appears more than once, so a model cannot
/// silently edit the wrong place.
#[rune::function]
fn edit(path: &str, old_string: &str, new_string: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("repo.edit", format!("{path:?}"));
    let json = edit_file(&ctx, path, old_string, new_string);
    finish(ctx, "repo.edit", json)
    })()
    .into_vm()
}

/// Write a whole file, replacing whatever was there.
#[rune::function]
fn write(path: &str, content: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("repo.write", format!("{path:?}"));
    let json = filesystem::write(&ctx, path, content);
    finish(ctx, "repo.write", json)
    })()
    .into_vm()
}

/// Working tree status.
#[rune::function]
fn git_status() -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("git.status", String::new());
    let json = block_on(git::status(&ctx));
    finish(ctx, "git.status", json)
    })()
    .into_vm()
}

/// Unified diff of the whole working tree.
#[rune::function]
fn git_diff() -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    (|| -> RuneResult<Value> {

    git_diff_inner("")
    })()
    
    })()
    .into_vm()
}

/// Unified diff of one path.
#[rune::function]
fn git_diff_path(path: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    (|| -> RuneResult<Value> {

    git_diff_inner(path)
    })()
    
    })()
    .into_vm()
}

fn git_diff_inner(path: &str) -> RuneResult<Value> {
    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("git.diff", String::new());
    let json = block_on(git::diff(&ctx, if path.is_empty() { None } else { Some(path) }));
    finish(ctx, "git.diff", json)
}

/// The twenty most recent commits.
#[rune::function]
fn git_log() -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("git.log", String::new());
    let json = block_on(git::log(&ctx, 20));
    finish(ctx, "git.log", json)
    })()
    .into_vm()
}

/// Check out a branch.
#[rune::function]
fn git_checkout(branch: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("git.checkout", format!("{branch:?}"));
    let json = block_on(git::checkout(&ctx, branch));
    finish(ctx, "git.checkout", json)
    })()
    .into_vm()
}

/// Delete one file.
#[rune::function]
fn delete(path: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("filesystem.delete", format!("{path:?}"));
    let json = filesystem::delete(&ctx, path);
    finish(ctx, "filesystem.delete", json)
    })()
    .into_vm()
}

/// Delete a directory and everything under it.
#[rune::function]
fn delete_tree(path: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("filesystem.delete_tree", format!("{path:?}"));
    let json = filesystem::delete_tree(&ctx, path);
    finish(ctx, "filesystem.delete_tree", json)
    })()
    .into_vm()
}

/// Whether a path exists.
#[rune::function]
fn exists(path: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("filesystem.exists", format!("{path:?}"));
    let json = filesystem::exists(&ctx, path);
    finish(ctx, "filesystem.exists", json)
    })()
    .into_vm()
}

/// Metadata about a path.
#[rune::function]
fn stat(path: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("filesystem.stat", format!("{path:?}"));
    let json = filesystem::stat(&ctx, path);
    finish(ctx, "filesystem.stat", json)
    })()
    .into_vm()
}

/// Run an allowlisted test target, e.g. `dex::test("cargo")`.
///
/// The target chooses its own arguments, so this is the form a program uses
/// almost always; `test_args` is the escape for a filter or a subcommand.
#[rune::function]
fn test(name: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    (|| -> RuneResult<Value> {

    test_inner(name, Vec::new())
    })()
    
    })()
    .into_vm()
}

/// Run an allowlisted test target with arguments, e.g.
/// `dex::test_args("cargo", ["test", "auth"])`.
#[rune::function]
fn test_args(name: &str, args: Vec<String>) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    (|| -> RuneResult<Value> {

    test_inner(name, args)
    })()
    
    })()
    .into_vm()
}

fn test_inner(name: &str, args: Vec<String>) -> RuneResult<Value> {
    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("testing.run", format!("{name:?}"));
    let json = block_on(testing::run(&ctx, name, &args));
    finish(ctx, "testing.run", json)
}

/// The targets `dex::test` accepts, for a program that guessed wrong.
#[rune::function]
fn test_targets() -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let names: Vec<serde_json::Value> = testing::target_names()
        .into_iter()
        .map(|n| serde_json::Value::String(n.to_string()))
        .collect();
    to_rune(&serde_json::Value::Array(names))
    })()
    .into_vm()
}

/// Store a reusable procedure under a key.
#[rune::function]
fn remember(key: &str, program: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("memory.save", format!("{key:?}"));
    let mut record = MemoryRecord::new(key, MemoryKind::Program);
    record.program = Some(program.to_string());
    match ctx.memory.save(record) {
        Ok(saved) => {
            ctx.events.emit(Event::MemoryWrite {
                key: saved.key.clone(),
                kind: saved.kind.as_str().to_string(),
            });
            ctx.emit_finished("memory.save", true, "saved");
            to_rune(&saved.to_value())
        }
        Err(e) => finish::<serde_json::Value>(ctx, "memory.save", Err(memory_error(e))),
    }
    })()
    .into_vm()
}

/// Load a stored record.
#[rune::function]
fn recall(key: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("memory.load", format!("{key:?}"));
    // Count this load before reading the record back, so what a program sees
    // includes it. The number answers "how well has this held up", and a count
    // that ignored the load in progress would be off by one forever.
    let _ = ctx.memory.record_use(key);
    match ctx.memory.load(key) {
        Ok(loaded) => {
            ctx.emit_finished("memory.load", true, "loaded");
            to_rune(&loaded.to_value())
        }
        Err(e) => finish::<serde_json::Value>(ctx, "memory.load", Err(memory_error(e))),
    }
    })()
    .into_vm()
}

/// Every stored key.
#[rune::function]
fn recalls() -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("memory.list", String::new());
    match ctx.memory.list() {
        Ok(keys) => {
            ctx.emit_finished("memory.list", true, format!("{} keys", keys.len()));
            to_rune(&serde_json::json!(keys))
        }
        Err(e) => finish::<serde_json::Value>(ctx, "memory.list", Err(memory_error(e))),
    }
    })()
    .into_vm()
}

/// Remove a stored record.
#[rune::function]
fn forget(key: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_started("memory.delete", format!("{key:?}"));
    match ctx.memory.delete(key) {
        Ok(()) => {
            ctx.emit_finished("memory.delete", true, "deleted");
            to_rune(&serde_json::json!({ "deleted": key }))
        }
        Err(e) => finish::<serde_json::Value>(ctx, "memory.delete", Err(memory_error(e))),
    }
    })()
    .into_vm()
}

/// Deliver the turn's answer and end the turn.
#[rune::function]
fn respond(message: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?;
    ctx.ui.respond(message.to_string());
    // A unit return is not an accepted binding signature, so the call yields
    // the unit value instead. Nothing downstream reads it.
    to_rune(&serde_json::Value::Null)
    })()
    .into_vm()
}

/// Ask the human a question and wait for the answer.
#[rune::function]
fn ask(message: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.events.emit(Event::UiPrompt {
        call_id: ctx.call,
        message: message.to_string(),
    });
    match block_on(ctx.ui.ask(message.to_string())) {
        Ok(reply) => to_rune(&serde_json::Value::String(reply)),
        Err(e) => finish::<serde_json::Value>(
            ctx,
            "ui.ask",
            Err(CapabilityError::new(
                CapabilityErrorKind::OperationFailed,
                e.to_string(),
            )),
        ),
    }
    })()
    .into_vm()
}

/// Emit a line of progress, delivered as a structured event.
#[rune::function]
fn log(message: &str) -> VmResult<Value> {
    (|| -> RuneResult<Value> {

    let ctx = current_ctx().map_err(|_| missing_context())?.scoped();
    ctx.emit_output(message.to_string());
    to_rune(&serde_json::Value::Null)
    })()
    .into_vm()
}


/// Exact-string edit with the ambiguity check that stops a wrong-file edit.
fn edit_file(
    ctx: &CapabilityCtx,
    path: &str,
    old_string: &str,
    new_string: &str,
) -> Result<serde_json::Value, CapabilityError> {
    if old_string.is_empty() {
        return Err(CapabilityError::new(
            CapabilityErrorKind::InvalidArgument,
            "old_string must not be empty; use dex::write to create a file",
        ));
    }
    let resolved = ctx.resolve_write(filesystem::WRITE, path)?;
    let bytes = ctx.read_file(&resolved)?;
    let text = String::from_utf8_lossy(&bytes).into_owned();

    match text.matches(old_string).count() {
        0 => {
            return Err(CapabilityError::new(
                CapabilityErrorKind::InvalidArgument,
                format!("old_string does not appear in {path}; the file may have changed"),
            ))
        }
        1 => {}
        n => {
            return Err(CapabilityError::new(
                CapabilityErrorKind::InvalidArgument,
                format!(
                    "old_string appears {n} times in {path}; include more surrounding context \
                     to disambiguate"
                ),
            ))
        }
    }

    let replaced = text.replacen(old_string, new_string, 1);
    ctx.write_file(&resolved, replaced.as_bytes())?;
    Ok(serde_json::json!({
        "path": path,
        "changed": true,
        "mode": "replace",
    }))
}
