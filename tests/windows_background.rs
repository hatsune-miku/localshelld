#![cfg(windows)]

use std::{os::windows::process::CommandExt, path::PathBuf, process::Command};

use localshelld::{config::Config, process::background_command, shell::Shell};

const PROBE_MODE: &str = "LOCALSHELLD_BACKGROUND_TEST";
const CONSOLE_PROBE: &str = r#"
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class ConsoleProbe {
    [DllImport("kernel32.dll")] public static extern IntPtr GetConsoleWindow();
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr window);
}
'@
if ([ConsoleProbe]::IsWindowVisible([ConsoleProbe]::GetConsoleWindow())) {
    throw 'background process has a visible console window'
}
"#;

fn powershell() -> PathBuf {
    PathBuf::from(std::env::var_os("SystemRoot").unwrap())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe")
}

/// A normal cargo test runner can own a console, masking missing creation flags.
/// Re-execute each probe with no console, like the detached user daemon.
fn in_detached_process(test: &str) -> bool {
    if std::env::var(PROBE_MODE).as_deref() == Ok(test) {
        return true;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(PROBE_MODE, test)
        .creation_flags(0x00000008) // DETACHED_PROCESS
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

#[test]
fn background_helpers_hide_console_and_preserve_output_and_exit_code() {
    if !in_detached_process("background_helpers_hide_console_and_preserve_output_and_exit_code") {
        return;
    }
    let script = format!(
        "{CONSOLE_PROBE}\n[Console]::Out.WriteLine('captured stdout'); [Console]::Error.WriteLine('captured stderr'); exit 7"
    );
    let output = background_command(powershell())
        .env_remove("PSModulePath")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "captured stdout"
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim(),
        "captured stderr"
    );
}

#[test]
fn shell_and_native_children_have_no_visible_console() {
    if !in_detached_process("shell_and_native_children_have_no_visible_console") {
        return;
    }
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let shell = Shell::spawn("pwsh", 8192).await.unwrap();
        let result = shell.execute(CONSOLE_PROBE, Some(20)).await.unwrap();
        assert!(result.success, "{}", result.output);
        // Native children must also inherit a windowless execution environment.
        let encoded = {
            use base64::Engine;
            let bytes: Vec<u8> = format!("{CONSOLE_PROBE}\n'child output'")
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect();
            base64::engine::general_purpose::STANDARD.encode(bytes)
        };
        let result = shell
            .execute(
                &format!("pwsh -NoProfile -NonInteractive -EncodedCommand {encoded}"),
                Some(20),
            )
            .await
            .unwrap();
        assert!(result.success, "{}", result.output);
        assert!(result.output.contains("child output"), "{}", result.output);
        let result = shell.execute("cmd /c exit 7", Some(20)).await.unwrap();
        assert_eq!(result.exit_code, Some(7));
        shell.terminate().await;
    });
}

#[test]
fn private_writes_keep_restrictive_directory_and_file_permissions() {
    if !in_detached_process("private_writes_keep_restrictive_directory_and_file_permissions") {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("private/config.toml");
    let mut config = Config {
        password_hash: "test hash".into(),
        ..Config::default()
    };
    for password in ["first value", "replacement value"] {
        config.password_hash = password.into();
        config.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().password_hash, password);
        let output = background_command(powershell())
            .env_remove("PSModulePath")
            .args(["-NoProfile", "-NonInteractive", "-Command", r#"
$ErrorActionPreference = 'Stop'
$owner = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$expected = @($owner, 'S-1-5-18', 'S-1-5-32-544') | Select-Object -Unique
foreach ($path in @($env:LOCALSHELLD_TEST_PRIVATE_FILE, (Split-Path -LiteralPath $env:LOCALSHELLD_TEST_PRIVATE_FILE))) {
    $acl = Get-Acl -LiteralPath $path
    if (-not $acl.AreAccessRulesProtected) { throw 'ACL inheritance is enabled' }
    $rules = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))
    foreach ($rule in $rules) {
        if ($rule.IsInherited -or $rule.AccessControlType -ne 'Allow' -or $rule.FileSystemRights -ne 'FullControl') { throw 'unexpected access rule' }
        if ($rule.IdentityReference.Value -notin $expected) { throw 'unexpected principal' }
    }
    foreach ($sid in $expected) {
        if ($sid -notin $rules.IdentityReference.Value) { throw 'missing private access rule' }
    }
}
'private ACLs retained'
"#])
            .env("LOCALSHELLD_TEST_PRIVATE_FILE", &path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("private ACLs retained"));
    }
}
