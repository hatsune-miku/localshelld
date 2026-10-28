use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
    process::Command,
};

#[cfg(target_os = "linux")]
use std::fs;
#[cfg(any(target_os = "linux", windows))]
use std::path::Path;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use localshelld::{
    auth::{AuthManager, create_totp, generate_totp_secret, hash_password},
    config::{
        Config, USER_PORT, default_config_path, load_or_create_seal_key, seal, seal_key_path,
        user_config_path,
    },
    server::AppState,
};
use zeroize::{Zeroize, Zeroizing};

mod bundle;
mod installation;
#[cfg(any(target_os = "macos", test))]
mod launchd;
mod maintenance;
mod update;
mod user_service;

#[derive(Parser)]
#[command(name = "trustd", version = localshelld::VERSION, about = "Install, configure and maintain trustd")]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Subcommand)]
enum CommandKind {
    /// Open the graphical installation and maintenance wizard in a local browser.
    #[command(alias = "setup")]
    Maintenance {
        /// Print the local wizard URL without opening the browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Internal, one-shot installer. Never exposed by either daemon.
    #[command(hide = true)]
    MaintenanceApply { plan: PathBuf, digest: String },
    /// Configure cloud-hosted Telegram from stdin; connect cloud and stop this daemon first.
    ConfigureTelegram {
        #[arg(long)]
        user: bool,
        #[arg(long)]
        config: Option<PathBuf>,
        /// Reuse a previously generated binding link (32 hexadecimal characters).
        #[arg(long)]
        binding_code: Option<String>,
    },
    /// Initialize the Master Password and optional Authenticator support.
    Init {
        /// Initialize an independent current-user daemon configuration.
        #[arg(long)]
        user: bool,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        totp: bool,
        #[arg(long)]
        force: bool,
        /// Read one password line from stdin (intended for automated installation).
        #[arg(long)]
        password_stdin: bool,
    },
    /// Register and start both daemons, then complete trust pairing in the management UI.
    Install {
        /// Install only the current non-elevated user's autostart daemon.
        #[arg(long)]
        user: bool,
        #[arg(long)]
        config: Option<PathBuf>,
        /// User configuration for a combined installation (defaults to the login user's directory).
        #[arg(long, conflicts_with_all = ["user", "system_only"])]
        user_config: Option<PathBuf>,
        /// Explicitly install only the system service, for headless hosts.
        #[arg(long, conflicts_with = "user")]
        system_only: bool,
    },
    /// Internal non-root installation step invoked by the privileged installer.
    #[command(hide = true)]
    SetupUser {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        privileged_daemon: std::net::SocketAddr,
        #[arg(long)]
        start: bool,
    },
    /// Stop and unregister both daemons without deleting configuration or authorization data.
    Uninstall {
        /// Remove only this account's user autostart (Windows also permits its elevated token).
        #[arg(long)]
        user: bool,
        /// Remove only the system service, leaving user autostart unchanged.
        #[arg(long, conflicts_with = "user")]
        system_only: bool,
    },
    /// Check for or install an update from the official GitHub Releases.
    Update(update::Options),
}

fn main() -> Result<()> {
    let command = if std::env::args_os().len() == 1 {
        CommandKind::Maintenance { no_open: false }
    } else {
        Cli::parse().command
    };
    #[cfg(windows)]
    if matches!(&command, CommandKind::Maintenance { no_open: false }) {
        localshelld_native_dialog::detach_owned_console()
            .context("failed to detach the user daemon's standalone console")?;
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "localshelld=info".into()),
        )
        .init();
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run_command(command))
}

