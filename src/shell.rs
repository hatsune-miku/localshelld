use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};

mod transport;
use crate::execution::RunOptions;
use transport::Process;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellKind {
    PowerShell,
    Bash,
}
impl ShellKind {
    pub const fn native() -> Self {
        if cfg!(windows) {
            Self::PowerShell
        } else {
            Self::Bash
        }
    }
    pub const fn name(self) -> &'static str {
        match self {
            Self::PowerShell => "PowerShell",
            Self::Bash => "Bash",
        }
    }
    pub const fn executable(self) -> &'static str {
        match self {
            Self::PowerShell => "pwsh",
            Self::Bash => "/bin/bash",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionResult {
    /// Compatibility text; stdout followed by stderr, not interleaving order.
    pub output: String,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub truncated: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub session_alive: bool,
    pub session_ended_reason: Option<String>,
    pub cwd: Option<String>,
    pub identity: serde_json::Value,
    /// The shell exited while running or right after this command (for example
    /// `exit`, `exec`, or a failure under `set -e`). The captured output and the
    /// shell's own exit code are still returned, but the session is now gone and
    /// further commands on this handle will fail.
    pub session_ended: bool,
}

#[derive(Debug, Error)]
pub enum ShellError {
    #[error("failed to start shell: {0}")]
    Start(#[source] std::io::Error),
    #[error("shell session ended unexpectedly or was destroyed")]
    Ended,
    #[error("shell I/O failed: {0}")]
    Io(#[source] std::io::Error),
    #[error("shell returned an invalid response")]
    InvalidResponse,
    #[error("command rejected: {0}")]
    Rejected(&'static str),
}

struct Request {
    command: String,
    options: RunOptions,
    reply: oneshot::Sender<Result<ExecutionResult, ShellError>>,
}

/// The worker serializes commands; cancellation never needs the execution lock.
/// A disconnected caller does not abandon a half-read protocol response.
pub struct Shell {
    requests: mpsc::Sender<Request>,
    cancel: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    identity: serde_json::Value,
}
impl Shell {
    pub async fn spawn(executable: &str, max_output_bytes: usize) -> Result<Self, ShellError> {
        Self::spawn_kind(ShellKind::native(), executable, max_output_bytes).await
    }
    pub async fn spawn_with_environment(
        executable: &str,
        max_output_bytes: usize,
        environment: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, ShellError> {
        Self::from_process(Process::spawn(
            ShellKind::native(),
            executable,
            max_output_bytes,
            environment,
        )?)
    }
    async fn spawn_kind(
        kind: ShellKind,
        executable: &str,
        max_output_bytes: usize,
    ) -> Result<Self, ShellError> {
        let process = Process::spawn(kind, executable, max_output_bytes, &Default::default())?;
        Self::from_process(process)
    }
    #[cfg(windows)]
    pub async fn spawn_administrator(
        executable: &str,
        max_output_bytes: usize,
        context: &localshelld_native_process::UserContext,
    ) -> Result<Self, ShellError> {
        Self::spawn_administrator_with_environment(
            executable,
            max_output_bytes,
            context,
            &Default::default(),
        )
        .await
    }
    #[cfg(windows)]
    pub async fn spawn_administrator_with_environment(
        executable: &str,
        max_output_bytes: usize,
        context: &localshelld_native_process::UserContext,
        environment: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, ShellError> {
        Self::from_process(Process::spawn_administrator(
            executable,
            max_output_bytes,
            context,
            environment,
        )?)
    }
    fn from_process(mut process: Process) -> Result<Self, ShellError> {
        let identity = process.identity.clone();
        let (requests, mut receiver) = mpsc::channel::<Request>(16);
        let (cancel, mut cancelled) = watch::channel(false);
        let (done, finished) = watch::channel(false);
        tokio::spawn(async move {
            loop {
                let request = tokio::select! {
                    biased;
                    _ = cancelled.changed() => break,
                    _ = process.child.wait() => break,
                    request = receiver.recv() => match request { Some(r) => r, None => break },
                };
                let result = process
                    .execute(&request.command, &request.options, &mut cancelled)
                    .await;
                // Any error, or a shell that exited during the command, ends the
                // session; its children are reaped before the next caller runs.
                let ended = match &result {
                    Err(_) => true,
                    Ok(execution) => execution.session_ended,
                };
                if ended {
                    process.terminate().await;
                }
                let _ = request.reply.send(result);
                if ended {
                    break;
                }
            }
            process.terminate().await;
            let _ = done.send(true);
        });
        Ok(Self {
            requests,
            cancel,
            finished,
            identity,
        })
    }
    pub fn identity(&self) -> &serde_json::Value {
        &self.identity
    }
    pub async fn execute(
        &self,
        command: &str,
        timeout_seconds: Option<u64>,
    ) -> Result<ExecutionResult, ShellError> {
        self.execute_with_options(
            command,
            RunOptions {
                timeout_seconds,
                ..RunOptions::default()
            },
        )
        .await
    }
    pub async fn execute_with_options(
        &self,
        command: &str,
        options: RunOptions,
    ) -> Result<ExecutionResult, ShellError> {
        if command.as_bytes().contains(&0) {
            // A NUL cannot survive transport through the shell's own string
            // handling, so it would silently truncate the command; reject it.
            return Err(ShellError::Rejected("command must not contain NUL bytes"));
        }
        if self.is_finished() {
            return Err(ShellError::Ended);
        }
        let (reply, response) = oneshot::channel();
        self.requests
            .send(Request {
                command: command.to_owned(),
                options,
                reply,
            })
            .await
            .map_err(|_| ShellError::Ended)?;
        response.await.map_err(|_| ShellError::Ended)?
    }
    pub fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
    pub fn is_finished(&self) -> bool {
        *self.cancel.borrow() || *self.finished.borrow()
    }
    pub async fn terminate(&self) {
        self.cancel();
        self.wait_ended().await;
    }
    pub async fn wait_ended(&self) {
        let mut finished = self.finished.clone();
        let _ = finished.wait_for(|done| *done).await;
    }
}
impl Drop for Shell {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Reconstructed by Bash's `printf %b`. Escape both the decoding escape character
/// and the `read` field separator: an unescaped trailing `|` would be discarded.
fn encode_bash_command(command: &str) -> String {
    let mut encoded = String::with_capacity(command.len());
    for byte in command.bytes() {
        if (0x20..=0x7e).contains(&byte) && !matches!(byte, b'\\' | b'|') {
            encoded.push(byte as char);
        } else {
            write!(&mut encoded, "\\{byte:03o}").unwrap();
        }
    }
    encoded
}

fn append_output(output: &mut Vec<u8>, bytes: &[u8], limit: usize, truncated: &mut bool) {
    let count = bytes.len().min(limit.saturating_sub(output.len()));
    output.extend_from_slice(&bytes[..count]);
    *truncated |= count < bytes.len();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::Stdio, sync::Arc, time::Duration};
    use tokio::process::Command;
    #[cfg(unix)]
    use tokio::time::Instant;

    #[test]
    fn bash_encoding_escapes_protocol_separators_and_preserves_plain_ascii() {
        assert_eq!(encode_bash_command("echo hello"), "echo hello");
        assert_eq!(encode_bash_command("echo hello |"), "echo hello \\174");
        assert_eq!(encode_bash_command("|a||b|"), "\\174a\\174\\174b\\174");
        assert_eq!(encode_bash_command("a\\b\n\r"), "a\\134b\\012\\015");
        assert_eq!(encode_bash_command("值"), "\\345\\200\\274");
    }

    fn backends() -> Vec<(ShellKind, String)> {
        let native = ShellKind::native();
        let mut backends = vec![(native, native.executable().into())];
        if cfg!(windows)
            && let Ok(path) = std::env::var("LOCALSHELLD_TEST_BASH")
        {
            backends.push((ShellKind::Bash, path));
        }
        backends
    }

    fn source(kind: ShellKind, powershell: &'static str, bash: &'static str) -> &'static str {
        match kind {
            ShellKind::PowerShell => powershell,
            ShellKind::Bash => bash,
        }
    }

    async fn run(shell: &Shell, script: &str) -> ExecutionResult {
        tokio::time::timeout(Duration::from_secs(20), shell.execute(script, None))
            .await
            .expect("shell protocol stalled")
            .unwrap()
    }

    #[tokio::test]
    async fn marks_broker_sessions_to_prevent_accidental_self_update() {
        for (kind, path) in backends() {
            let shell = Shell::spawn_kind(kind, &path, 1024).await.unwrap();
            let result = run(
                &shell,
                source(
                    kind,
                    "$env:LOCALSHELLD_SESSION",
                    "printf '%s\\n' \"$LOCALSHELLD_SESSION\"",
                ),
            )
            .await;
            assert_eq!(result.output.trim(), "1");
            shell.terminate().await;
        }
    }

    #[tokio::test]
    async fn credential_environment_is_data_not_source_and_reaches_child_programs() {
        use base64::Engine as _;
        let value = "fixture-only ' \" $ ` ; &\nUnicode: 密码";
        let environment =
            std::collections::BTreeMap::from([("TEST_SECRET_VALUE".into(), value.into())]);
        for (kind, path) in backends() {
            let shell =
                Shell::from_process(Process::spawn(kind, &path, 64 * 1024, &environment).unwrap())
                    .unwrap();
            let result = run(
                &shell,
                source(
                    kind,
                    "[Console]::Write($env:TEST_SECRET_VALUE)",
                    "printf '%s' \"$TEST_SECRET_VALUE\"",
                ),
            )
            .await;
            assert_eq!(result.stdout, value, "{kind:?}");
            let child = run(&shell, source(kind,
                "pwsh -NoProfile -NonInteractive -Command '[Console]::Write([Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($env:TEST_SECRET_VALUE)))'",
                "/bin/bash --noprofile --norc -c 'printf %s \"$TEST_SECRET_VALUE\" | base64'"
            )).await;
            assert_eq!(
                child.stdout.trim(),
                base64::engine::general_purpose::STANDARD.encode(value),
                "{kind:?}"
            );
            shell.terminate().await;
        }
    }

    #[tokio::test]
    async fn native_syntax_unicode_pipelines_wildcards_and_state() {
        for (kind, path) in backends() {
            let shell = Shell::spawn_kind(kind, &path, 1024 * 1024).await.unwrap();
            let script = source(
                kind,
                "$values = 1..5\n$values | Where-Object { $_ -gt 3 } | ForEach-Object { \"值=$_\" }; Get-ChildItem -Name Cargo.*",
                "for value in 4 5; do printf '值=%s\\n' \"$value\"; done | cat\nprintf '%s\\n' Cargo.*\ncat <<'EOF'\nheredoc value\nEOF",
            );
            let result = run(&shell, script).await;
            assert!(result.success, "{kind:?}: {}", result.output);
            for expected in ["值=4", "值=5", "Cargo.toml"] {
                assert!(
                    result.output.contains(expected),
                    "{kind:?}: {}",
                    result.output
                );
            }
            run(&shell, source(kind,
                "$localshelld_test = 'persisted'; $env:LOCALSHELLD_TEST = 'yes'; function localshelld_function { 'function-ok' }; Set-Location src",
                "localshelld_test='persisted'; export LOCALSHELLD_TEST=yes; localshelld_function() { printf 'function-ok\\n'; }; cd src")).await;
            let result = run(
                &shell,
                source(
                    kind,
                    "\"$localshelld_test/$env:LOCALSHELLD_TEST\"; localshelld_function; (Get-Location).Path",
                    "printf '%s/%s\\n' \"$localshelld_test\" \"$LOCALSHELLD_TEST\"; localshelld_function; pwd",
                ),
            )
            .await;
            for expected in ["persisted/yes", "function-ok", "src"] {
                assert!(
                    result.output.contains(expected),
                    "{kind:?}: {}",
                    result.output
                );
            }
            shell.terminate().await;
        }
    }

    #[tokio::test]
    async fn output_framing_truncation_errors_and_next_command() {
        for (kind, path) in backends() {
            let shell = Shell::spawn_kind(kind, &path, 1024).await.unwrap();
            let result = run(
                &shell,
                source(
                    kind,
                    "[Console]::Out.Write('direct-without-newline:'); 'captured'",
                    "printf 'direct-without-newline:captured'",
                ),
            )
            .await;
            assert!(result.output.contains("direct-without-newline:captured"));
            let controls = run(
                &shell,
                source(
                    kind,
                    "[Console]::Out.Write(\"a$([char]0)b$([char]30)other$([char]31)c\")",
                    "printf 'a\\000b\\036other\\037c'",
                ),
            )
            .await;
            assert_eq!(controls.output, "a\0b\u{001e}other\u{001f}c");
            let result = run(
                &shell,
                source(
                    kind,
                    "[Console]::Out.Write(('x' * 40000))",
                    "printf '%40000s' ''",
                ),
            )
            .await;
            assert!(result.success && result.truncated);
            assert_eq!(result.output.len(), 1024);
            assert!(
                run(&shell, "echo next-command")
                    .await
                    .output
                    .contains("next-command")
            );
            let error = run(
                &shell,
                source(
                    kind,
                    "Write-Error 'expected failure'",
                    "printf 'expected failure' >&2; false",
                ),
            )
            .await;
            assert!(!error.success);
            assert!(error.output.contains("expected failure"));
            let native = run(
                &shell,
                source(kind, "pwsh -NoProfile -Command 'exit 7'", "(exit 7)"),
            )
            .await;
            assert_eq!(native.exit_code, Some(7));
            assert!(run(&shell, "echo recovered").await.success);
            shell.terminate().await;
        }
    }

    #[tokio::test]
    async fn explicit_timeout_destroys_session_but_has_no_server_maximum() {
        for (kind, path) in backends() {
            let shell = Shell::spawn_kind(kind, &path, 1024).await.unwrap();
            assert!(
                shell
                    .execute("echo above-old-limit", Some(3600))
                    .await
                    .unwrap()
                    .success
            );
            let result = shell
                .execute(
                    source(
                        kind,
                        "while ($true) { Start-Sleep -Milliseconds 100 }",
                        "while :; do :; done",
                    ),
                    Some(1),
                )
                .await;
            let result = result.unwrap();
            assert!(result.timed_out && result.session_ended && !result.session_alive);
            assert_eq!(result.session_ended_reason.as_deref(), Some("timeout"));
            assert!(shell.execute("echo invalid", None).await.is_err());
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_unlimited_and_queued_commands() {
        for (kind, path) in backends() {
            let shell = Arc::new(Shell::spawn_kind(kind, &path, 1024).await.unwrap());
            run(&shell, "echo ready").await;
            let running = {
                let shell = Arc::clone(&shell);
                tokio::spawn(async move {
                    shell
                        .execute(
                            source(
                                kind,
                                "while ($true) { Start-Sleep -Milliseconds 100 }",
                                "while :; do :; done",
                            ),
                            None,
                        )
                        .await
                })
            };
            tokio::time::sleep(Duration::from_millis(200)).await;
            let queued = {
                let shell = Arc::clone(&shell);
                tokio::spawn(async move { shell.execute("echo never", None).await })
            };
            tokio::time::timeout(Duration::from_secs(5), shell.terminate())
                .await
                .unwrap();
            assert_eq!(
                running
                    .await
                    .unwrap()
                    .unwrap()
                    .session_ended_reason
                    .as_deref(),
                Some("cancelled")
            );
            assert!(queued.await.unwrap().is_err());
        }
    }

    #[tokio::test]
    async fn disconnected_request_does_not_corrupt_next_response() {
        for (kind, path) in backends() {
            let shell = Arc::new(Shell::spawn_kind(kind, &path, 1024).await.unwrap());
            run(&shell, "echo ready").await;
            let request = {
                let shell = Arc::clone(&shell);
                tokio::spawn(async move {
                    shell
                        .execute(
                            source(
                                kind,
                                "Start-Sleep -Seconds 1; 'first'",
                                "sleep 1; echo first",
                            ),
                            None,
                        )
                        .await
                })
            };
            tokio::time::sleep(Duration::from_millis(200)).await;
            request.abort();
            let result = run(&shell, "echo second").await;
            assert!(result.output.contains("second"));
            assert!(!result.output.contains("first"));
            shell.terminate().await;
        }
    }

    #[tokio::test]
    async fn shell_exit_reports_exit_code_then_ends_session() {
        for (kind, path) in backends() {
            let shell = Shell::spawn_kind(kind, &path, 1024).await.unwrap();
            let result =
                tokio::time::timeout(Duration::from_secs(10), shell.execute("exit 3", None))
                    .await
                    .expect("a shell exit must not hang")
                    .expect("a shell exit is reported as a result, not an I/O error");
            assert!(result.session_ended);
            assert_eq!(result.exit_code, Some(3));
            // The session is gone; the next command fails fast rather than hanging.
            assert!(
                tokio::time::timeout(Duration::from_secs(10), shell.execute("echo next", None))
                    .await
                    .expect("the next command must not hang")
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn rejects_commands_containing_nul() {
        for (kind, path) in backends() {
            let shell = Shell::spawn_kind(kind, &path, 1024).await.unwrap();
            assert!(matches!(
                shell.execute("echo a\0b", None).await,
                Err(ShellError::Rejected(_))
            ));
            // A rejected command must leave the session usable.
            assert!(run(&shell, "echo alive").await.output.contains("alive"));
            shell.terminate().await;
        }
    }

    #[tokio::test]
    async fn bash_shell_exit_preserves_output_and_exit_code() {
        // set -e and a bare exit are common in agent scripts; the diagnostic
        // output and exit code must survive, not be replaced by a bare error.
        let shell = match bash_backend() {
            Some(shell) => shell.await,
            None => return,
        };
        let exited = shell
            .execute("echo important-diagnostic; exit 5", None)
            .await
            .expect("a clean exit is a result");
        assert!(exited.session_ended);
        assert_eq!(exited.exit_code, Some(5));
        assert!(exited.output.contains("important-diagnostic"));

        let shell = bash_backend()
            .expect("the configured Bash backend is available")
            .await;
        let sete = shell
            .execute("set -e; echo before; false; echo after", None)
            .await
            .expect("a set -e failure is a result");
        assert!(sete.session_ended);
        assert_eq!(sete.exit_code, Some(1));
        assert!(sete.output.contains("before"));
        assert!(!sete.output.contains("after"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_background_job_does_not_stall_exit_and_is_killed() {
        let shell = match bash_backend() {
            Some(shell) => shell.await,
            None => return,
        };
        let started = Instant::now();
        // The background sleep holds the output pipe open after the shell exits.
        let result = tokio::time::timeout(
            Duration::from_secs(8),
            shell.execute("sleep 300 & echo $!; exit 0", None),
        )
        .await
        .expect("must not wait for the detached background job to finish")
        .expect("exit is reported");
        assert!(result.session_ended);
        assert!(started.elapsed() < Duration::from_secs(5));

        // The session-ended path also signals the process group, so the orphaned
        // child does not keep running with the shell's privileges.
        #[cfg(unix)]
        {
            let pid: i32 = result.output.trim().parse().expect("a background PID");
            let mut waited = 0;
            while process_is_alive(pid) && waited < 50 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                waited += 1;
            }
            assert!(
                !process_is_alive(pid),
                "background child {pid} survived exit"
            );
        }
    }

    #[tokio::test]
    async fn bash_command_redirection_and_read_redefinition_stay_isolated() {
        let shell = match bash_backend() {
            Some(shell) => shell.await,
            None => return,
        };
        // `exec >/dev/null` must only affect its own command, not the session.
        assert_eq!(run(&shell, "exec >/dev/null; echo hidden").await.output, "");
        assert!(run(&shell, "echo visible").await.output.contains("visible"));
        // A user function named `read` must not break the protocol reader.
        run(&shell, "read() { echo NO; }; echo defined").await;
        assert!(run(&shell, "echo next").await.output.contains("next"));
        shell.terminate().await;
    }

    #[tokio::test]
    async fn bash_trailing_pipe_is_rejected_without_executing_partial_command() {
        let Some((kind, path)) = backends()
            .into_iter()
            .find(|(kind, _)| *kind == ShellKind::Bash)
        else {
            return;
        };
        let invalid = "printf SHOULD_NOT_RUN |";
        // Bash 3.2 (macOS) reports 1 for this eval syntax error; newer Bash
        // reports 2. The broker must preserve the configured shell's result,
        // not impose one version's error code on every supported backend.
        let native = Command::new(&path)
            .args([
                "--noprofile",
                "--norc",
                "-c",
                "builtin eval -- \"$1\"",
                "localshelld-test",
                invalid,
            ])
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .env_remove("SHELLOPTS")
            .env_remove("BASHOPTS")
            .env_remove("CDPATH")
            .stdin(Stdio::null())
            .output()
            .await
            .expect("the configured Bash can evaluate the reference command");
        let expected_exit = native.status.code().expect("Bash exited normally");
        assert_ne!(expected_exit, 0);
        assert!(native.stdout.is_empty());

        let shell = Shell::spawn_kind(kind, &path, 1024).await.unwrap();
        let result = run(&shell, invalid).await;
        assert_eq!(result.exit_code, Some(expected_exit));
        assert!(!result.success);
        assert!(!result.session_ended);
        assert!(!result.output.starts_with("SHOULD_NOT_RUN"));
        // An invalid pipeline must not execute its left side or poison the next call.
        let result = run(&shell, "localshelld_pipe_test=changed |").await;
        assert!(!result.success);
        assert_eq!(
            run(&shell, "printf '%s' \"${localshelld_pipe_test-unset}\"")
                .await
                .output,
            "unset"
        );
        shell.terminate().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_session_teardown_kills_child_processes() {
        let shell = match bash_backend() {
            Some(shell) => shell.await,
            None => return,
        };
        let pid: i32 = run(&shell, "sleep 300 & echo $!")
            .await
            .output
            .trim()
            .parse()
            .expect("a background PID");
        assert!(process_is_alive(pid));
        shell.terminate().await;
        // The shell's process group was signalled, so the child dies too.
        let mut deadline = 0;
        while process_is_alive(pid) && deadline < 50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            deadline += 1;
        }
        assert!(
            !process_is_alive(pid),
            "child {pid} survived session teardown"
        );
    }

    /// The native bash backend, or `None` on a platform whose native shell is
    /// not bash and where no test bash was provided.
    fn bash_backend() -> Option<impl std::future::Future<Output = Shell>> {
        backends()
            .into_iter()
            .find(|(kind, _)| *kind == ShellKind::Bash)
            .map(|(kind, path)| async move { Shell::spawn_kind(kind, &path, 1024).await.unwrap() })
    }

    #[cfg(unix)]
    fn process_is_alive(pid: i32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}
