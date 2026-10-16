use anyhow::{Context, Result, ensure};
use base64::Engine;

const OPERATIONS: &str = include_str!("windows_uninstall.ps1");

pub(super) fn uninstall() -> Result<()> {
    let sid = super::sid()?;
    // sid() validates the character set; no user-controlled path is interpolated.
    let result = run_script(&format!(
        r#"
$service = New-Object -ComObject Schedule.Service
$service.Connect()
$folder = $service.GetFolder('\')
Remove-LocalshelldUserTask $folder 'localshelld-user-{sid}' '{sid}'
"#
    ))?;
    ensure!(
        matches!(result.trim(), "removed" | "missing"),
        "unexpected task removal result"
    );
    Ok(())
}

fn run_script(body: &str) -> Result<String> {
    let script = format!(
        r#"
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$ErrorActionPreference = 'Stop'
{OPERATIONS}
try {{
{body}
}} catch {{
    $exception = $_.Exception.GetBaseException()
    [Console]::Error.WriteLine(('{{0}} (HRESULT 0x{{1}})' -f $exception.Message, $exception.HResult.ToString('X8')))
    if ($exception.HResult -eq -2147024891) {{
        [Console]::Error.WriteLine('Access denied: rerun uninstall from an Administrator terminal of the SAME logged-in user (or use sudo).')
    }}
    exit 1
}}
"#
    );
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let output = super::super::windows_command("WindowsPowerShell/v1.0/powershell.exe")?
        .env_remove("PSModulePath")
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded])
        .output()
        .context("failed to run the Windows task removal helper")?;
    let errors =
        String::from_utf8(output.stderr).context("task helper returned non-UTF-8 diagnostics")?;
    ensure!(
        output.status.success(),
        "could not remove user autostart: {}",
        errors.trim()
    );
    String::from_utf8(output.stdout).context("task helper returned non-UTF-8 output")
}

#[cfg(test)]
mod tests;
