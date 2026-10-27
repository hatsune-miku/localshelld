use localshelld::{execution::RunOptions, shell::Shell};

fn script(windows: &str, unix: &str) -> String {
    if cfg!(windows) { windows } else { unix }.to_owned()
}
async fn shell() -> Shell {
    Shell::spawn(localshelld::shell::ShellKind::native().executable(), 8192)
        .await
        .unwrap()
}

#[tokio::test]
async fn input_eof_streams_and_state_are_separate() {
    let shell = shell().await;
    let command = script(
        "$saved='kept'; [Console]::Out.Write([Console]::In.ReadToEnd()); [Console]::Error.Write('stderr-only')",
        "saved=kept; cat; printf stderr-only >&2",
    );
    let result = shell
        .execute_with_options(
            &command,
            RunOptions {
                stdin: "你好\nsecond\0last".into(),
                timeout_seconds: Some(10),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.stdout, "你好\nsecond\0last");
    assert_eq!(result.stderr, "stderr-only");
    assert!(result.success && result.session_alive, "{result:?}");
    assert!(result.cwd.as_ref().is_some_and(|value| !value.is_empty()));
    assert!(result.identity["user"].is_string());
    let next = shell
        .execute(
            &script(
                "[Console]::Out.Write([Console]::In.ReadToEnd()); $saved",
                "cat; printf '%s' \"$saved\"",
            ),
            Some(10),
        )
        .await
        .unwrap();
    assert_eq!(next.stdout.trim(), "kept");
    assert!(next.stderr.is_empty());
    shell.terminate().await;
}

#[tokio::test]
async fn native_program_receives_stdin_and_stderr_does_not_mean_failure() {
    let shell = shell().await;
    let command = script(
        "pwsh -NoProfile -NonInteractive -Command '[Console]::OpenStandardInput().CopyTo([Console]::OpenStandardOutput()); [Console]::Error.Write(\"native-error-text\"); exit 0'",
        "/bin/sh -c 'cat; printf native-error-text >&2; exit 0'",
    );
    let result = shell
        .execute_with_options(
            &command,
            RunOptions {
                stdin: "native input 值\n".into(),
                timeout_seconds: Some(10),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.stdout.trim(), "native input 值", "{result:?}");
    assert!(result.stderr.contains("native-error-text"), "{result:?}");
    assert!(!result.stdout.contains("native-error-text"));
    assert!(result.success, "{result:?}");
    assert_eq!(result.exit_code, Some(0));
    shell.terminate().await;
}

#[tokio::test]
async fn stream_truncation_is_independent_and_does_not_poison_next_command() {
    let shell = Shell::spawn(localshelld::shell::ShellKind::native().executable(), 128)
        .await
        .unwrap();
    let result = shell
        .execute(
            &script(
                "[Console]::Out.Write(('o' * 40000)); [Console]::Error.Write(('e' * 40000))",
                "printf '%40000s' '' | tr ' ' o; printf '%40000s' '' | tr ' ' e >&2",
            ),
            Some(10),
        )
        .await
        .unwrap();
    assert_eq!(result.stdout, "o".repeat(128));
    assert_eq!(result.stderr, "e".repeat(128));
    assert!(result.stdout_truncated && result.stderr_truncated && result.truncated);
    assert!(
        shell
            .execute("echo after-truncation", Some(10))
            .await
            .unwrap()
            .success
    );
    shell.terminate().await;
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_errors_still_fail_after_error_history_is_full() {
    let shell = shell().await;
    shell
        .execute(
            "1..300 | ForEach-Object { Write-Error history -ErrorAction SilentlyContinue }",
            Some(10),
        )
        .await
        .unwrap();
    let result = shell
        .execute("Write-Error newest-error", Some(10))
        .await
        .unwrap();
    assert_eq!(result.exit_code, Some(1));
    assert!(!result.success && result.session_alive);
    assert!(result.stderr.contains("newest-error"));
    shell.terminate().await;
}

#[tokio::test]
async fn timeout_retains_partial_output_and_explains_session_loss() {
    let shell = shell().await;
    shell.execute("echo ready", Some(10)).await.unwrap();
    let command = script(
        "[Console]::Out.Write('partial-out'); [Console]::Error.Write('partial-err'); Start-Sleep -Seconds 30",
        "printf partial-out; printf partial-err >&2; sleep 30",
    );
    let result = shell.execute(&command, Some(1)).await.unwrap();
    assert_eq!(result.stdout, "partial-out");
    assert_eq!(result.stderr, "partial-err");
    assert!(result.timed_out && result.session_ended && !result.session_alive);
    assert!(!result.success);
    assert_eq!(result.exit_code, None);
    assert_eq!(result.session_ended_reason.as_deref(), Some("timeout"));
    assert!(result.duration_ms >= 900);
    assert!(shell.execute("echo never", Some(10)).await.is_err());
}

#[cfg(windows)]
#[tokio::test]
async fn windows_destroy_and_timeout_kill_children_and_grandchildren() {
    let command = r#"
$childScript = @'
$grandInfo = [Diagnostics.ProcessStartInfo]::new('pwsh')
$grandInfo.UseShellExecute = $false
$grandInfo.CreateNoWindow = $true
$grandInfo.ArgumentList.Add('-NoProfile')
$grandInfo.ArgumentList.Add('-NonInteractive')
$grandInfo.ArgumentList.Add('-Command')
$grandInfo.ArgumentList.Add('Start-Sleep -Seconds 60')
$grandchild = [Diagnostics.Process]::Start($grandInfo)
[Console]::Out.WriteLine($grandchild.Id)
Start-Sleep -Seconds 60
'@
$childInfo = [Diagnostics.ProcessStartInfo]::new('pwsh')
$childInfo.UseShellExecute = $false
$childInfo.CreateNoWindow = $true
$childInfo.RedirectStandardOutput = $true
$childInfo.ArgumentList.Add('-NoProfile')
$childInfo.ArgumentList.Add('-NonInteractive')
$childInfo.ArgumentList.Add('-Command')
$childInfo.ArgumentList.Add($childScript)
$child = [Diagnostics.Process]::Start($childInfo)
[Console]::Out.WriteLine($child.Id)
[Console]::Out.WriteLine($child.StandardOutput.ReadLine())
"#;
    for timeout in [false, true] {
        let shell = shell().await;
        shell.execute("echo ready", Some(10)).await.unwrap();
        let result = if timeout {
            shell
                .execute(&format!("{command}\nStart-Sleep -Seconds 30"), Some(3))
                .await
                .unwrap()
        } else {
            shell.execute(command, Some(10)).await.unwrap()
        };
        assert_eq!(result.timed_out, timeout, "{result:?}");
        let pids: Vec<u32> = result
            .stdout
            .lines()
            .map(|line| line.trim().parse().unwrap())
            .collect();
        assert_eq!(pids.len(), 2, "child and grandchild PIDs must be captured");
        shell.terminate().await;
        let output = localshelld::process::background_command("pwsh")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!(
                    "if (Get-Process -Id {},{} -ErrorAction SilentlyContinue) {{ exit 1 }}",
                    pids[0], pids[1]
                ),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "descendants {pids:?} survived session teardown (timeout={timeout})"
        );
    }
}
