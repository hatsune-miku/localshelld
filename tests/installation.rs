//! Test the real preparation helper without registering services or autostart tasks.
use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

use localshelld::{
    auth::{AuthManager, Credential, CredentialKind},
    config::{Config, seal_key_path},
};

fn prepare(path: &Path, endpoint: &str, input: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_localshelld"));
    command
        .args(["setup-user", "--privileged-daemon", endpoint, "--config"])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn installation_prepares_credentials_now_and_preserves_existing_manual_trust() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("user/config.toml");
    let result = prepare(&path, "127.0.0.1:33319", "setup password\nsetup password\n");
    if !result.status.success() {
        // Elevated runners must reject this non-root helper; they may not silently
        // initialize a supposed user daemon with Administrator/root identity.
        assert!(String::from_utf8_lossy(&result.stderr).contains("non-elevated"));
        assert!(!path.exists());
        return;
    }
    let prompt = String::from_utf8_lossy(&result.stdout);
    assert!(prompt.contains("User daemon configuration is missing"));
    assert!(prompt.contains("system daemon password is NOT being reset"));
    let config = Config::load(&path).unwrap();
    assert_eq!(config.bind, "127.0.0.1:32120".parse().unwrap());
    assert_eq!(config.privileged_daemon, "127.0.0.1:33319".parse().unwrap());
    AuthManager::new(config.password_hash, None)
        .verify_credential(&Credential {
            kind: CredentialKind::Password,
            value: "setup password".into(),
        })
        .unwrap();
    let authorization = path.with_extension("authorization.json");
    assert!(
        !authorization.exists(),
        "preparation must not create trust automatically"
    );
    let saved_trust = br#"{"pairing":{"key":"existing key","peer":"127.0.0.1:33319"},"remembered":{"Test":true}}"#;
    std::fs::write(&authorization, saved_trust).unwrap();
    let saved_config = std::fs::read(&path).unwrap();
    let saved_seal_key = std::fs::read(seal_key_path(&path)).unwrap();
    let repeated = prepare(&path, "127.0.0.1:33319", "");
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert!(!String::from_utf8_lossy(&repeated.stdout).contains("New Master Password"));
    let mismatch = prepare(&path, "127.0.0.1:33321", "");
    assert!(!mismatch.status.success());
    assert!(String::from_utf8_lossy(&mismatch.stderr).contains("not overwritten"));
    assert_eq!(std::fs::read(&path).unwrap(), saved_config);
    assert_eq!(std::fs::read(seal_key_path(&path)).unwrap(), saved_seal_key);
    assert_eq!(std::fs::read(&authorization).unwrap(), saved_trust);
}

#[test]
fn installation_does_not_invent_a_password_when_input_is_missing_or_mismatched() {
    for input in ["", "one\ntwo\n"] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let result = prepare(&path, "127.0.0.1:32119", input);
        assert!(!result.status.success());
        assert!(!path.exists());
        assert!(!seal_key_path(&path).exists());
    }
}
