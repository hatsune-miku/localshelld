//! Exercise the actual CLI process and its inherited OS identity, not just AppState.
use std::{
    io::Write,
    process::{Child, Command, Stdio},
    time::Duration,
};

use localshelld::config::Config;
use serde_json::{Value, json};

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cli(user: bool) -> Command {
    let command = Command::new(if user {
        env!("CARGO_BIN_EXE_trustd-user")
    } else {
        env!("CARGO_BIN_EXE_trustd")
    });
    #[cfg(windows)]
    let command = {
        use std::os::windows::process::CommandExt;
        let mut command = command;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        command
    };
    command
}

#[tokio::test]
async fn actual_user_daemon_executes_as_current_user_without_root_backend() {
    verify_user_daemon(true).await;
}

#[cfg(windows)]
#[tokio::test]
async fn windowless_user_daemon_keeps_the_native_desktop_event_loop_and_http_alive() {
    verify_user_daemon(false).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn background_user_daemon_has_accessory_policy_and_keeps_http_alive() {
    verify_user_daemon(false).await;
}

#[cfg(target_os = "macos")]
async fn assert_accessory_process(pid: u32) {
    // Read AppKit's policy for only our disposable child. This needs neither
    // Accessibility permissions nor System Events automation access.
    let script = format!(
        "ObjC.import('AppKit'); const app = $.NSRunningApplication.runningApplicationWithProcessIdentifier({pid}); app.isNil() ? -1 : Number(app.activationPolicy);"
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let output = tokio::process::Command::new("/usr/bin/osascript")
                .args(["-l", "JavaScript", "-e", &script])
                .kill_on_drop(true)
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "AppKit policy probe failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            if String::from_utf8_lossy(&output.stdout).trim() == "1" {
                // Accessory (1), not Regular (0) or Prohibited (2).
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("GUI user daemon must use the accessory activation policy");
}

async fn verify_user_daemon(headless: bool) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("user/config.toml");
    let mut init = cli(false)
        .args(["init", "--user", "--password-stdin", "--config"])
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    init.stdin
        .take()
        .unwrap()
        .write_all(b"localshelld integration test password\n")
        .unwrap();
    let initialized = init.wait_with_output().unwrap();
    if !initialized.status.success() {
        // Elevated CI runners must refuse user mode, never silently treat it as
        // an unprivileged server. Unix CI separately tests both real identities.
        let error = String::from_utf8_lossy(&initialized.stderr);
        assert!(error.contains("non-elevated"), "user init failed: {error}");
        let serve = cli(true)
            .args(["--config"])
            .arg(&path)
            .args(headless.then_some("--headless"))
            .output()
            .unwrap();
        assert!(!serve.status.success());
        assert!(String::from_utf8_lossy(&serve.stderr).contains("non-elevated"));
        return;
    }
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    let mut config = Config::load(&path).unwrap();
    config.bind = address;
    config.privileged_daemon = "127.0.0.1:0".parse().unwrap();
    config.save(&path).unwrap();
    drop(reservation);
    let mut daemon = Daemon(
        cli(true)
            .args(["--config"])
            .arg(&path)
            .args(headless.then_some("--headless"))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let base = format!("http://{address}");
    let health: Value = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            assert!(daemon.0.try_wait().unwrap().is_none(), "user daemon exited");
            if let Ok(response) = client.get(format!("{base}/health")).send().await {
                break response.json().await.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(health["daemon"], "user");
    #[cfg(target_os = "macos")]
    if !headless {
        assert_accessory_process(daemon.0.id()).await;
    }
    let page = client
        .get(format!("{base}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("本机授权管理"));
    assert!(page.contains("/assets/"));
    let issued: Value = client.post(format!("{base}/v1/admin/tokens/issue")).json(&json!({"credential":{"type":"password", "value":"localshelld integration test password"}}))
        .send().await.unwrap().json().await.unwrap();
    assert!(issued["warning"].as_str().unwrap().contains("current-user"));
    let entered: Value = client
        .post(format!("{base}/v1/sessions/enter"))
        .json(&json!({"token":issued["token"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    if cfg!(windows) {
        assert_eq!(entered["privilege_levels"], json!(["current-user"]));
    } else {
        assert_eq!(entered["sudo_available"], false);
    }
    let handle = entered["handle"].as_str().unwrap();
    let mut body = localshelld::execution::execution_argument(
        localshelld::execution::PrivilegeLevel::CurrentUser,
    );
    body["handle"] = json!(handle);
    body["command"] = json!("whoami");
    let result: Value = client
        .post(format!("{base}/v1/commands/run"))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let expected = Command::new("whoami").output().unwrap();
    assert!(expected.status.success());
    assert_eq!(
        result["output"].as_str().unwrap().trim(),
        String::from_utf8_lossy(&expected.stdout).trim()
    );
    let mut body = localshelld::execution::execution_argument(
        localshelld::execution::PrivilegeLevel::privileged(),
    );
    body["handle"] = json!(handle);
    body["command"] = json!("whoami");
    let denied = client
        .post(format!("{base}/v1/commands/run"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);
    client
        .post(format!("{base}/v1/sessions/destroy"))
        .json(&json!({"handle":handle}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}
