//! Login-scoped autostart. Daemons always use the non-elevated login identity.
//! On Windows the same user's elevated installer may register the limited task.
#[cfg(unix)]
use std::process::Command;
use std::{fs, path::Path};

use anyhow::{Context, Result, bail, ensure};

#[cfg(windows)]
mod windows_uninstall;

pub fn install(config: &Path) -> Result<()> {
    install_at(&super::bundle::sibling(super::bundle::USER)?, config)
}

pub(crate) fn install_at(executable: &Path, config: &Path) -> Result<()> {
    let config = config
        .canonicalize()
        .context("failed to resolve user configuration")?;
    let loaded = localshelld::config::Config::load(&config)?;
    if loaded.bind == loaded.privileged_daemon {
        bail!("user and privileged daemon addresses must differ");
    }
    for path in [executable, &config] {
        let value = path
            .to_str()
            .context("service paths must be valid Unicode")?;
        if value.chars().any(|ch| ch.is_control()) {
            bail!("service paths must not contain control characters");
        }
    }
    install_native(executable, &config)?;
    println!("trustd user daemon installed and started for this user.");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    // Running the daemon must remain non-elevated, but an administrator-owned
    // Task Scheduler registration can require this same user's elevated token.
    #[cfg(windows)]
    require_interactive_owner()?;
    #[cfg(unix)]
    super::require_user()?;
    uninstall_native()?;
    println!(
        "localshelld user autostart removed (or already absent). Configuration, seal.key and authorization data were preserved."
    );
    Ok(())
}

/// A responding port alone is insufficient: a foreground process might already
/// own it while the newly installed autostart daemon failed to start.
pub fn verify_running(config: &Path) -> Result<()> {
    verify_running_at(&super::bundle::sibling(super::bundle::USER)?, config)
}

pub(crate) fn verify_running_at(executable: &Path, config: &Path) -> Result<()> {
    #[cfg(windows)]
    let running = {
        let sid = sid()?;
        let document = scheduled_task(executable, &config.canonicalize()?, &sid);
        existing_task(&format!("localshelld-user-{sid}"), &document)? == "running"
    };
    #[cfg(target_os = "linux")]
    let running = {
        let _ = (config, executable);
        Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", "localshelld-user.service"])
            .status()?
            .success()
    };
    #[cfg(target_os = "macos")]
    let running = {
        let _ = (config, executable);
        let output = Command::new("/bin/launchctl")
            .args(["print", &format!("{}/dev.localshelld.user", domain()?)])
            .output()?;
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim() == "state = running")
    };
    ensure!(
        running,
        "the user autostart task/service is not running; another process may own its port; installation is incomplete"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn home() -> Result<std::path::PathBuf> {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_owned())
        .context("unable to determine the current user's home directory")
}

#[cfg(target_os = "linux")]
fn unit_path() -> Result<std::path::PathBuf> {
    let dirs =
        directories::BaseDirs::new().context("unable to find user configuration directory")?;
    Ok(dirs
        .config_dir()
        .join("systemd/user/localshelld-user.service"))
}

#[cfg(any(target_os = "linux", test))]
fn systemd_unit(executable: &Path, config: &Path) -> String {
    let quote = |path: &Path| {
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    };
    format!(
        "[Unit]\nDescription=trustd current-user daemon\n\n[Service]\nExecStart=\"{}\" --config \"{}\"\nRestart=on-failure\nRestartSec=3\n\n[Install]\nWantedBy=default.target\n",
        quote(executable),
        quote(config)
    )
}

#[cfg(target_os = "linux")]
fn install_native(executable: &Path, config: &Path) -> Result<()> {
    let path = unit_path()?;
    let document = systemd_unit(executable, config);
    if path.exists() {
        ensure!(
            fs::read_to_string(&path)? == document,
            "existing user service has different paths/settings; registration was preserved"
        );
    } else {
        write_new_registration(&path, document.as_bytes())?;
    }
    super::checked(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    super::checked(Command::new("systemctl").args([
        "--user",
        "enable",
        "--now",
        "localshelld-user.service",
    ]))
}

#[cfg(target_os = "linux")]
fn uninstall_native() -> Result<()> {
    let path = unit_path()?;
    if path.exists() {
        super::checked(Command::new("systemctl").args([
            "--user",
            "disable",
            "--now",
            "localshelld-user.service",
        ]))?;
        fs::remove_file(path)?;
        super::checked(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    }
    Ok(())
}

#[cfg(any(target_os = "macos", windows, test))]
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "macos")]
fn agent_path() -> Result<std::path::PathBuf> {
    Ok(home()?.join("Library/LaunchAgents/dev.localshelld.user.plist"))
}

