//! Win32 presentation only: no daemon connection, token, shell, file IO or
//! authorization policy. The caller supplies an in-process one-shot callback.
//!
//! All FFI lives in this small crate so the application can retain its
//! `forbid(unsafe_code)` boundary. HWNDs, fonts and callback pointers never leave
//! the owning UI thread; the cross-thread sender transports owned Rust data.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Choice, Event, EventLoop, Outcome, Request, Sender};
