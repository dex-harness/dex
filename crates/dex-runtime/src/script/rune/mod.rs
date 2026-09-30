//! The Rune implementation of [`ScriptRuntime`].
//!
//! This is the **only** module in the runtime that imports `rune`. Everything
//! else is written against the `ScriptRuntime` trait, which is what keeps the
//! scripting language an implementation detail rather than a dependency. A test
//! in `tests/language_boundary.rs` enforces that.
//!
//! # Threading
//!
//! `rune::Vm` is `!Send`, so a VM cannot move between threads, and neither can
//! the future that drives it. One dedicated worker thread owns a VM at a time,
//! driven by its own current-thread tokio runtime. [`ScriptRuntime::execute`]
//! hands work over a channel and awaits a oneshot reply, so callers see a
//! plain async API while the language's non-`Send` internals stay on the
//! worker.
//!
//! # Reaching the capability context
//!
//! The VM exposes no accessor for host state, and each execution needs a
//! different `CapabilityCtx`. Rebuilding the whole `Context` per run is not an
//! option — `Context::runtime` is documented as not cheap. The native functions
//! instead read a thread-local slot the worker fills in before each run, which
//! is sound precisely because all execution happens on that one thread.

use std::cell::RefCell;
use std::sync::Arc;

use async_trait::async_trait;
use rune::runtime::{budget, RuntimeContext, VmError};
use rune::{Context, Diagnostics, Source, Sources, Unit, Value, Vm};
use tokio::sync::{mpsc, oneshot};

mod capabilities;

use capabilities::install;
use super::{ScriptContext, ScriptError, ScriptLanguage, ScriptResult, ScriptRuntime};
use crate::capability::CapabilityCtx;

/// What a program can reach while it is running.
struct Ambient {
    ctx: CapabilityCtx,
    /// Handle to the worker runtime, so a synchronous capability binding can run
    /// an async capability to completion.
    handle: tokio::runtime::Handle,
}

// Host state for the program currently running on this thread.
thread_local! {
    static CURRENT: RefCell<Option<Ambient>> = const { RefCell::new(None) };
}

/// Borrow the current context, cloning so the borrow ends before any call.
///
/// Cloning is cheap: the context is a handful of `Arc`s and a `PathBuf`.
pub(super) fn current_ctx() -> Result<CapabilityCtx, ScriptError> {
    CURRENT.with(|slot| {
        slot.borrow()
            .as_ref()
            .map(|ambient| ambient.ctx.clone())
            .ok_or_else(|| ScriptError::Unavailable("no capability context is installed".into()))
    })
}

/// Run an async capability to completion from a synchronous binding.
///
/// Rune 0.14's `#[rune::function]` does not accept `async fn`, so the bindings
/// are synchronous and this is the seam. The worker runs a multi-threaded
/// runtime, so `block_in_place` can hand the thread's other tasks off before
/// this one blocks — which is what makes calling async Rust from a synchronous
/// foreign function safe rather than a nested-runtime panic.
pub(super) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    let handle = CURRENT.with(|slot| slot.borrow().as_ref().map(|a| a.handle.clone()));
    match handle {
        Some(handle) => tokio::task::block_in_place(|| handle.block_on(future)),
        None => unreachable!("a capability can only run while a program is executing"),
    }
}

/// A program sent to the worker thread.
struct Job {
    program: String,
    ctx: ScriptContext,
    reply: oneshot::Sender<Result<ScriptResult, ScriptError>>,
}

/// Executes model-generated Rune programs.
pub struct RuneScriptRuntime {
    jobs: mpsc::UnboundedSender<Job>,
    language: ScriptLanguage,
}

