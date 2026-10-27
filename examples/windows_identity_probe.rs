//! Manual two-account smoke test. `hold <directory>` runs as the ordinary user;
//! `verify <directory>` runs as SYSTEM. No services, accounts or registry values
//! are created/modified. The JSON environment stays in the temporary directory.
#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use localshelld_native_process::{UserContext, current_user_context};
    use std::{
        path::PathBuf,
        time::{Duration, Instant},
    };
    let mut args = std::env::args().skip(1);
    let mode = args.next().expect("hold, verify or stop");
    let directory = PathBuf::from(args.next().expect("temporary directory"));
    let context_file = directory.join("user-context.json");
    match mode.as_str() {
        "hold" => {
            let context = current_user_context()?;
            std::fs::write(&context_file, serde_json::to_vec(&context)?)?;
            println!("ready");
            let started = Instant::now();
            while !directory.join("stop").exists() && started.elapsed() < Duration::from_secs(300) {
                std::thread::sleep(Duration::from_millis(200));
            }
            std::fs::remove_file(context_file)?;
        }
        "verify" => {
            let context: UserContext = serde_json::from_slice(&std::fs::read(context_file)?)?;
            let script = r#"
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
@{ sid=$identity.User.Value; elevated=$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator); profile=$env:USERPROFILE; path=$env:PATH; hkcu=(Get-ItemProperty -LiteralPath 'Registry::HKEY_CURRENT_USER\Volatile Environment' -ErrorAction Stop).USERPROFILE } | ConvertTo-Json -Compress
"#;
            let result = tokio::runtime::Runtime::new()?.block_on(async {
                let shell =
                    localshelld::shell::Shell::spawn_administrator("pwsh", 1024 * 1024, &context)
                        .await?;
                let result = shell.execute(script, Some(20)).await;
                shell.terminate().await;
                result
            })?;
            anyhow::ensure!(
                result.stderr.is_empty(),
                "administrator probe failed: {}",
                result.stderr
            );
            anyhow::ensure!(
                result.success,
                "administrator probe command did not succeed"
            );
            let actual: serde_json::Value = serde_json::from_str(&result.stdout)?;
            let env_value = |name: &str| {
                context
                    .environment
                    .iter()
                    .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    .map(|(_, value)| value.as_str())
                    .unwrap_or("")
            };
            anyhow::ensure!(
                actual["sid"] == context.identity.sid,
                "administrator SID changed"
            );
            anyhow::ensure!(actual["elevated"] == true, "token is not elevated");
            anyhow::ensure!(
                actual["profile"] == env_value("USERPROFILE"),
                "USERPROFILE was not preserved"
            );
            anyhow::ensure!(
                actual["path"] == env_value("PATH"),
                "user PATH was not preserved"
            );
            anyhow::ensure!(
                actual["hkcu"] == env_value("USERPROFILE"),
                "HKCU does not belong to the paired user"
            );
            println!("PASS: same user SID, elevated token, USERPROFILE, HKCU and user PATH");
        }
        "stop" => std::fs::write(directory.join("stop"), [])?,
        _ => anyhow::bail!("unknown probe mode"),
    }
    Ok(())
}