#[cfg(target_os = "macos")]
fn domain() -> Result<String> {
    let result = Command::new("/usr/bin/id").arg("-u").output()?;
    let uid = String::from_utf8(result.stdout)?.trim().parse::<u32>()?;
    Ok(format!("gui/{uid}"))
}

#[cfg(any(target_os = "macos", test))]
fn launch_agent(executable: &Path, config: &Path, directory: &Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>dev.localshelld.user</string>
<key>ProgramArguments</key><array><string>{}</string><string>--config</string><string>{}</string></array>
<key>WorkingDirectory</key><string>{}</string>
<key>EnvironmentVariables</key><dict><key>PATH</key><string>/usr/bin:/bin:/usr/sbin:/sbin:/usr/local/bin:/opt/homebrew/bin</string></dict>
<key>RunAtLoad</key><true/><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
<key>ThrottleInterval</key><integer>3</integer>
</dict></plist>
"#,
        xml(&executable.to_string_lossy()),
        xml(&config.to_string_lossy()),
        xml(&directory.to_string_lossy())
    )
}

#[cfg(target_os = "macos")]
fn install_native(executable: &Path, config: &Path) -> Result<()> {
    let path = agent_path()?;
    let document = launch_agent(executable, config, &home()?);
    if path.exists() {
        ensure!(
            fs::read_to_string(&path)? == document,
            "existing user LaunchAgent has different paths/settings; registration was preserved"
        );
    } else {
        write_new_registration(&path, document.as_bytes())?;
    }
    let domain = domain()?;
    let label = format!("{domain}/dev.localshelld.user");
    let existing = Command::new("/bin/launchctl")
        .args(["print", &label])
        .output()?;
    if existing.status.success() {
        if String::from_utf8_lossy(&existing.stdout)
            .lines()
            .any(|line| line.trim() == "state = running")
        {
            return Ok(());
        }
        return super::checked(Command::new("/bin/launchctl").args(["kickstart", &label]));
    }
    super::checked(
        Command::new("/bin/launchctl")
            .args(["bootstrap", &domain])
            .arg(path),
    )
}

#[cfg(unix)]
fn write_new_registration(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    fs::create_dir_all(path.parent().context("missing autostart directory")?)?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(bytes)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_native() -> Result<()> {
    let path = agent_path()?;
    if path.exists() {
        let output = Command::new("/bin/launchctl")
            .args(["bootout", &format!("{}/dev.localshelld.user", domain()?)])
            .output()?;
        if !output.status.success() && output.status.code() != Some(3) {
            bail!("could not unload user LaunchAgent; plist preserved");
        }
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(windows)]
fn sid() -> Result<String> {
    let output = super::windows_command("WindowsPowerShell/v1.0/powershell.exe")?
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[Security.Principal.WindowsIdentity]::GetCurrent().User.Value",
        ])
        .output()?;
    let sid = String::from_utf8(output.stdout)?.trim().to_owned();
    if !output.status.success()
        || !sid.starts_with("S-1-")
        || !sid
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'S' || b == b'-')
    {
        bail!("unable to determine current user SID");
    }
    Ok(sid)
}

#[cfg(windows)]
pub fn require_interactive_owner() -> Result<()> {
    let output = super::windows_command("WindowsPowerShell/v1.0/powershell.exe")?
        .args(["-NoProfile", "-NonInteractive", "-Command",
            "$identity=[Security.Principal.WindowsIdentity]::GetCurrent(); @{sid=$identity.User.Value; session=[Diagnostics.Process]::GetCurrentProcess().SessionId} | ConvertTo-Json -Compress"])
        .output()?;
    ensure!(
        output.status.success(),
        "unable to inspect the Windows user-management identity"
    );
    let identity: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        interactive_owner(
            identity["sid"].as_str().unwrap_or_default(),
            identity["session"].as_u64().unwrap_or_default()
        ),
        "user daemon management requires the intended logged-in account (standard or Administrator), not LocalSystem/Session 0; use --system-only on headless hosts"
    );
    Ok(())
}

#[cfg(any(windows, test))]
fn interactive_owner(sid: &str, session: u64) -> bool {
    session != 0
        && sid.starts_with("S-1-")
        && !matches!(sid, "S-1-5-18" | "S-1-5-19" | "S-1-5-20")
        && sid
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'S' || b == b'-')
}

