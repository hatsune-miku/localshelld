#[cfg(unix)]
use std::process::Command;
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use super::checked;
#[cfg(target_os = "macos")]
use super::output;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Installed {
    pub executable: std::path::PathBuf,
    pub config: std::path::PathBuf,
    pub was_running: bool,
}

/// Registration metadata only. This can be inspected without opening the
/// protected configuration; callers must validate it before service changes.
pub(crate) fn discover(target: &Path) -> Result<Option<Installed>> {
    registration()?
        .map(|value| Installed::new(&value.executable, &value.config, target, value.was_running))
        .transpose()
}

impl Installed {
    fn new(executable: &Path, config: &Path, target: &Path, was_running: bool) -> Result<Self> {
        let executable = executable
            .canonicalize()
            .context("registered service binary is missing")?;
        ensure!(
            executable == target,
            "a system service is registered at {}; use the maintenance wizard to migrate it",
            executable.display()
        );
        let config = config.canonicalize()?;
        localshelld::config::Config::load(&config)?;
        Ok(Self {
            executable,
            config,
            was_running,
        })
    }
}

impl Installed {
    pub(crate) fn start_and_verify(&self, expected: Option<&str>) -> Result<()> {
        start()?;
        let config = localshelld::config::Config::load(&self.config)?;
        // The loopback health check must not follow redirects or use an HTTP proxy.
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()?;
        let url = format!("http://{}/health", config.bind);
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut healthy = 0;
        loop {
            let health = client
                .get(&url)
                .send()
                .and_then(|response| response.error_for_status())
                .ok()
                .and_then(|response| super::read_bounded(response, 4096).ok())
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
            let valid = health.is_some_and(|health| {
                health["service"] == "localshelld"
                    && health["status"] == "ok"
                    && expected.is_none_or(|version| health["version"] == version)
            });
            if valid && running()? {
                healthy += 1;
                if healthy >= 3 {
                    return Ok(());
                }
            } else {
                healthy = 0;
            }
            ensure!(
                Instant::now() < deadline,
                "service did not become healthy at the expected version"
            );
            thread::sleep(Duration::from_millis(300));
        }
    }
}

/// Validate the resolved file, not who owns it or where the user installed it.
/// Filesystem and service operations report any permissions required by the OS.
pub(super) fn validate_target(target: &Path) -> Result<()> {
    ensure!(
        target.is_absolute(),
        "update target must be an absolute path"
    );
    let metadata = fs::metadata(target)
        .with_context(|| format!("cannot inspect update target {}", target.display()))?;
    ensure!(metadata.is_file(), "update target must be a regular file");
    Ok(())
}

