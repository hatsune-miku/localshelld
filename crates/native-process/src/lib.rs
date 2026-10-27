//! Narrow Win32 boundary. Children are created suspended, assigned to a
//! kill-on-close job, then resumed. No child can run before containment.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