async fn run_command(command: CommandKind) -> Result<()> {
    match command {
        CommandKind::ConfigureTelegram {
            user,
            config,
            binding_code,
        } => {
            if user {
                require_user()?;
            }
            let path = mode_config_path(config, user)?;
            let config = Config::load(&path)?;
            // Reserve the same listening socket throughout provisioning. A live
            // daemon or a second setup process must not rotate the same tokens.
            let _exclusive = tokio::net::TcpListener::bind(config.bind)
                .await
                .context("stop the daemon before configuring Telegram from the CLI")?;
            let auth = AuthManager::new(config.password_hash.clone(), None);
            let state = if user {
                AppState::user(config, auth)?
            } else {
                AppState::new(config, auth)
            }
            .with_storage(path)?;
            use localshelld::telegram::Action;
            if state.telegram_action("local-cli", Action::Status).await?["mode"] != "cloud" {
                bail!("Telegram authorization requires a cloud connection");
            }
            let token = Zeroizing::new(read_input_line(&mut io::stdin().lock())?);
            state
                .telegram_action(
                    "local-cli",
                    Action::Configure {
                        token: Some(token.to_string()),
                        enabled: true,
                    },
                )
                .await?;
            let binding = state
                .telegram_action("local-cli", Action::BindStart { code: binding_code })
                .await?;
            println!(
                "Open {} and select Start.",
                binding["url"].as_str().unwrap_or_default()
            );
            let code = binding["code"]
                .as_str()
                .context("missing binding code")?
                .to_owned();
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(900);
            loop {
                if tokio::time::Instant::now() >= deadline {
                    bail!("Telegram binding expired; token remains saved");
                }
                let result = state
                    .telegram_action("local-cli", Action::BindPoll { code: code.clone() })
                    .await?;
                if result["bound"] == true {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
            state.telegram_action("local-cli", Action::Test).await?;
            println!(
                "{}",
                state.telegram_action("local-cli", Action::Status).await?
            );
            state.shutdown().await;
            Ok(())
        }
        CommandKind::Maintenance { no_open } => maintenance::run(no_open).await,
        CommandKind::MaintenanceApply { plan, digest } => {
            tokio::task::spawn_blocking(move || maintenance::apply(&plan, &digest)).await?
        }
        CommandKind::Init {
            user,
            config,
            totp,
            force,
            password_stdin,
        } => initialize(
            mode_config_path(config, user)?,
            totp,
            force,
            password_stdin,
            user,
        ),
        CommandKind::Install {
            config,
            user,
            user_config,
            system_only,
        } => {
            tokio::task::spawn_blocking(move || {
                if user {
                    require_user()?;
                    installation::setup_user(config, None, true)
                } else {
                    installation::install(config_path(config)?, user_config, system_only)
                }
            })
            .await?
        }
        CommandKind::SetupUser {
            config,
            privileged_daemon,
            start,
        } => {
            tokio::task::spawn_blocking(move || {
                require_user()?;
                installation::setup_user(config, Some(privileged_daemon), start)
            })
            .await?
        }
        CommandKind::Uninstall { user, system_only } => {
            tokio::task::spawn_blocking(move || {
                if user {
                    user_service::uninstall()
                } else {
                    installation::uninstall(system_only)
                }
            })
            .await?
        }
        CommandKind::Update(options) => {
            tokio::task::spawn_blocking(move || update::run(options)).await?
        }
    }
}

fn config_path(path: Option<PathBuf>) -> Result<PathBuf> {
    path.map_or_else(default_config_path, Ok)
}

fn mode_config_path(path: Option<PathBuf>, user: bool) -> Result<PathBuf> {
    if user {
        path.map_or_else(user_config_path, Ok)
    } else {
        config_path(path)
    }
}

fn read_input_line(reader: &mut impl BufRead) -> io::Result<String> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "input ended before a line was provided",
        ));
    }
    // Remove only the line ending; spaces and other characters are input data.
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(line)
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    Ok(read_input_line(&mut io::stdin().lock())?)
}

fn initialize(
    path: PathBuf,
    enable_totp: bool,
    force: bool,
    password_stdin: bool,
    user: bool,
) -> Result<()> {
    if user {
        require_user()?;
    }
    initialize_config(path, enable_totp, force, password_stdin, user, None)
}

