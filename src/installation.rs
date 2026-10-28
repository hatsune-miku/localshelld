//! Eager installation of both identities. Establishing trust remains a UI action.
use std::{
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use localshelld::config::{Config, user_config_path};

pub fn install(system_path: PathBuf, user_path: Option<PathBuf>, system_only: bool) -> Result<()> {
    ensure!(
        super::is_elevated()?,
        "service installation requires Administrator/root"
    );
    let system_path = system_path
        .canonicalize()
        .context("system configuration is not initialized")?;
    let system = Config::load(&system_path)?;
    if system_only {
        super::install_system(system_path)?;
        wait_healthy(system.bind, "privileged", Duration::from_secs(30))?;
        println!(
            "System-only installation: http://localhost:{}/",
            system.bind.port()
        );
        return Ok(());
    }

    let owner = LoginOwner::detect()?;
    // Resolve relative paths before sudo changes HOME/working-directory context.
    let user_path = user_path.map(std::path::absolute).transpose()?;
    if let Some(path) = &user_path {
        ensure!(
            std::path::absolute(path)? != system_path
                && path.canonicalize().ok().as_ref() != Some(&system_path),
            "system and user configuration files must be different"
        );
    }
    owner
        .setup(user_path.as_deref(), system.bind, false)
        .context("user configuration preparation failed; services were not installed")?;
    super::install_system(system_path)?;
    wait_healthy(system.bind, "privileged", Duration::from_secs(30))?;
    owner.setup(user_path.as_deref(), system.bind, true).context(
        "system service is running, but user installation/startup failed; fix the reported error and rerun install (existing configuration is preserved)",
    )?;
    println!(
        "Both daemons are running. Existing trust was preserved; no new pairing was created automatically.\nIf not already paired, generate a code in the user management page, then complete pairing at http://localhost:{}/ under Desktop authorization before using the agent.",
        system.bind.port()
    );
    Ok(())
}

pub fn uninstall(system_only: bool) -> Result<()> {
    ensure!(
        super::is_elevated()?,
        "combined/system uninstallation requires Administrator/root; use --user to remove only the current user's autostart"
    );
    if system_only {
        return super::uninstall_system();
    }
    // Resolve the same login identity as install before changing either side.
    let owner = LoginOwner::detect()?;
    uninstall_both(|| owner.uninstall(), super::uninstall_system)?;
    println!(
        "Both daemon registrations were removed (or already absent). Configuration, seal.key, pairing and passkeys were preserved."
    );
    Ok(())
}

fn uninstall_both(
    user: impl FnOnce() -> Result<()>,
    system: impl FnOnce() -> Result<()>,
) -> Result<()> {
    user().context("user autostart removal failed; system service was left unchanged")?;
    system().context("user autostart was removed, but system service removal failed; fix the error and rerun uninstall")
}

pub fn setup_user(path: Option<PathBuf>, system: Option<SocketAddr>, start: bool) -> Result<()> {
    let path = path.map_or_else(user_config_path, Ok)?;
    let config = prepare_user_config(&path, system, |path| {
        println!(
            "User daemon configuration is missing. Create its separate Master Password; the existing system daemon password is NOT being reset.\nUser configuration: {}",
            path.display()
        );
        super::initialize_config(path.into(), false, false, false, true, system)
    })?;
    if start {
        super::user_service::install(&path)?;
        wait_healthy(config.bind, "user", Duration::from_secs(30))?;
        super::user_service::verify_running(&path)?;
        println!(
            "User management is ready at http://localhost:{}/. If not already paired, open Desktop authorization to create a pairing code.",
            config.bind.port()
        );
    }
    Ok(())
}

fn prepare_user_config(
    path: &Path,
    system: Option<SocketAddr>,
    initialize: impl FnOnce(&Path) -> Result<()>,
) -> Result<Config> {
    if !path.try_exists()? {
        initialize(path)?;
    }
    let config = Config::load(path)?;
    ensure!(
        config.bind != config.privileged_daemon,
        "user and system endpoints must differ"
    );
    if let Some(system) = system {
        ensure!(
            config.privileged_daemon == system,
            "existing user configuration points to {}, but this system service listens at {}; configuration was not overwritten",
            config.privileged_daemon,
            system
        );
        ensure!(
            config.bind != system,
            "user and system endpoints must differ"
        );
    }
    Ok(config)
}

fn health_matches(bytes: &[u8], daemon: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes).is_ok_and(|value| {
        value["service"] == "localshelld" && value["status"] == "ok" && value["daemon"] == daemon
    })
}

