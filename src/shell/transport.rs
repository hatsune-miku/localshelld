use super::{ExecutionResult, ShellError, ShellKind, append_output, encode_bash_command};
use crate::execution::RunOptions;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use rand_core::{OsRng, RngCore};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::Command,
    sync::watch,
    time::Instant,
};

type Reader = Box<dyn AsyncRead + Unpin + Send>;
type Writer = Box<dyn AsyncWrite + Unpin + Send>;

pub(super) enum Child {
    Native(tokio::process::Child),
    #[cfg(windows)]
    Windows(localshelld_native_process::Child),
}
impl Child {
    fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        match self {
            Self::Native(child) => child.try_wait().map(|status| {
                status.map(|status| {
                    status.code().unwrap_or({
                        #[cfg(unix)]
                        {
                            use std::os::unix::process::ExitStatusExt;
                            status.signal().map_or(-1, |signal| 128 + signal)
                        }
                        #[cfg(not(unix))]
                        {
                            -1
                        }
                    })
                })
            }),
            #[cfg(windows)]
            Self::Windows(child) => child.try_wait(),
        }
    }
    pub async fn wait(&mut self) -> std::io::Result<i32> {
        loop {
            if let Some(code) = self.try_wait()? {
                return Ok(code);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    async fn kill_tree(&mut self) {
        match self {
            Self::Native(child) => {
                #[cfg(unix)]
                if let Some(pid) = child.id() {
                    kill_descendants(pid).await;
                }
                let _ = child.kill().await;
            }
            #[cfg(windows)]
            Self::Windows(child) => {
                let _ = child.kill_tree();
            }
        }
    }
}

pub(super) struct Process {
    kind: ShellKind,
    pub child: Child,
    stdin: Writer,
    stdout: Reader,
    stderr: Reader,
    max_output_bytes: usize,
    pub identity: serde_json::Value,
    #[cfg(unix)]
    pgid: Option<i32>,
}
impl Process {
    pub fn spawn(
        kind: ShellKind,
        executable: &str,
        max_output_bytes: usize,
        environment: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, ShellError> {
        #[cfg(windows)]
        if kind == ShellKind::PowerShell {
            return Self::spawn_windows(executable, max_output_bytes, None, environment);
        }
        let mut command = Command::from(crate::process::background_command(executable));
        command.args(["--noprofile", "--norc", "-c", include_str!("bash.sh")]);
        for name in ["BASH_ENV", "ENV", "SHELLOPTS", "BASHOPTS", "CDPATH"] {
            command.env_remove(name);
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .envs(environment)
            .env("LOCALSHELLD_SESSION", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(ShellError::Start)?;
        Ok(Self {
            kind,
            stdin: Box::new(child.stdin.take().ok_or(ShellError::Ended)?),
            stdout: Box::new(child.stdout.take().ok_or(ShellError::Ended)?),
            stderr: Box::new(child.stderr.take().ok_or(ShellError::Ended)?),
            max_output_bytes,
            identity: crate::execution::current_identity().map_err(ShellError::Start)?,
            #[cfg(unix)]
            pgid: child.id().map(|pid| pid as i32),
            child: Child::Native(child),
        })
    }
    #[cfg(windows)]
    pub fn spawn_administrator(
        executable: &str,
        max_output_bytes: usize,
        context: &localshelld_native_process::UserContext,
        environment: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, ShellError> {
        Self::spawn_windows(executable, max_output_bytes, Some(context), environment)
    }
    #[cfg(windows)]
    fn spawn_windows(
        executable: &str,
        max_output_bytes: usize,
        context: Option<&localshelld_native_process::UserContext>,
        environment: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, ShellError> {
        let bytes: Vec<u8> = include_str!("powershell.ps1")
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let encoded = STANDARD.encode(bytes);
        let spawned = localshelld_native_process::spawn_with_environment(
            executable,
            &[
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-EncodedCommand",
                &encoded,
            ],
            context,
            environment,
        )
        .map_err(ShellError::Start)?;
        let identity = serde_json::to_value(&spawned.child.identity)
            .map_err(|_| ShellError::InvalidResponse)?;
        Ok(Self {
            kind: ShellKind::PowerShell,
            child: Child::Windows(spawned.child),
            stdin: Box::new(tokio::fs::File::from_std(spawned.stdin)),
            stdout: Box::new(tokio::fs::File::from_std(spawned.stdout)),
            stderr: Box::new(tokio::fs::File::from_std(spawned.stderr)),
            max_output_bytes,
            identity,
        })
    }
    pub async fn execute(
        &mut self,
        command: &str,
        options: &RunOptions,
        cancelled: &mut watch::Receiver<bool>,
    ) -> Result<ExecutionResult, ShellError> {
        if self.child.try_wait().map_err(ShellError::Io)?.is_some() {
            return Err(ShellError::Ended);
        }
        let started = Instant::now();
        let mut random = [0u8; 18];
        OsRng.fill_bytes(&mut random);
        let marker = STANDARD.encode(random);
        // Bash redirects stdin from a private file owned by this daemon. The
        // handle remains here so timeout/cancellation always removes the file.
        let mut input_file = None;
        let packet = match self.kind {
            ShellKind::PowerShell => format!(
                "{marker}|{}|{}\n",
                STANDARD.encode(command),
                STANDARD.encode(&options.stdin)
            ),
            ShellKind::Bash => {
                use std::io::Write;
                let mut file = tempfile::NamedTempFile::new().map_err(ShellError::Io)?;
                file.write_all(options.stdin.as_bytes())
                    .map_err(ShellError::Io)?;
                let path = encode_bash_command(&file.path().to_string_lossy());
                input_file = Some(file);
                format!("{marker}|{}|{path}\n", encode_bash_command(command))
            }
        };
        let mut capture = Capture::new(&marker, self.max_output_bytes);
        let deadline = options
            .timeout_seconds
            .and_then(|seconds| started.checked_add(Duration::from_secs(seconds)));
        let result = tokio::select! {
            biased;
            _ = cancelled.changed() => Ok(Some("cancelled")),
            _ = async { if let Some(deadline) = deadline { tokio::time::sleep_until(deadline).await } else { std::future::pending::<()>().await } } => Ok(Some("timeout")),
            result = self.execute_inner(&packet, &mut capture) => result.map(|ended| if ended { Some("shell_exited") } else { None }),
        };
        let ended_reason = result?;
        if ended_reason.is_some() {
            self.terminate().await;
            // Draining after termination captures output already in OS pipes.
            let _ = tokio::time::timeout(Duration::from_secs(1), self.drain(&mut capture)).await;
        }
        drop(input_file);
        let exit_code = if ended_reason == Some("timeout") || ended_reason == Some("cancelled") {
            None
        } else {
            capture
                .exit_code
                .or(self.child.try_wait().map_err(ShellError::Io)?)
        };
        let (stdout, stdout_truncated) = capture.stdout.finish();
        let (stderr, stderr_truncated) = capture.stderr.finish();
        let output = format!("{stdout}{stderr}");
        Ok(ExecutionResult {
            output,
            stdout,
            stderr,
            exit_code,
            success: ended_reason != Some("timeout")
                && ended_reason != Some("cancelled")
                && exit_code == Some(0),
            truncated: stdout_truncated || stderr_truncated,
            stdout_truncated,
            stderr_truncated,
            duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            timed_out: ended_reason == Some("timeout"),
            session_alive: ended_reason.is_none(),
            session_ended: ended_reason.is_some(),
            session_ended_reason: ended_reason.map(str::to_owned),
            cwd: capture.cwd,
            identity: self.identity.clone(),
        })
    }
    async fn execute_inner(
        &mut self,
        packet: &str,
        capture: &mut Capture,
    ) -> Result<bool, ShellError> {
        self.stdin
            .write_all(packet.as_bytes())
            .await
            .map_err(ShellError::Io)?;
        self.stdin.flush().await.map_err(ShellError::Io)?;
        let mut out = [0u8; 8192];
        let mut err = [0u8; 8192];
        loop {
            if capture.stdout.done && capture.stderr.done {
                return Ok(false);
            }
            tokio::select! {
                biased;
                count = self.stdout.read(&mut out), if !capture.stdout.done && !capture.stdout.closed => {
                    capture.feed(false, &out[..count.map_err(ShellError::Io)?])?;
                }
                count = self.stderr.read(&mut err), if !capture.stderr.done && !capture.stderr.closed => {
                    capture.feed(true, &err[..count.map_err(ShellError::Io)?])?;
                }
                code = self.child.wait() => { capture.exit_code = Some(code.map_err(ShellError::Io)?); return Ok(true); }
            }
        }
    }
    async fn drain(&mut self, capture: &mut Capture) -> Result<(), ShellError> {
        let mut out = [0u8; 8192];
        let mut err = [0u8; 8192];
        while !capture.stdout.closed || !capture.stderr.closed {
            tokio::select! {
                count = self.stdout.read(&mut out), if !capture.stdout.closed => capture.feed(false, &out[..count.map_err(ShellError::Io)?])?,
                count = self.stderr.read(&mut err), if !capture.stderr.closed => capture.feed(true, &err[..count.map_err(ShellError::Io)?])?,
            }
        }
        Ok(())
    }
    pub async fn terminate(&mut self) {
        self.child.kill_tree().await;
        #[cfg(unix)]
        if let Some(pid) = self.pgid.and_then(rustix::process::Pid::from_raw) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.child.wait().await;
    }
}

struct Stream {
    pending: Vec<u8>,
    output: Vec<u8>,
    limit: usize,
    truncated: bool,
    done: bool,
    closed: bool,
}
impl Stream {
    fn new(limit: usize) -> Self {
        Self {
            pending: Vec::new(),
            output: Vec::new(),
            limit,
            truncated: false,
            done: false,
            closed: false,
        }
    }
    fn finish(mut self) -> (String, bool) {
        if !self.done {
            append_output(
                &mut self.output,
                &self.pending,
                self.limit,
                &mut self.truncated,
            );
        }
        (
            String::from_utf8_lossy(&self.output).into_owned(),
            self.truncated,
        )
    }
}
struct Capture {
    stdout: Stream,
    stderr: Stream,
    needle: Vec<u8>,
    exit_code: Option<i32>,
    cwd: Option<String>,
}
impl Capture {
    fn new(marker: &str, limit: usize) -> Self {
        Self {
            stdout: Stream::new(limit),
            stderr: Stream::new(limit),
            needle: format!("\u{001e}{marker}|").into_bytes(),
            exit_code: None,
            cwd: None,
        }
    }
    fn feed(&mut self, stderr: bool, bytes: &[u8]) -> Result<(), ShellError> {
        let stream = if stderr {
            &mut self.stderr
        } else {
            &mut self.stdout
        };
        if bytes.is_empty() {
            stream.closed = true;
            return Ok(());
        }
        if stream.done {
            return Ok(());
        }
        stream.pending.extend_from_slice(bytes);
        if let Some(start) = stream
            .pending
            .windows(self.needle.len())
            .position(|value| value == self.needle)
        {
            let remainder = &stream.pending[start + self.needle.len()..];
            if let Some(end) = remainder.iter().position(|byte| *byte == 0x1f) {
                let frame = std::str::from_utf8(&remainder[..end])
                    .map_err(|_| ShellError::InvalidResponse)?;
                let (code, cwd) = frame.split_once('|').ok_or(ShellError::InvalidResponse)?;
                let code: i32 = code.parse().map_err(|_| ShellError::InvalidResponse)?;
                if self.exit_code.is_some_and(|existing| existing != code) {
                    return Err(ShellError::InvalidResponse);
                }
                self.exit_code = Some(code);
                if !cwd.is_empty() {
                    self.cwd = Some(
                        String::from_utf8_lossy(
                            &STANDARD
                                .decode(cwd)
                                .map_err(|_| ShellError::InvalidResponse)?,
                        )
                        .into_owned(),
                    );
                }
                append_output(
                    &mut stream.output,
                    &stream.pending[..start],
                    stream.limit,
                    &mut stream.truncated,
                );
                stream.pending.clear();
                stream.done = true;
            } else if remainder.len() > 65_536 {
                return Err(ShellError::InvalidResponse);
            }
        } else {
            let count = stream.pending.len().saturating_sub(self.needle.len() - 1);
            append_output(
                &mut stream.output,
                &stream.pending[..count],
                stream.limit,
                &mut stream.truncated,
            );
            stream.pending.drain(..count);
        }
        Ok(())
    }
}

#[cfg(unix)]
async fn kill_descendants(root: u32) {
    // Capture ancestry before killing the parent. Also handles children that
    // created another process group; the original group is killed separately.
    let Ok(output) = Command::new("/bin/ps")
        .args(["-eo", "pid=,ppid="])
        .output()
        .await
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let pairs: Vec<(u32, u32)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some((fields.next()?.parse().ok()?, fields.next()?.parse().ok()?))
        })
        .collect();
    let mut descendants = vec![root];
    let mut cursor = 0;
    while cursor < descendants.len() {
        let parent = descendants[cursor];
        for &(pid, ppid) in &pairs {
            if ppid == parent && !descendants.contains(&pid) {
                descendants.push(pid);
            }
        }
        cursor += 1;
    }
    for pid in descendants.into_iter().skip(1).rev() {
        if let Some(pid) = rustix::process::Pid::from_raw(pid as i32) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
}