impl RuneScriptRuntime {
    /// Start the worker and build the Rune context.
    ///
    /// The context is built with stdio disabled, so a program has no ambient way
    /// to print or read the terminal: the only output channel is `dex.log`,
    /// which becomes a structured event. That is the "no ambient authority" rule
    /// enforced at the language level rather than by convention.
    pub fn start() -> Result<Arc<Self>, ScriptError> {
        let mut context = Context::with_config(false).map_err(|e| {
            ScriptError::Unavailable(format!("building the Rune context: {e}"))
        })?;
        install(&mut context)
            .map_err(|e| ScriptError::Unavailable(format!("installing capabilities: {e}")))?;

        // Built before the thread starts, then moved into it, so the worker has
        // it ready for the first job.
        let shared: Arc<RuntimeContext> = Arc::new(context.runtime().map_err(|e| {
            ScriptError::Unavailable(format!("preparing the Rune runtime: {e}"))
        })?);
        // The Context is kept alongside the RuntimeContext: the runtime executes
        // compiled units, but the compiler has to resolve `dex::` names against
        // it, and `Context::runtime` is not cheap enough to rebuild per program.
        let context = Arc::new(context);

        let (jobs, mut inbox) = mpsc::unbounded_channel::<Job>();

        std::thread::Builder::new()
            .name("dex-rune".to_string())
            .spawn(move || {
                // Multi-threaded, not current-thread: a capability binding is a
                // synchronous foreign function that has to run async Rust to
                // completion, and `block_in_place` needs a multi-threaded
                // runtime to hand the thread's other tasks off first.
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        tracing::error!(error = %e, "could not start the script worker runtime");
                        return;
                    }
                };
                let handle = runtime.handle().clone();
                while let Some(job) = inbox.blocking_recv() {
                    let result = runtime.block_on(run_one(
                        &shared,
                        &context,
                        handle.clone(),
                        job.program,
                        job.ctx,
                    ));
                    // The caller may have gone away; a dropped reply is not an
                    // error worth reporting.
                    let _ = job.reply.send(result);
                }
            })
            .map_err(|e| ScriptError::Unavailable(format!("starting the worker: {e}")))?;

        Ok(Arc::new(Self {
            jobs,
            language: ScriptLanguage {
                name: "rune",
                version: "0.14",
            },
        }))
    }
}

#[async_trait]
impl ScriptRuntime for RuneScriptRuntime {
    async fn execute(
        &self,
        program: &str,
        ctx: ScriptContext,
    ) -> Result<ScriptResult, ScriptError> {
        let (reply, rx) = oneshot::channel();
        self.jobs
            .send(Job {
                program: program.to_string(),
                ctx,
                reply,
            })
            .map_err(|_| ScriptError::Unavailable("the script worker has stopped".into()))?;
        rx.await
            .map_err(|_| ScriptError::Unavailable("the script worker dropped the job".into()))?
    }

    fn describe(&self) -> ScriptLanguage {
        self.language
    }
}

/// Run one program on the worker thread, publishing its context first.
async fn run_one(
    shared: &Arc<RuntimeContext>,
    context: &Context,
    handle: tokio::runtime::Handle,
    program: String,
    ctx: ScriptContext,
) -> Result<ScriptResult, ScriptError> {
    let started = std::time::Instant::now();

    // Publish the context for this run before the VM can call anything.
    let capability_ctx = ctx.capability_ctx()?;
    CURRENT.with(|slot| *slot.borrow_mut() = Some(Ambient { ctx: capability_ctx, handle }));

    let outcome = execute_inner(shared, context, &program, &ctx).await;

    // Clear it even on failure, so a later program cannot inherit a stale
    // context.
    CURRENT.with(|slot| *slot.borrow_mut() = None);

    let mut result = outcome?;
    result.duration_ms = started.elapsed().as_millis() as u64;
    Ok(result)
}

async fn execute_inner(
    shared: &Arc<RuntimeContext>,
    context: &Context,
    program: &str,
    ctx: &ScriptContext,
) -> Result<ScriptResult, ScriptError> {
    let unit = compile(context, program)?;

    let mut vm = Vm::new(shared.clone(), Arc::new(unit));
    // A Rune module holds only declarations, so a program has exactly one
    // entry point: `main`. Calling it rather than evaluating the module also
    // means its last expression is the program's result.
    let driving = async move {
        let value = vm.async_call(["main"], ()).await.map_err(vm_error)?;
        value_to_json(&value)
    };

    // One select, three outcomes. Dropping `driving` drops the VM, so a timeout
    // or a cancellation ends the program rather than leaking it.
    let instructions = ctx.limits.instructions;
    let value = tokio::select! {
        biased;
        () = ctx.cancel.cancelled() => return Err(ScriptError::Cancelled),
        () = tokio::time::sleep(ctx.limits.wall_clock) => {
            return Err(ScriptError::Timeout(ctx.limits.wall_clock));
        }
        // `budget::with` is Rune's own per-instruction budget: it wraps the
        // driving future and stops it once the allowance is spent. It is a much
        // tighter bound than a wall clock, which cannot interrupt a loop that
        // never awaits.
        result = budget::with(instructions as usize, driving) => result?,
    };

    Ok(ScriptResult::new(value))
}