pub(crate) fn wait_healthy(address: SocketAddr, daemon: &str, timeout: Duration) -> Result<()> {
    wait_healthy_version(address, daemon, None, timeout)
}

pub(crate) fn wait_healthy_version(
    address: SocketAddr,
    daemon: &str,
    version: Option<&str>,
    timeout: Duration,
) -> Result<()> {
    ensure!(
        address.ip().is_loopback(),
        "health checks require a loopback address"
    );
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()?;
    let url = format!("http://{address}/health");
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(response) = client.get(&url).send().and_then(|r| r.error_for_status()) {
            let mut bytes = Vec::new();
            if response.take(4097).read_to_end(&mut bytes).is_ok()
                && bytes.len() <= 4096
                && health_matches(&bytes, daemon)
                && version.is_none_or(|version| {
                    serde_json::from_slice::<serde_json::Value>(&bytes)
                        .is_ok_and(|health| health["version"] == version)
                })
            {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "{daemon} daemon did not become healthy at {url}; check its startup task/service and configuration; installation is incomplete"
            );
        }
        thread::sleep(Duration::from_millis(200));
    }
}

struct LoginOwner {
    #[cfg(unix)]
    uid: u32,
}

impl LoginOwner {
    fn detect() -> Result<Self> {
        #[cfg(windows)]
        {
            super::user_service::require_interactive_owner()?;
            Ok(Self {})
        }
        #[cfg(unix)]
        {
            let uid = parse_owner_uid(std::env::var("SUDO_UID").ok().as_deref())?;
            #[cfg(target_os = "linux")]
            ensure!(
                Path::new(&format!("/run/user/{uid}/bus")).exists(),
                "the invoking user's login session/bus is unavailable; install from that user's logged-in session with sudo, or explicitly use --system-only"
            );
            #[cfg(target_os = "macos")]
            ensure!(
                std::process::Command::new("/bin/launchctl")
                    .args(["print", &format!("gui/{uid}")])
                    .output()?
                    .status
                    .success(),
                "the invoking user's GUI login session is unavailable; install from that user's desktop with sudo, or explicitly use --system-only"
            );
            Ok(Self { uid })
        }
    }

    fn setup(&self, path: Option<&Path>, system: SocketAddr, start: bool) -> Result<()> {
        #[cfg(windows)]
        {
            // Elevated Windows terminals retain the same account/SID. The task
            // uses InteractiveToken + LeastPrivilege, never the installer's token.
            setup_user(path.map(Path::to_owned), Some(system), start)
        }
        #[cfg(unix)]
        {
            let mut command = owner_command(self.uid, &std::env::current_exe()?);
            command
                .arg("setup-user")
                .arg("--privileged-daemon")
                .arg(system.to_string());
            if let Some(path) = path {
                command.arg("--config").arg(path);
            }
            if start {
                command.arg("--start");
            }
            // Inherit the terminal: the preparation step may ask for a new
            // user password, and the start step prints the pairing page URL.
            let status = command
                .status()
                .context("failed to run user installation step")?;
            ensure!(status.success(), "user installation step failed ({status})");
            Ok(())
        }
    }

    fn uninstall(&self) -> Result<()> {
        #[cfg(windows)]
        {
            // Administrative task ownership does not change the account/SID
            // whose limited user task is being removed.
            super::user_service::uninstall()
        }
        #[cfg(unix)]
        {
            super::checked(
                owner_command(self.uid, &std::env::current_exe()?).args(["uninstall", "--user"]),
            )
        }
    }
}

#[cfg(any(unix, test))]
fn parse_owner_uid(value: Option<&str>) -> Result<u32> {
    let uid: u32 = value.context("cannot identify the login user; invoke install/uninstall through sudo from that user, or explicitly use --system-only")?
        .parse().context("invalid SUDO_UID")?;
    ensure!(
        uid != 0,
        "the user daemon must not belong to root; use sudo from the intended user or --system-only"
    );
    Ok(uid)
}

