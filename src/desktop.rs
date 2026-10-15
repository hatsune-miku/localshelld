//! Platform presentation adapters. Authorization, deadlines and remembered
//! choices stay in `consent` / `server`, independently of the window toolkit.
#[cfg(not(windows))]
mod webview;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
pub use webview::{Desktop, available};
#[cfg(windows)]
pub use windows::{Desktop, available};