fn initialize_config(
    path: PathBuf,
    enable_totp: bool,
    force: bool,
    password_stdin: bool,
    user: bool,
    privileged_daemon: Option<std::net::SocketAddr>,
) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists; use --force to replace it",
            path.display()
        );
    }
    let mut password = if password_stdin {
        Zeroizing::new(read_input_line(&mut io::stdin().lock())?)
    } else {
        let first = Zeroizing::new(prompt_line("New Master Password: ")?);
        let second = Zeroizing::new(prompt_line("Confirm Master Password: ")?);
        if *first != *second {
            bail!("passwords do not match");
        }
        first
    };
    let password_hash = hash_password(password.as_bytes())?;
    password.zeroize();

    let key_path = seal_key_path(&path);
    let key = load_or_create_seal_key(&key_path)?;
    let totp_secret = if enable_totp {
        let secret = generate_totp_secret();
        let mut totp = create_totp(&secret)?;
        if user {
            totp.account_name = "local-user".into();
        }
        println!("\nAdd this account to Proton Authenticator (or another RFC 6238 app):");
        println!("URI: {}", totp.get_url());
        println!("Manual secret: {}", totp.get_secret_base32());
        let mut code = Zeroizing::new(prompt_line("Enter the current 6-digit code to confirm: ")?);
        if !totp.check_current(code.trim()).unwrap_or(false) {
            code.zeroize();
            bail!("incorrect TOTP code; configuration was not written");
        }
        code.zeroize();
        Some(seal(&secret, &key)?)
    } else {
        None
    };
    let mut config = Config {
        password_hash,
        totp_secret,
        ..Config::default()
    };
    if user {
        config.bind.set_port(USER_PORT);
    }
    if let Some(address) = privileged_daemon {
        config.privileged_daemon = address;
    }
    config.save(&path)?;
    println!("Initialized {}", path.display());
    println!("No Master Password was stored; only its Argon2id verifier was written.");
    Ok(())
}

use localshelld::daemon::{is_elevated, require_user, shutdown_signal};

#[cfg(windows)]
fn windows_command(relative: &str) -> Result<Command> {
    Ok(localshelld::process::background_command(
        localshelld_native_process::system_directory()?.join(relative),
    ))
}

fn install_system(config_path: PathBuf) -> Result<()> {
    if !is_elevated()? {
        bail!("service installation requires Administrator/root");
    }
    let config_path = config_path
        .canonicalize()
        .context("failed to resolve configuration path")?;
    Config::load(&config_path)?;
    let executable = bundle::sibling(bundle::PRIVILEGED)?;
    install_system_at(&executable, &config_path)
}

