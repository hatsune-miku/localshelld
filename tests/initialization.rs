use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, Command, Output, Stdio},
};

use localshelld::{
    auth::{AuthManager, Credential, CredentialKind, create_totp},
    config::{Config, load_or_create_seal_key, seal_key_path, unseal},
};

struct Process(Child);

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cli(config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trustd"));
    command.args(["init", "--config"]).arg(config);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    command
}

fn initialize(config: &Path, input: &str, stdin_password: bool) -> Output {
    let mut command = cli(config);
    if stdin_password {
        command.arg("--password-stdin");
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
fn initialization_accepts_empty_short_and_unrestricted_passwords() {
    for stdin_password in [false, true] {
        for password in ["", "x", "   ", "  密码\t "] {
            let temp = tempfile::tempdir().unwrap();
            let config = temp.path().join("config.toml");
            let input = if stdin_password {
                format!("{password}\n")
            } else {
                format!("{password}\n{password}\n")
            };
            let result = initialize(&config, &input, stdin_password);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            if !stdin_password {
                let output = String::from_utf8_lossy(&result.stdout);
                assert!(output.contains("New Master Password: "));
                assert!(output.contains("Confirm Master Password: "));
            }
            let loaded = Config::load(&config).unwrap();
            assert!(loaded.password_hash.starts_with("$argon2id$"));
            let mut auth = AuthManager::new(loaded.password_hash, None);
            auth.verify_credential(&Credential {
                kind: CredentialKind::Password,
                value: password.into(),
            })
            .unwrap();
            assert!(
                auth.verify_credential(&Credential {
                    kind: CredentialKind::Password,
                    value: format!("{password}wrong"),
                })
                .is_err()
            );
        }
    }
}

#[test]
fn initialization_still_requires_input_and_matching_confirmation() {
    for (input, stdin_password, error) in [
        ("", true, "input ended"),
        ("", false, "input ended"),
        ("x\n", false, "input ended"),
        ("x\ny\n", false, "passwords do not match"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config.toml");
        let result = initialize(&config, input, stdin_password);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(error));
        assert!(!config.exists());
        assert!(!seal_key_path(&config).exists());
    }
}

#[test]
fn totp_confirmation_uses_ordinary_stdin() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config.toml");
    let mut process = Process(cli(&config).arg("--totp").spawn().unwrap());
    let mut input = process.0.stdin.take().unwrap();
    input.write_all(b"x\nx\n").unwrap();
    input.flush().unwrap();
    let mut output = BufReader::new(process.0.stdout.take().unwrap());
    let secret = loop {
        // Windows ACL setup may also print localized, non-UTF-8 tool output.
        let mut line = Vec::new();
        assert_ne!(
            output.read_until(b'\n', &mut line).unwrap(),
            0,
            "initialization exited before displaying the TOTP secret"
        );
        let line = String::from_utf8_lossy(&line);
        if let Some(secret) = line.trim().strip_prefix("Manual secret: ") {
            break totp_rs::Secret::Encoded(secret.into()).to_bytes().unwrap();
        }
    };
    let code = create_totp(&secret).unwrap().generate_current().unwrap();
    writeln!(input, "{code}").unwrap();
    drop(input);
    assert!(process.0.wait().unwrap().success());
    let loaded = Config::load(&config).unwrap();
    let key = load_or_create_seal_key(&seal_key_path(&config)).unwrap();
    let stored = unseal(loaded.totp_secret.as_ref().unwrap(), &key).unwrap();
    assert_eq!(&*stored, &secret);
}
