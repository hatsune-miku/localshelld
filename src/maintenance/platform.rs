#[cfg(not(windows))]
use anyhow::Context;
use anyhow::{Result, ensure};
#[cfg(not(windows))]
use std::fs;
#[cfg(not(windows))]
use std::process::Command;
use std::{path::Path, path::PathBuf};

pub(super) fn owner() -> Result<String> {
    #[cfg(windows)]
    {
        Ok(localshelld_native_process::current_identity()?.sid)
    }
    #[cfg(unix)]
    {
        Ok(rustix::process::getuid().as_raw().to_string())
    }
}

pub(super) fn shell() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        Ok(localshelld_native_process::program_files_directory()?.join("PowerShell/7/pwsh.exe"))
    }
    #[cfg(not(windows))]
    {
        Ok(PathBuf::from("/bin/bash"))
    }
}

pub(super) fn elevation_available() -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new("/usr/bin/pkexec").is_file()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

pub(super) fn private(path: &Path, directory: bool) -> Result<()> {
    #[cfg(windows)]
    localshelld_native_process::restrict_path(
        path,
        directory,
        localshelld_native_process::FileAccess::Private,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
        )?;
    }
    Ok(())
}

pub(super) fn elevate(helper: &Path, plan: &Path, digest: &str) -> Result<()> {
    let arguments = vec![
        "maintenance-apply".into(),
        plan.to_string_lossy().into_owned(),
        digest.into(),
    ];
    #[cfg(windows)]
    {
        let status = localshelld_native_process::elevate_and_wait(helper, &arguments)?;
        ensure!(status == 0, "installation helper exited with code {status}");
    }
    #[cfg(target_os = "macos")]
    {
        let mut parts = vec![
            "/usr/bin/env".to_owned(),
            "-i".into(),
            "PATH=/usr/bin:/bin:/usr/sbin:/sbin".into(),
            helper.to_string_lossy().into_owned(),
        ];
        parts.extend(arguments);
        let command = parts
            .iter()
            .map(|value| shell_quote(value))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!(
            "do shell script {} with administrator privileges",
            apple_quote(&command)
        );
        let output = Command::new("/usr/bin/osascript")
            .args(["-e", &script])
            .output()?;
        ensure!(
            output.status.success(),
            "installation authorization/helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[cfg(target_os = "linux")]
    {
        let status = Command::new("/usr/bin/pkexec")
            .arg(helper)
            .args(arguments)
            .status()?;
        ensure!(
            status.success(),
            "installation authorization/helper failed ({status})"
        );
    }
    Ok(())
}

pub(super) fn open_browser(url: &str) -> Result<()> {
    #[cfg(windows)]
    localshelld_native_process::open_browser(url)?;
    #[cfg(target_os = "macos")]
    {
        Command::new("/usr/bin/open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("/usr/bin/xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

pub(super) fn shortcut(executable: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(executable.to_string_lossy().as_bytes());
        let script = format!(
            r#"$ErrorActionPreference='Stop'; $target=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}')); $link=Join-Path ([Environment]::GetFolderPath('Programs')) 'trustd.lnk'; $shortcut=(New-Object -ComObject WScript.Shell).CreateShortcut($link); $shortcut.TargetPath=$target; $shortcut.Arguments='maintenance'; $shortcut.WorkingDirectory=Split-Path -Parent $target; $shortcut.Save()"#
        );
        super::super::checked(
            super::super::windows_command("WindowsPowerShell/v1.0/powershell.exe")?.args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &script,
            ]),
        )?;
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::PermissionsExt;
        let base = directories::BaseDirs::new().context("missing user home")?;
        let app = base.home_dir().join("Applications/trustd.app/Contents");
        let directory = app.join("MacOS");
        fs::create_dir_all(&directory)?;
        let launcher = directory.join("trustd-maintenance");
        fs::write(
            &launcher,
            format!(
                "#!/bin/sh\nexec {} maintenance\n",
                shell_quote(&executable.to_string_lossy())
            ),
        )?;
        fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755))?;
        fs::write(
            app.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>dev.localshelld.maintenance</string><key>CFBundleName</key><string>trustd</string><key>CFBundleExecutable</key><string>trustd-maintenance</string><key>CFBundlePackageType</key><string>APPL</string><key>LSUIElement</key><true/></dict></plist>"#,
        )?;
    }
    #[cfg(target_os = "linux")]
    {
        let base = directories::BaseDirs::new().context("missing user data directory")?;
        let directory = base.data_dir().join("applications");
        fs::create_dir_all(&directory)?;
        let executable = executable
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%");
        fs::write(
            directory.join("dev.localshelld.maintenance.desktop"),
            format!(
                "[Desktop Entry]\nType=Application\nName=trustd\nExec=\"{executable}\" maintenance\nTerminal=false\nCategories=System;Settings;\n"
            ),
        )?;
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
#[cfg(any(target_os = "macos", test))]
fn apple_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn macos_elevation_quotes_paths_as_data_in_both_languages() {
        assert_eq!(shell_quote("/Users/A 'B/$tool"), "'/Users/A '\\''B/$tool'");
        assert_eq!(apple_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }
}
