#![forbid(unsafe_code)]

mod api;
pub mod assets;
pub mod auth;
pub(crate) mod bridge;
pub mod config;
pub mod consent;
pub mod daemon;
pub mod desktop;
pub mod execution;
pub(crate) mod management;
pub mod mcp;
mod mcp_text;
pub mod paths;
mod peer;
pub mod process;
pub(crate) mod secrets;
pub mod server;
pub(crate) mod settings;
pub mod shell;
pub mod stdio;
pub(crate) mod storage;
pub(crate) mod sync;
pub mod telegram;
#[cfg(windows)]
mod windows_service_host;

pub const VERSION: &str = env!("LOCALSHELLD_VERSION");
pub const COMMIT: &str = env!("LOCALSHELLD_COMMIT");
