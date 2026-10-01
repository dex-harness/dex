//! `dexd` — the DEX runtime daemon.
//!
//! Loads configuration, builds the one long-lived script worker, binds the
//! socket, and serves until it is asked to stop. Everything else lives in the
//! library, so this file stays a wiring diagram rather than a place where
//! behaviour hides.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

use dex_runtime::config;
use dex_runtime::memory::MemoryStore;
use dex_runtime::provider::openai_compat::OpenAiCompatible;
use dex_runtime::provider::Provider;
use dex_runtime::script::rune::RuneScriptRuntime;
use dex_runtime::script::ScriptRuntime;
use dex_runtime::server;
use dex_runtime::session::{SessionDeps, SessionManager};

#[derive(Parser, Debug)]
#[command(name = "dexd", version, about = "The DEX runtime")]
struct Args {
    /// Configuration file. Defaults to `.env` in the current directory.
    #[arg(long, default_value = ".env")]
    env_file: PathBuf,

    /// Override the socket path from the environment.
    #[arg(long)]
    socket: Option<PathBuf>,

    /// Print the effective configuration and exit.
    #[arg(long)]
    check: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,dex_runtime=debug".into()),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    let mut config = config::load(Some(&args.env_file))?;
    if let Some(socket) = args.socket {
        config.socket_path = socket;
    }

    if args.check {
        report(&config);
        return Ok(());
    }

    // One script worker for the process. Each run gets a fresh VM, so a shared
    // worker costs nothing in isolation and keeps one session's programs from
    // outliving their turn on the heap.
    let script: Arc<dyn ScriptRuntime> = RuneScriptRuntime::start()?;
    let provider: Arc<dyn Provider> = Arc::new(OpenAiCompatible::new(config.provider.clone()));
    let memory = Arc::new(MemoryStore::new(config.memory_dir.clone()));

    let manager = SessionManager::new(SessionDeps {
        config: config.clone(),
        provider,
        script,
        memory,
    });

    let (listener, bound) = server::bind_socket(&config.socket_path)?;
    tracing::info!(
        socket = %bound.path.display(),
        model = %config.provider.model,
        base_url = %config.provider.base_url,
        grants = config.authority.describe().len(),
        budget = %config.budget.describe(),
        "dexd listening"
    );

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("shutting down");
        }
        let _ = stop.send(());
    });
    // A terminate signal ends immediately, so a wedged shutdown is never a trap.
    tokio::spawn(async move {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            term.recv().await;
            tracing::info!("terminated");
            std::process::exit(0);
        }
    });

    server::serve(listener, manager, async {
        let _ = stopped.await;
    })
    .await;

    server::cleanup(&bound);
    Ok(())
}

/// Print the effective configuration, including what the session may actually do.
fn report(config: &config::RuntimeConfig) {
    println!("socket        {}", config.socket_path.display());
    println!("base_url      {}", config.provider.base_url);
    println!("model         {}", config.provider.model);
    println!("memory        {}", config.memory_dir.display());
    println!("budget        {}", config.budget.describe());
    println!("max rounds    {}", config.max_program_rounds);
    println!("max sessions  {}", config.max_sessions);
    let authorities = config.authority.describe();
    if authorities.is_empty() {
        println!("authority     (none granted; every capability will be refused)");
    } else {
        println!("authority");
        for grant in authorities {
            println!("  {:<22} {}", grant.capability, grant.scope);
        }
    }
}