/// Compile, turning a parse or type error into a structured compile failure
/// that carries the compiler's own diagnostics — which is what lets a model fix
/// its own code rather than just being told it failed.
fn compile(context: &Context, program: &str) -> Result<Unit, ScriptError> {
    let mut sources = Sources::new();
    // `Source::memory` takes the source text directly and is named "memory";
    // the model only ever sees compiler diagnostics, not this.
    let source = Source::memory(program).map_err(|e| ScriptError::Compile {
        message: e.to_string(),
        diagnostics: None,
    })?;
    sources.insert(source).map_err(|e| ScriptError::Compile {
        message: e.to_string(),
        diagnostics: None,
    })?;

    let mut diagnostics = Diagnostics::new();
    let built = rune::prepare(&mut sources)
        .with_context(context)
        .with_diagnostics(&mut diagnostics)
        .build();

    if !diagnostics.is_empty() {
        // Render into a buffer rather than a terminal: the text goes back to the
        // model so it can fix its own code, not to a human watching a screen.
        let mut buffer = rune::termcolor::Buffer::no_color();
        let _ = diagnostics.emit(&mut buffer, &sources);
        let text = String::from_utf8_lossy(buffer.as_slice()).into_owned();
        let headline = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("program did not compile")
            .to_string();
        return Err(ScriptError::Compile {
            message: headline,
            diagnostics: Some(text),
        });
    }

    built.map_err(|e| ScriptError::Compile {
        message: e.to_string(),
        diagnostics: None,
    })
}

/// Convert a returned Rune value to JSON.
///
/// Rune 0.14 has no `Value::to_json`, so the value is walked: the scalar
/// accessors each report the wrong type, and `downcast` identifies the two
/// container kinds.
fn value_to_json(value: &Value) -> Result<serde_json::Value, ScriptError> {
    if let Ok(b) = value.as_bool() {
        return Ok(serde_json::Value::Bool(b));
    }
    if let Ok(i) = value.as_signed() {
        return Ok(serde_json::Value::from(i));
    }
    if let Ok(u) = value.as_unsigned() {
        return Ok(serde_json::Value::from(u));
    }
    if let Ok(f) = value.as_float() {
        return Ok(serde_json::Value::from(f));
    }
    if let Ok(s) = value.borrow_string_ref() {
        return Ok(serde_json::Value::String(s.as_ref().to_string()));
    }
    if value.into_unit().is_ok() {
        return Ok(serde_json::Value::Null);
    }
    if let Ok(items) = value.clone().downcast::<rune::runtime::Vec>() {
        let mut out = Vec::with_capacity(items.len());
        for item in items.iter() {
            out.push(value_to_json(item)?);
        }
        return Ok(serde_json::Value::Array(out));
    }
    if let Ok(fields) = value.clone().downcast::<rune::runtime::Object>() {
        let mut out = serde_json::Map::new();
        for (key, item) in fields.iter() {
            out.insert(key.as_str().to_string(), value_to_json(item)?);
        }
        return Ok(serde_json::Value::Object(out));
    }
    // A program whose last expression is an unhandled `Result` ends up here.
    // That is a recoverable situation, not a reason to fail the turn: the
    // model is far better served by reading what the value was than by being
    // told it could not be represented. `Value` implements `Debug`, so the
    // error a program left unhandled arrives as text.
    Ok(serde_json::json!({
        "unhandled": format!("{value:?}"),
    }))
}

fn vm_error(e: VmError) -> ScriptError {
    ScriptError::Raised {
        message: e.to_string(),
    }
}


