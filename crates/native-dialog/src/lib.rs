//! Win32 presentation only: no daemon connection, token, shell or authorization
//! policy. Console detachment touches standard handles and the NUL device only.
//! The caller supplies an in-process one-shot callback for desktop prompts.
//!
//! All FFI lives in this small crate so the application can retain its
//! `forbid(unsafe_code)` boundary. HWNDs, fonts and callback pointers never leave
//! the owning UI thread; the cross-thread sender transports owned Rust data.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(windows)]
mod console;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use console::detach_owned_console;
#[cfg(windows)]
pub use windows::{Choice, Event, EventLoop, Outcome, Request, Sender};
