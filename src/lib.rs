#![forbid(unsafe_code)]

pub mod assets;
pub mod auth;
pub(crate) mod bridge;
pub mod config;
pub mod consent;
pub mod desktop;
pub mod execution;
pub(crate) mod management;
pub mod mcp;
mod mcp_text;
mod peer;
pub mod process;
pub(crate) mod secrets;
pub mod server;
pub(crate) mod settings;
pub mod shell;
pub(crate) mod storage;
pub(crate) mod sync;

pub const VERSION: &str = env!("LOCALSHELLD_VERSION");
pub const COMMIT: &str = env!("LOCALSHELLD_COMMIT");