#[cfg(any(windows, test))]
fn scheduled_task(executable: &Path, config: &Path, sid: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
<Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{sid}</UserId></LogonTrigger></Triggers>
<Principals><Principal id="User"><UserId>{sid}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
<Settings><MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy><DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries><StopIfGoingOnBatteries>false</StopIfGoingOnBatteries><ExecutionTimeLimit>PT0S</ExecutionTimeLimit><RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure></Settings>
<Actions Context="User"><Exec><Command>{}</Command><Arguments>--config &quot;{}&quot;</Arguments></Exec></Actions>
</Task>
"#,
        xml(&executable.to_string_lossy()),
        xml(&config.to_string_lossy()),
        sid = xml(sid)
    )
}

#[cfg(windows)]
fn install_native(executable: &Path, config: &Path) -> Result<()> {
    let sid = sid()?;
    let task = format!("localshelld-user-{sid}");
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("task.xml");
    let document = scheduled_task(executable, config, &sid);
    let existing = existing_task(&task, &document)?;
    if existing == "running" {
        return Ok(());
    }
    let bytes: Vec<u8> = std::iter::once(0xfeff_u16)
        .chain(document.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect();
    fs::write(&path, bytes)?;
    if existing == "missing" {
        super::checked(
            super::windows_command("schtasks.exe")?
                .args(["/Create", "/TN", &task, "/XML"])
                .arg(path),
        )?;
    }
    super::checked(super::windows_command("schtasks.exe")?.args(["/Run", "/TN", &task]))
}

#[cfg(windows)]
fn existing_task(name: &str, expected_xml: &str) -> Result<String> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(expected_xml.as_bytes());
    let validation = task_validation_script();
    // The task name is built only from the validated current SID. The XML travels
    // as base64, not interpolated paths, and this query never changes a task.
    let script = format!(
        r#"
$ErrorActionPreference='Stop'
$service=New-Object -ComObject Schedule.Service
$service.Connect()
try {{ $existing=$service.GetFolder('\').GetTask('{name}') }} catch {{
    if ($_.Exception.HResult -eq -2147024894 -or $_.Exception.InnerException.HResult -eq -2147024894) {{ 'missing'; exit 0 }}
    throw
}}
$expected=$service.NewTask(0)
$expected.XmlText=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}'))
{validation}
Confirm-UserTask $existing.Definition $expected
if ($existing.State -eq 4) {{ 'running' }} else {{ 'stopped' }}
"#
    );
    let output = super::windows_command("WindowsPowerShell/v1.0/powershell.exe")?
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()?;
    ensure!(
        output.status.success(),
        "could not inspect user autostart task: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let state = String::from_utf8(output.stdout)?.trim().to_owned();
    ensure!(
        matches!(state.as_str(), "missing" | "stopped" | "running"),
        "unexpected user task query response"
    );
    Ok(state)
}

#[cfg(windows)]
fn task_validation_script() -> &'static str {
    r#"
function Get-UserSid([string]$value) {
    if ($value.StartsWith('S-1-')) { return $value }
    return ([Security.Principal.NTAccount]$value).Translate([Security.Principal.SecurityIdentifier]).Value
}
function Confirm-UserTask($actual, $expected) {
    if ((Get-UserSid $actual.Principal.UserId) -ne (Get-UserSid $expected.Principal.UserId) -or $actual.Principal.RunLevel -ne 0 -or $actual.Principal.LogonType -ne 3 -or $actual.Actions.Count -ne 1 -or $actual.Actions.Item(1).Path -ne $expected.Actions.Item(1).Path -or $actual.Actions.Item(1).Arguments -cne $expected.Actions.Item(1).Arguments) { throw 'Existing user task has different identity, privileges, or paths; registration was preserved.' }
    if (-not $actual.Settings.Enabled -or $actual.Triggers.Count -ne 1 -or $actual.Triggers.Item(1).Type -ne $expected.Triggers.Item(1).Type -or -not $actual.Triggers.Item(1).Enabled -or (Get-UserSid $actual.Triggers.Item(1).UserId) -ne (Get-UserSid $expected.Triggers.Item(1).UserId)) { throw 'Existing user task is not enabled for the intended user login; registration was preserved.' }
}
"#
}