fn install_system_at(executable: &std::path::Path, config_path: &std::path::Path) -> Result<()> {
    if !is_elevated()? {
        bail!("service installation requires Administrator/root");
    }
    let config_path = config_path.canonicalize()?;
    Config::load(&config_path)?;
    if update::reuse_installed_service(&executable.canonicalize()?, &config_path)? {
        println!("Existing localshelld system service is running; registration was preserved.");
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    install_systemd(executable, &config_path)?;
    #[cfg(windows)]
    install_windows_service(executable, &config_path)?;
    #[cfg(target_os = "macos")]
    launchd::install(executable, &config_path)?;
    println!("trustd service installed and started.");
    Ok(())
}

fn uninstall_system() -> Result<()> {
    if !is_elevated()? {
        bail!("service uninstallation requires Administrator/root");
    }
    #[cfg(target_os = "linux")]
    let removed = uninstall_systemd()?;
    #[cfg(windows)]
    let removed = uninstall_windows_service()?;
    #[cfg(target_os = "macos")]
    let removed = launchd::uninstall()?;

    if removed {
        println!("localshelld service stopped and uninstalled.");
    } else {
        println!("localshelld service is not installed.");
    }
    println!("Configuration and seal.key were preserved.");
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_systemd(executable: &Path, config: &Path) -> Result<()> {
    let command = if executable.file_stem().and_then(|s| s.to_str()) == Some("trustd-root") {
        ""
    } else {
        "serve "
    };
    let unit = format!(
        "[Unit]\nDescription=trustd root daemon\nAfter=network.target\n\n[Service]\nType=simple\nExecStart={} {command}--config {}\nRestart=on-failure\nRestartSec=3\nNoNewPrivileges=false\n\n[Install]\nWantedBy=multi-user.target\n",
        systemd_escape(executable),
        systemd_escape(config)
    );
    fs::write(SYSTEMD_UNIT_PATH, unit)?;
    checked(Command::new("systemctl").arg("daemon-reload"))?;
    checked(Command::new("systemctl").args(["enable", "--now", "localshelld.service"]))
}

#[cfg(target_os = "linux")]
const SYSTEMD_UNIT_PATH: &str = "/etc/systemd/system/localshelld.service";

#[cfg(target_os = "linux")]
fn uninstall_systemd() -> Result<bool> {
    let unit_path = Path::new(SYSTEMD_UNIT_PATH);
    if !unit_path.exists() {
        return Ok(false);
    }
    checked(Command::new("systemctl").args(["disable", "--now", "localshelld.service"]))?;
    fs::remove_file(unit_path)?;
    checked(Command::new("systemctl").arg("daemon-reload"))?;
    Ok(true)
}

#[cfg(target_os = "linux")]
fn systemd_escape(path: &Path) -> String {
    path.to_string_lossy().replace(' ', "\\x20")
}

#[cfg(windows)]
fn install_windows_service(executable: &Path, config: &Path) -> Result<()> {
    let command = if executable.file_stem().and_then(|s| s.to_str()) == Some("trustd-system") {
        ""
    } else {
        "service "
    };
    let bin_path = format!(
        "\"{}\" {command}--config \"{}\"",
        executable.display(),
        config.display()
    );
    checked(windows_command("sc.exe")?.args([
        "create",
        "localshelld",
        "binPath=",
        &bin_path,
        "start=",
        "auto",
        "DisplayName=",
        "trustd SYSTEM daemon",
    ]))?;
    checked(windows_command("sc.exe")?.args([
        "description",
        "localshelld",
        "User-controlled privileged PowerShell broker",
    ]))?;
    checked(windows_command("sc.exe")?.args([
        "failure",
        "localshelld",
        "reset=",
        "86400",
        "actions=",
        "restart/5000/restart/15000/\"\"/0",
    ]))?;
    checked(windows_command("sc.exe")?.args(["start", "localshelld"]))
}

#[cfg(windows)]
fn uninstall_windows_service() -> Result<bool> {
    use std::{thread::sleep, time::Duration};

    use windows_service::{
        service::{ServiceAccess, ServiceState},
        service_manager::{ServiceManager, ServiceManagerAccess},
    };

    const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;
    const ERROR_SERVICE_NOT_ACTIVE: i32 = 1062;
    const ERROR_SERVICE_MARKED_FOR_DELETE: i32 = 1072;

    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("failed to connect to the Windows Service Control Manager")?;
    let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
    let service = match manager.open_service("localshelld", access) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(error))
            if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error).context("failed to open the localshelld service"),
    };

    match service.delete() {
        Ok(()) => {}
        Err(windows_service::Error::Winapi(error))
            if error.raw_os_error() == Some(ERROR_SERVICE_MARKED_FOR_DELETE) => {}
        Err(error) => {
            return Err(error).context("failed to mark the localshelld service for deletion");
        }
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut stop_requested = false;
    loop {
        let state = service
            .query_status()
            .context("failed to query the localshelld service status")?
            .current_state;
        match state {
            ServiceState::Stopped => break,
            ServiceState::Running | ServiceState::Paused if !stop_requested => {
                match service.stop() {
                    Ok(_) => stop_requested = true,
                    Err(windows_service::Error::Winapi(error))
                        if error.raw_os_error() == Some(ERROR_SERVICE_NOT_ACTIVE) =>
                    {
                        stop_requested = true;
                    }
                    Err(error) => {
                        return Err(error).context("failed to stop the localshelld service");
                    }
                }
            }
            ServiceState::StopPending => stop_requested = true,
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            bail!("localshelld was marked for deletion but did not stop within 15 seconds");
        }
        sleep(Duration::from_millis(200));
    }
    drop(service);
    Ok(true)
}

