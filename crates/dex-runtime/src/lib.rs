//! DEX runtime.
//!
//! The runtime owns everything the model can influence indirectly: the model
//! provider, the script runtime that executes generated programs, the capability
//! registry those programs call into, authorization, memory, and the IPC server
//! frontends connect to. A frontend never reaches past this boundary.
//!
//! The organising principle is that the model does not call tools; it programs
//! the runtime. Capabilities and the script language are separate concerns:
//! `script::ScriptRuntime` is the seam, and only `script::rune` knows which
//! language is in use.

pub mod auth;
pub mod budget;
pub mod capability;
pub mod config;
pub mod events;
pub mod memory;
pub mod provider;
pub mod script;
pub mod server;
pub mod session;

pub use config::RuntimeConfig;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
