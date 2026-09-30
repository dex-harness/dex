//! DEX runtime.
//!
//! The runtime owns everything the model can influence indirectly: the model
//! provider, the script runtime that executes generated programs, the capability
//! registry those programs call into, authorization, memory, and the IPC server
//! frontends connect to. A frontend never reaches past this boundary.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