fn checked(command: &mut Command) -> Result<()> {
    let description = format!("{command:?}");
    let output = command
        .output()
        .with_context(|| format!("failed to run {description}"))?;
    if !output.status.success() {
        bail!(
            "{description} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_input_accepts_empty_and_preserves_whitespace() {
        for (input, expected) in [
            ("\n", ""),
            ("\r\n", ""),
            ("x\n", "x"),
            ("  密码\t \r\n", "  密码\t "),
            ("no final newline", "no final newline"),
            ("embedded\rcharacter\n", "embedded\rcharacter"),
        ] {
            assert_eq!(read_input_line(&mut input.as_bytes()).unwrap(), expected);
        }
        assert_eq!(
            read_input_line(&mut "".as_bytes()).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn user_mode_has_separate_configuration_and_no_elevation_bypass() {
        for command in ["init", "install", "uninstall"] {
            assert!(Cli::try_parse_from(["trustd", command, "--user"]).is_ok());
        }
        assert!(Cli::try_parse_from(["trustd", "serve"]).is_err());
        assert!(Cli::try_parse_from(["trustd", "service"]).is_err());
        assert_ne!(
            mode_config_path(None, true).unwrap(),
            mode_config_path(None, false).unwrap()
        );
    }

    #[test]
    fn uninstall_defaults_to_both_daemons_and_rejects_conflicting_scopes() {
        let cli = Cli::try_parse_from(["trustd", "uninstall"]).unwrap();
        assert!(matches!(
            cli.command,
            CommandKind::Uninstall {
                user: false,
                system_only: false
            }
        ));
        let cli = Cli::try_parse_from(["trustd", "uninstall", "--user"]).unwrap();
        assert!(matches!(
            cli.command,
            CommandKind::Uninstall {
                user: true,
                system_only: false
            }
        ));
        let cli = Cli::try_parse_from(["trustd", "uninstall", "--system-only"]).unwrap();
        assert!(matches!(
            cli.command,
            CommandKind::Uninstall {
                user: false,
                system_only: true
            }
        ));
        assert!(Cli::try_parse_from(["trustd", "uninstall", "--user", "--system-only"]).is_err());
    }

    #[test]
    fn install_defaults_to_both_daemons_with_explicit_single_identity_options() {
        assert!(matches!(
            Cli::try_parse_from(["trustd", "install"]).unwrap().command,
            CommandKind::Install {
                user: false,
                system_only: false,
                ..
            }
        ));
        assert!(Cli::try_parse_from(["trustd", "install", "--system-only"]).is_ok());
        assert!(Cli::try_parse_from(["trustd", "install", "--user-config", "user.toml"]).is_ok());
        for arguments in [
            vec!["localshelld", "install", "--user", "--system-only"],
            vec!["trustd", "install", "--user", "--user-config", "user.toml"],
            vec![
                "localshelld",
                "install",
                "--system-only",
                "--user-config",
                "user.toml",
            ],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
    }

    #[test]
    fn parses_update_options_and_rejects_ambiguous_downgrades() {
        assert!(Cli::try_parse_from(["trustd", "update", "--check", "--prerelease"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "localshelld",
                "update",
                "--tag",
                "v0.1.0",
                "--allow-downgrade"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["trustd", "update", "--allow-downgrade"]).is_err());
        assert!(
            Cli::try_parse_from(["trustd", "update", "--tag", "v0.1.0", "--prerelease"]).is_err()
        );
    }
}
