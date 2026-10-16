//! Background child processes must not allocate a visible Windows console.
use std::{ffi::OsStr, process::Command};

/// Keep normal redirected/inherited IO, but never create a console window for
/// an internal helper. This also works after the user daemon calls FreeConsole.
pub fn background_command(program: impl AsRef<OsStr>) -> Command {
    let command = Command::new(program);
    #[cfg(windows)]
    let command = {
        use std::os::windows::process::CommandExt;
        let mut command = command;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        command
    };
    command
}