#[cfg(windows)]
fn uninstall_native() -> Result<()> {
    windows_uninstall::uninstall()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_accounts_and_session_zero_are_not_user_installation_targets() {
        assert!(interactive_owner("S-1-5-21-1234-1001", 1));
        for sid in ["S-1-5-18", "S-1-5-19", "S-1-5-20", "invalid"] {
            assert!(!interactive_owner(sid, 1));
        }
        assert!(!interactive_owner("S-1-5-21-1234-1001", 0));
    }

    #[test]
    fn autostart_uses_user_identity_and_escaped_paths() {
        let exe = Path::new("/home/A & B/localshelld");
        let config = Path::new("/home/A & B/config.toml");
        let task = scheduled_task(exe, config, "S-1-5-21-123");
        assert!(task.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(task.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(task.contains("A &amp; B"));
        assert!(task.contains("--config"));
        let agent = launch_agent(exe, config, Path::new("/home/A & B"));
        assert!(!agent.contains("<key>UserName</key>"));
        assert!(!agent.contains("<string>--user</string>"));
        assert!(agent.contains("<string>--config</string>"));
        assert!(agent.contains("A &amp; B"));
        let unit = systemd_unit(Path::new("/home/a%u/$bin"), config);
        assert!(unit.contains("a%%u/$$bin"));
        assert!(unit.contains("--config \"/home/A & B/config.toml\""));
    }

    #[cfg(windows)]
    #[test]
    fn task_scheduler_accepts_user_task_xml_without_registering() {
        use base64::Engine;
        let document = scheduled_task(
            Path::new(r"C:\Program Files\localshelld\localshelld.exe"),
            Path::new(r"C:\Users\A & B\config.toml"),
            &sid().unwrap(),
        );
        let encoded = base64::engine::general_purpose::STANDARD.encode(document.as_bytes());
        let script = format!(
            "$ErrorActionPreference='Stop'; $service=New-Object -ComObject Schedule.Service; $service.Connect(); $task=$service.NewTask(0); $task.XmlText=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}')); if ($task.Principal.RunLevel -ne 0) {{ throw 'task requests elevation' }}"
        );
        let output = super::super::windows_command("WindowsPowerShell/v1.0/powershell.exe")
            .unwrap()
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(windows)]
    #[test]
    fn missing_user_autostart_query_does_not_register_a_task() {
        let identity = sid().unwrap();
        let name = format!("localshelld-read-only-test-{}", uuid::Uuid::new_v4());
        let document = scheduled_task(
            Path::new(r"C:\localshelld.exe"),
            Path::new(r"C:\config.toml"),
            &identity,
        );
        assert_eq!(existing_task(&name, &document).unwrap(), "missing");
        assert_eq!(existing_task(&name, &document).unwrap(), "missing");
    }

    #[cfg(windows)]
    #[test]
    fn task_reuse_checks_identity_privileges_paths_and_login_start_without_registering() {
        use base64::Engine;
        let document = scheduled_task(
            Path::new(r"C:\localshelld.exe"),
            Path::new(r"C:\config.toml"),
            &sid().unwrap(),
        );
        let encoded = base64::engine::general_purpose::STANDARD.encode(document.as_bytes());
        let validation = task_validation_script();
        let script = format!(
            r#"
$ErrorActionPreference='Stop'
$service=New-Object -ComObject Schedule.Service
$service.Connect()
$expected=$service.NewTask(0)
$expected.XmlText=[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}'))
{validation}
$actual=$service.NewTask(0)
$actual.XmlText=$expected.XmlText
Confirm-UserTask $actual $expected
foreach ($mutation in @({{$actual.Principal.RunLevel=1}}, {{$actual.Actions.Item(1).Arguments='serve --config wrong.toml'}}, {{$actual.Triggers.Item(1).Enabled=$false}}, {{$actual.Settings.Enabled=$false}}, {{$actual.Principal.UserId='S-1-5-18'}})) {{
    $actual.XmlText=$expected.XmlText
    & $mutation
    $rejected=$false
    try {{ Confirm-UserTask $actual $expected }} catch {{ $rejected=$true }}
    if (-not $rejected) {{ throw 'Changed autostart definition was incorrectly reused' }}
}}
"#
        );
        let output = super::super::windows_command("WindowsPowerShell/v1.0/powershell.exe")
            .unwrap()
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn user_agent_is_a_valid_plist_without_root_identity() {
        let document = launch_agent(
            Path::new("/Applications/A & B/localshelld"),
            Path::new("/Users/test/config.toml"),
            Path::new("/Users/test"),
        );
        let parsed = plist::Value::from_reader(std::io::Cursor::new(document.as_bytes())).unwrap();
        let dictionary = parsed.as_dictionary().unwrap();
        assert_eq!(
            dictionary["Label"].as_string(),
            Some("dev.localshelld.user")
        );
        assert!(!dictionary.contains_key("UserName"));
    }
}