#[cfg(windows)]
fn open_service(
    access: windows_service::service::ServiceAccess,
) -> Result<Option<windows_service::service::Service>> {
    use windows_service::{
        Error,
        service_manager::{ServiceManager, ServiceManagerAccess},
    };
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    match manager.open_service("localshelld", access) {
        Ok(service) => Ok(Some(service)),
        Err(Error::Winapi(error)) if error.raw_os_error() == Some(1060) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
pub(crate) fn registration() -> Result<Option<Installed>> {
    use windows_service::service::{ServiceAccess, ServiceState};
    let Some(service) = open_service(ServiceAccess::QUERY_CONFIG | ServiceAccess::QUERY_STATUS)?
    else {
        return Ok(None);
    };
    // windows-service returns the complete ImagePath in executable_path.
    let config = service.query_config()?;
    let command = config
        .executable_path
        .to_str()
        .context("service ImagePath is not UTF-8")?;
    let (binary, config) = parse_windows_registration(command)?;
    let state = service.query_status()?.current_state;
    ensure!(
        matches!(state, ServiceState::Running | ServiceState::Stopped),
        "service is transitioning or paused; retry when running/stopped"
    );
    Ok(Some(Installed {
        executable: binary.into(),
        config: config.into(),
        was_running: state == ServiceState::Running,
    }))
}

#[cfg(any(windows, test))]
fn parse_windows_registration(command: &str) -> Result<(&str, &str)> {
    let value = command
        .strip_prefix('"')
        .and_then(|text| text.strip_suffix('"'))
        .context("unsupported service ImagePath; expected managed installation")?;
    let (binary, config) = value
        .split_once("\" service --config \"")
        .or_else(|| value.split_once("\" --config \""))
        .context("unsupported service ImagePath; expected managed installation")?;
    ensure!(
        !binary.contains('"') && !config.contains('"'),
        "unsupported quotes in ImagePath"
    );
    Ok((binary, config))
}

#[cfg(windows)]
fn running() -> Result<bool> {
    use windows_service::service::{ServiceAccess, ServiceState};
    Ok(open_service(ServiceAccess::QUERY_STATUS)?
        .context("service disappeared")?
        .query_status()?
        .current_state
        == ServiceState::Running)
}

#[cfg(windows)]
fn start() -> Result<()> {
    use windows_service::service::{ServiceAccess, ServiceState};
    let service = open_service(ServiceAccess::QUERY_STATUS | ServiceAccess::START)?
        .context("service disappeared")?;
    if service.query_status()?.current_state == ServiceState::Stopped {
        service.start::<&str>(&[])?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
const UNIT: &str = "/etc/systemd/system/localshelld.service";

#[cfg(target_os = "linux")]
pub(crate) fn registration() -> Result<Option<Installed>> {
    if !Path::new("/usr/bin/systemctl").try_exists()? && !Path::new(UNIT).try_exists()? {
        return Ok(None);
    }
    let load = systemctl(&[
        "show",
        "localshelld.service",
        "--property=LoadState",
        "--value",
    ])?;
    if load.trim() == "not-found" {
        return Ok(None);
    }
    ensure!(
        load.trim() == "loaded",
        "systemd service is not loaded normally"
    );
    let fragment = systemctl(&[
        "show",
        "localshelld.service",
        "--property=FragmentPath",
        "--value",
    ])?;
    let stale = systemctl(&[
        "show",
        "localshelld.service",
        "--property=NeedDaemonReload",
        "--value",
    ])?;
    ensure!(
        stale.trim() == "no",
        "systemd unit changed on disk; reload/reconcile it before updating"
    );
    let overrides = systemctl(&[
        "show",
        "localshelld.service",
        "--property=DropInPaths",
        "--value",
    ])?;
    ensure!(
        fragment.trim() == UNIT && overrides.trim().is_empty(),
        "custom systemd unit/drop-ins require manual updating"
    );
    let text = fs::read_to_string(UNIT)?;
    let (binary, config) = parse_systemd_registration(&text)?;
    let state = systemctl(&[
        "show",
        "localshelld.service",
        "--property=ActiveState",
        "--value",
    ])?;
    ensure!(
        matches!(state.trim(), "active" | "inactive" | "failed"),
        "service is transitioning; retry update"
    );
    Ok(Some(Installed {
        executable: binary.into(),
        config: config.into(),
        was_running: state.trim() == "active",
    }))
}

#[cfg(any(target_os = "linux", test))]
fn parse_systemd_registration(text: &str) -> Result<(String, String)> {
    let lines: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix("ExecStart="))
        .collect();
    ensure!(lines.len() == 1, "unsupported systemd service command");
    let (binary, config) = lines[0]
        .split_once(" serve --config ")
        .or_else(|| lines[0].split_once(" --config "))
        .context("unsupported systemd service command")?;
    let decode = |value: &str| -> Result<String> {
        let value = value.replace("\\x20", " ");
        ensure!(
            !value.contains(['\\', '%', '$', '"', '\'']),
            "custom systemd escaping requires manual updating"
        );
        Ok(value)
    };
    Ok((decode(binary)?, decode(config)?))
}

#[cfg(target_os = "linux")]
fn systemctl(args: &[&str]) -> Result<String> {
    checked(Command::new("/usr/bin/systemctl").args(args))
}

#[cfg(target_os = "linux")]
fn running() -> Result<bool> {
    Ok(systemctl(&[
        "show",
        "localshelld.service",
        "--property=ActiveState",
        "--value",
    ])?
    .trim()
        == "active")
}

#[cfg(target_os = "linux")]
fn start() -> Result<()> {
    systemctl(&["start", "--no-block", "localshelld.service"])?;
    Ok(())
}

#[cfg(target_os = "macos")]
const PLIST: &str = "/Library/LaunchDaemons/dev.localshelld.plist";
#[cfg(target_os = "macos")]
const LABEL: &str = "system/dev.localshelld";

#[cfg(target_os = "macos")]
fn launch_state() -> Result<Option<String>> {
    let result = output(Command::new("/bin/launchctl").args(["print", LABEL]))?;
    if result.status.success() {
        return Ok(Some(String::from_utf8(result.stdout)?));
    }
    ensure!(
        matches!(result.status.code(), Some(3 | 113)),
        "unable to inspect launchd service: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(None)
}

#[cfg(target_os = "macos")]
pub(crate) fn registration() -> Result<Option<Installed>> {
    if !Path::new(PLIST).try_exists()? {
        ensure!(
            launch_state()?.is_none(),
            "loaded service has no managed plist; update manually"
        );
        return Ok(None);
    }
    let value = plist::Value::from_file(PLIST)?;
    let dictionary = value.as_dictionary().context("invalid service plist")?;
    let args = dictionary
        .get("ProgramArguments")
        .and_then(plist::Value::as_array)
        .context("missing ProgramArguments")?;
    let args: Vec<_> = args
        .iter()
        .map(|arg| arg.as_string().context("non-string service argument"))
        .collect::<Result<_>>()?;
    ensure!(
        (args.len() == 4 && args[1] == "serve" && args[2] == "--config")
            || (args.len() == 3 && args[1] == "--config"),
        "custom launchd arguments require manual updating"
    );
    let state = launch_state()?;
    // An unloaded service stays unloaded. A loaded but non-running job may auto
    // start during replacement, so fail closed instead of altering its policy.
    let active = state
        .as_ref()
        .is_some_and(|text| text.lines().any(|line| line.trim() == "state = running"));
    ensure!(
        state.is_none() || active,
        "launchd job is loaded but not running; resolve its state before updating"
    );
    Ok(Some(Installed {
        executable: args[0].into(),
        config: args[args.len() - 1].into(),
        was_running: active,
    }))
}

#[cfg(target_os = "macos")]
fn running() -> Result<bool> {
    Ok(launch_state()?
        .is_some_and(|text| text.lines().any(|line| line.trim() == "state = running")))
}

#[cfg(target_os = "macos")]
fn start() -> Result<()> {
    if launch_state()?.is_none() {
        checked(Command::new("/bin/launchctl").args(["bootstrap", "system", PLIST]))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_managed_service_commands_without_shell_evaluation() {
        assert_eq!(parse_windows_registration(r#""C:\Program Files\trustd\trustd-system.exe" --config "C:\ProgramData\localshelld\config.toml""#).unwrap(),
            (r"C:\Program Files\trustd\trustd-system.exe", r"C:\ProgramData\localshelld\config.toml"));
        assert_eq!(
            parse_systemd_registration(
                "ExecStart=/opt/trustd/trustd-root --config /etc/localshelld/config.toml"
            )
            .unwrap(),
            (
                "/opt/trustd/trustd-root".into(),
                "/etc/localshelld/config.toml".into()
            )
        );
        assert_eq!(parse_windows_registration(r#""C:\Program Files\localshelld\localshelld.exe" service --config "C:\ProgramData\localshelld\config.toml""#).unwrap(),
            (r"C:\Program Files\localshelld\localshelld.exe", r"C:\ProgramData\localshelld\config.toml"));
        assert!(parse_windows_registration("localshelld service").is_err());
        assert_eq!(parse_systemd_registration("ExecStart=/opt/localshelld\\x20test/localshelld serve --config /etc/localshelld/config.toml").unwrap(),
            ("/opt/localshelld test/localshelld".into(), "/etc/localshelld/config.toml".into()));
        assert!(parse_systemd_registration("ExecStart=/bin/sh -c evil").is_err());
        assert!(
            parse_systemd_registration(
                "ExecStart=/opt/$USER/localshelld serve --config /etc/config"
            )
            .is_err()
        );
    }
}