#[cfg(unix)]
fn owner_command(uid: u32, executable: &Path) -> std::process::Command {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("/bin/launchctl");
        command.args(["asuser", &uid.to_string(), "/usr/bin/sudo"]);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("/usr/bin/sudo");
    command.args(["-n", "-H", "-u", &format!("#{uid}"), "--", "/usr/bin/env"]);
    // Never reuse root's XDG directories or session bus for the user daemon.
    command.args([
        "-u",
        "XDG_CONFIG_HOME",
        "-u",
        "XDG_DATA_HOME",
        "-u",
        "DBUS_SESSION_BUS_ADDRESS",
        "-u",
        "XDG_RUNTIME_DIR",
    ]);
    #[cfg(target_os = "linux")]
    command.args([
        format!("XDG_RUNTIME_DIR=/run/user/{uid}"),
        format!("DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus"),
    ]);
    command.arg(executable);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn combined_uninstall_removes_user_first_and_reports_partial_failure() {
        use std::cell::RefCell;
        let calls = RefCell::new(Vec::new());
        uninstall_both(
            || {
                calls.borrow_mut().push("user");
                Ok(())
            },
            || {
                calls.borrow_mut().push("system");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["user", "system"]);

        let error = uninstall_both(
            || bail!("access denied"),
            || panic!("must not uninstall the system service after user removal failed"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("system service was left unchanged"));
        assert!(format!("{error:#}").contains("access denied"));

        let error = uninstall_both(|| Ok(()), || bail!("service busy")).unwrap_err();
        assert!(format!("{error:#}").contains("user autostart was removed"));
        assert!(format!("{error:#}").contains("service busy"));
    }

    #[test]
    fn login_owner_never_silently_falls_back_to_root() {
        for value in [None, Some("0"), Some("root"), Some("-1"), Some("1000;id")] {
            assert!(parse_owner_uid(value).is_err());
        }
        assert_eq!(parse_owner_uid(Some("1000")).unwrap(), 1000);
    }

    #[test]
    fn health_requires_the_expected_identity_not_just_an_open_port() {
        assert!(health_matches(
            br#"{"service":"localshelld","status":"ok","daemon":"user"}"#,
            "user"
        ));
        assert!(!health_matches(
            br#"{"service":"localshelld","status":"ok","daemon":"privileged"}"#,
            "user"
        ));
        assert!(!health_matches(
            br#"{"service":"another","status":"ok","daemon":"user"}"#,
            "user"
        ));
        assert!(!health_matches(b"not json", "user"));
    }

    #[test]
    fn startup_probe_checks_a_real_loopback_response() {
        use std::{io::Write, net::TcpListener};
        for (identity, expected) in [("user", true), ("privileged", false)] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            listener.set_nonblocking(true).unwrap();
            let server = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    if let Ok((stream, _)) = listener.accept() {
                        break stream;
                    }
                    assert!(Instant::now() < deadline, "loopback probe never connected");
                    thread::sleep(Duration::from_millis(10));
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = [0; 2048];
                assert!(stream.read(&mut request).unwrap() > 0);
                let body =
                    format!(r#"{{"service":"localshelld","status":"ok","daemon":"{identity}"}}"#);
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            });
            assert_eq!(
                wait_healthy(address, "user", Duration::ZERO).is_ok(),
                expected
            );
            server.join().unwrap();
        }
    }

    #[test]
    fn existing_user_configuration_and_trust_are_never_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let config = Config {
            bind: "127.0.0.1:32120".parse().unwrap(),
            password_hash: "existing-verifier".into(),
            ..Config::default()
        };
        config.save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let trust = path.with_extension("authorization.json");
        std::fs::write(&trust, b"existing pairing and credentials").unwrap();
        prepare_user_config(&path, Some(config.privileged_daemon), |_| {
            panic!("must not initialize existing configuration")
        })
        .unwrap();
        assert!(
            prepare_user_config(&path, Some("127.0.0.1:12345".parse().unwrap()), |_| panic!(
                "must not initialize existing configuration"
            ))
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(
            std::fs::read(&trust).unwrap(),
            b"existing pairing and credentials"
        );
    }

    #[test]
    fn missing_user_configuration_is_initialized_before_it_can_be_started() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let system = "127.0.0.1:33319".parse().unwrap();
        let config = prepare_user_config(&path, Some(system), |path| {
            Config {
                bind: "127.0.0.1:32120".parse().unwrap(),
                privileged_daemon: system,
                password_hash: "new-verifier".into(),
                ..Config::default()
            }
            .save(path)
        })
        .unwrap();
        assert_eq!(config.privileged_daemon, system);
        assert_ne!(config.bind, system);
        assert!(!path.with_extension("authorization.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn user_setup_command_drops_identity_without_shell_interpolation() {
        let command = owner_command(1234, Path::new("/opt/A & B/localshelld"));
        let arguments: Vec<_> = command.get_args().map(|s| s.to_string_lossy()).collect();
        assert!(arguments.iter().any(|value| value == "#1234"));
        assert_eq!(arguments.last().unwrap(), "/opt/A & B/localshelld");
        assert!(!arguments.iter().any(|value| value == "-c"));
        #[cfg(target_os = "linux")]
        assert!(
            arguments
                .iter()
                .any(|value| value == "XDG_RUNTIME_DIR=/run/user/1234")
        );
    }
}
