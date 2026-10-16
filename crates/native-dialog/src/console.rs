//! Release a console created solely for an autostart daemon, not a shared shell.
use std::{fs::OpenOptions, io, os::windows::io::IntoRawHandle};

use windows_sys::Win32::System::Console::{
    FreeConsole, GetConsoleMode, GetConsoleProcessList, GetStdHandle, STD_ERROR_HANDLE,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};

/// A task-scheduler launch owns its console; a foreground CLI normally shares
/// its parent's. Detach only in the former case, leaving redirected IO intact.
pub fn detach_owned_console() -> io::Result<bool> {
    let mut process = 0;
    // A one-element buffer is sufficient: any larger result means a shared
    // console, and zero means no console (or that it cannot safely be inspected).
    let count = unsafe { GetConsoleProcessList(&mut process, 1) };
    if count != 1 || process != std::process::id() {
        return Ok(false);
    }
    let handles = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    let console_handles = handles.map(|handle| {
        let mut mode = 0;
        unsafe { GetConsoleMode(GetStdHandle(handle), &mut mode) != 0 }
    });
    // FreeConsole invalidates console handles. Replace those with NUL so Rust
    // stdout/stderr do not panic; pipes/files supplied by a caller stay intact.
    let null = if console_handles.contains(&true) {
        Some(OpenOptions::new().read(true).write(true).open("NUL")?)
    } else {
        None
    };
    if unsafe { FreeConsole() } == 0 {
        return Err(io::Error::last_os_error());
    }
    if let Some(null) = null {
        // Standard handles remain valid for the process lifetime. Windows closes
        // this handle on process exit; a local File must not close it on return.
        let raw = null.into_raw_handle();
        for (handle, replace) in handles.into_iter().zip(console_handles) {
            if replace && unsafe { SetStdHandle(handle, raw) } == 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, os::windows::process::CommandExt, process::Command};

    const MODE: &str = "LOCALSHELLD_CONSOLE_TEST_MODE";
    const PROBE: &str = "console::tests::console_probe";

    fn probe() -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", PROBE, "--nocapture"]);
        command
    }

    #[test]
    fn console_probe() {
        let Ok(mode) = std::env::var(MODE) else {
            return;
        };
        let mut process = 0;
        let count = unsafe { GetConsoleProcessList(&mut process, 1) };
        if mode == "none" {
            assert_eq!(count, 0);
            assert!(!detach_owned_console().unwrap());
        } else if mode == "windowless" {
            // CREATE_NO_WINDOW may still allocate a console without a window.
            assert!(count <= 1);
            assert_eq!(detach_owned_console().unwrap(), count == 1);
        } else if mode == "shared" {
            assert!(count > 1, "child must share its parent's console");
            assert!(!detach_owned_console().unwrap());
        } else {
            assert_eq!(mode, "owned");
            assert_eq!(count, 1);
            assert_eq!(process, std::process::id());
            let shared = probe().env(MODE, "shared").output().unwrap();
            assert!(shared.status.success(), "{shared:?}");
            assert!(String::from_utf8_lossy(&shared.stdout).contains("shared: retained"));

            // stdout is redirected by the launcher. Give stderr a real console
            // handle too, exercising both preservation and NUL replacement.
            let console = OpenOptions::new().write(true).open("CONOUT$").unwrap();
            assert_ne!(
                unsafe { SetStdHandle(STD_ERROR_HANDLE, console.into_raw_handle()) },
                0
            );
            assert!(detach_owned_console().unwrap());
            assert_eq!(unsafe { GetConsoleProcessList(&mut process, 1) }, 0);
            writeln!(io::stderr(), "detached console stderr remains writable").unwrap();
            assert!(!detach_owned_console().unwrap());
        }
        println!("{mode}: retained redirected stdout");
    }

    #[test]
    fn no_console_launch_keeps_redirected_output() {
        let output = probe()
            .env(MODE, "none")
            .creation_flags(0x00000008) // DETACHED_PROCESS
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("none: retained"));
    }

    #[test]
    fn windowless_launch_keeps_redirected_output() {
        let output = probe()
            .env(MODE, "windowless")
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("windowless: retained"));
    }

    #[test]
    fn own_console_is_released_but_shared_console_and_redirected_io_survive() {
        let directory = tempfile::tempdir().unwrap();
        let stdout = directory.path().join("stdout.log");
        let stderr = directory.path().join("stderr.log");
        let powershell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        // Start with a hidden, dedicated console like Task Scheduler. The
        // launcher is also hidden, so this regression test does not flash a UI.
        let output = Command::new(powershell)
            .env_remove("PSModulePath")
            .args(["-NoProfile", "-NonInteractive", "-Command", r#"
$ErrorActionPreference='Stop'
$child=Start-Process -FilePath $env:LOCALSHELLD_CONSOLE_TEST_EXE -ArgumentList @('--exact','console::tests::console_probe','--nocapture') -WindowStyle Hidden -Wait -PassThru -RedirectStandardOutput $env:LOCALSHELLD_CONSOLE_TEST_STDOUT -RedirectStandardError $env:LOCALSHELLD_CONSOLE_TEST_STDERR
exit $child.ExitCode
"#])
            .env(MODE, "owned")
            .env("LOCALSHELLD_CONSOLE_TEST_EXE", std::env::current_exe().unwrap())
            .env("LOCALSHELLD_CONSOLE_TEST_STDOUT", &stdout)
            .env("LOCALSHELLD_CONSOLE_TEST_STDERR", &stderr)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        let text = std::fs::read_to_string(stdout).unwrap_or_default();
        let errors = std::fs::read_to_string(stderr).unwrap_or_default();
        assert!(output.status.success(), "{output:?}\n{text}\n{errors}");
        assert!(text.contains("owned: retained redirected stdout"), "{text}");
    }
}
