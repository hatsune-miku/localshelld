use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    auth::{AuthError, AuthManager, Credential, DEFAULT_TOKEN_TTL_SECONDS, TokenRecord},
    bridge::{self, ReplayGuard},
    config::Config,
    consent::{CONSENT_TIMEOUT, Choice, ConsentBroker, Decision, Prompt, validate_request},
    execution::{PrivilegeLevel, RunOptions},
    mcp,
    peer::Peer,
    settings::{Pairing, Settings},
    shell::{ExecutionResult, Shell, ShellError, ShellKind},
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub mode: DaemonMode,
    peer: Option<Peer>,
    pub(crate) auth: Arc<Mutex<AuthManager>>,
    sessions: Arc<Mutex<SessionStore>>,
    pub(crate) settings: Arc<Mutex<Settings>>,
    pub(crate) bridge_replay: ReplayGuard,
    pub(crate) consent: Option<ConsentBroker>,
    pub(crate) management: Arc<Mutex<crate::management::Management>>,
    pub(crate) mcp_sessions: Arc<mcp::TransportSessions>,
    interactive_permits: Arc<tokio::sync::Semaphore>,
}

struct Session {
    token_id: String,
    expires_at: Option<i64>,
    shell: Arc<Shell>,
    environment: Arc<crate::secrets::Environment>,
    remote_handle: Option<String>,
    #[cfg(windows)]
    administrator: Arc<tokio::sync::OnceCell<Arc<Shell>>>,
}
impl Session {
    fn metadata(&self) -> (Option<i64>, Vec<String>) {
        (self.expires_at, self.environment.keys())
    }
    fn cancel(&self) {
        self.shell.cancel();
        #[cfg(windows)]
        if let Some(shell) = self.administrator.get() {
            shell.cancel();
        }
    }
    async fn terminate(&self) {
        self.cancel();
        self.shell.terminate().await;
        #[cfg(windows)]
        if let Some(shell) = self.administrator.get() {
            shell.terminate().await;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DaemonMode {
    User,
    Privileged,
}

#[derive(Default)]
struct SessionStore {
    shutting_down: bool,
    by_handle: HashMap<String, Session>,
    by_token: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct EnterResult {
    pub handle: String,
    pub reused: bool,
    pub secret_keys: Vec<String>,
    pub shell: ShellKind,
    pub platform: &'static str,
    pub daemon: DaemonMode,
    #[serde(skip_serializing_if = "is_windows")]
    pub sudo_available: bool,
    pub privilege_levels: Vec<String>,
    pub identities: std::collections::BTreeMap<String, serde_json::Value>,
    pub expires_at: Option<i64>,
    pub expires_at_iso: Option<String>,
    pub message: String,
}
fn is_windows(_: &bool) -> bool {
    cfg!(windows)
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub(crate) fn with_status(status: u16, message: String) -> Self {
        Self {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            message,
        }
    }

    pub(crate) fn status_code(&self) -> u16 {
        self.status.as_u16()
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: message.into(),
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    pub(crate) fn peer(status: StatusCode) -> Self {
        Self {
            status,
            message: format!("privileged daemon rejected the request ({status})"),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        let status = match error {
            AuthError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            AuthError::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::UNAUTHORIZED,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl From<ShellError> for ApiError {
    fn from(error: ShellError) -> Self {
        let status = match error {
            ShellError::Rejected(_) => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

impl AppState {
    pub fn new(config: Config, auth: AuthManager) -> Self {
        Self {
            settings: Arc::new(Mutex::new(Settings::memory(config.clone()))),
            bridge_replay: ReplayGuard::default(),
            consent: None,
            management: Arc::new(Mutex::new(crate::management::Management::default())),
            mcp_sessions: Arc::new(mcp::TransportSessions::default()),
            interactive_permits: Arc::new(tokio::sync::Semaphore::new(crate::consent::MAX_PENDING)),
            mode: DaemonMode::Privileged,
            peer: None,
            config: Arc::new(config),
            auth: Arc::new(Mutex::new(auth)),
            sessions: Arc::new(Mutex::new(SessionStore::default())),
        }
    }

    pub fn with_storage(mut self, path: std::path::PathBuf) -> anyhow::Result<Self> {
        self.settings = Arc::new(Mutex::new(Settings::load(path, (*self.config).clone())?));
        Ok(self)
    }

    pub fn with_consent(mut self, consent: ConsentBroker) -> Self {
        self.consent = Some(consent);
        self
    }

    pub(crate) async fn pairing(&self) -> Result<Pairing, ApiError> {
        self.settings.lock().await.saved.pairing.clone().ok_or_else(|| ApiError::unavailable(
            "desktop daemon is not paired; pair the user and privileged daemons in the management UI (or use the user daemon at port 32120 for current-user execution)"
        ))
    }

    pub async fn enter_interactive(
        &self,
        agent_name: &str,
        request_reason: &str,
    ) -> Result<serde_json::Value, ApiError> {
        self.enter_interactive_with_secrets(agent_name, request_reason, &[])
            .await
    }

    pub async fn enter_interactive_with_secrets(
        &self,
        agent_name: &str,
        request_reason: &str,
        secret_keys: &[String],
    ) -> Result<serde_json::Value, ApiError> {
        use serde_json::json;
        validate_request(agent_name, request_reason)?;
        let secret_keys = crate::secrets::normalize_keys(secret_keys)?;
        let _permit = self
            .interactive_permits
            .try_acquire()
            .map_err(|_| ApiError::unavailable("too many pending authorization requests"))?;
        let deadline = tokio::time::Instant::now() + CONSENT_TIMEOUT;
        if self.mode == DaemonMode::User && self.settings.lock().await.saved.pairing.is_some() {
            let pairing = self.pairing().await?;
            let mut result: serde_json::Value = bridge::call(
                &pairing,
                DaemonMode::Privileged,
                bridge::Request::EnterInteractive {
                    agent_name: agent_name.into(),
                    request_reason: request_reason.into(),
                    secret_keys,
                },
                Some(CONSENT_TIMEOUT + Duration::from_secs(1)),
                65_536,
            )
            .await?;
            if result["status"] == "authorized" {
                let handle = result["handle"]
                    .as_str()
                    .ok_or_else(|| ApiError::unavailable("missing authorized session handle"))?;
                let attached = self.attach_remote(handle).await?;
                let btw = result["btw"].clone();
                result = serde_json::to_value(attached)
                    .map_err(|_| ApiError::internal("session serialization failed"))?;
                result["status"] = json!("authorized");
                result["btw"] = btw;
            }
            return Ok(result);
        }
        let (remembered, approved_keys) = {
            let settings = self.settings.lock().await;
            (
                settings.saved.remembered.get(agent_name).copied(),
                settings
                    .saved
                    .remembered_secrets
                    .get(agent_name)
                    .cloned()
                    .unwrap_or_default(),
            )
        };
        if remembered == Some(false) {
            return Ok(
                json!({"status":"denied", "automatic":true, "message":"Automatically denied by the user's remembered choice.", "btw":""}),
            );
        }
        if !secret_keys.is_empty() {
            let available = self.secret_keys().await?;
            for key in &secret_keys {
                if !available.contains(key) {
                    return Err(ApiError::bad_request(format!("unknown secret key: {key}")));
                }
            }
        }
        let mut btw = String::new();
        if remembered.is_none() || secret_keys.iter().any(|key| !approved_keys.contains(key)) {
            let prompt = Prompt {
                id: uuid::Uuid::new_v4().to_string(),
                agent_name: agent_name.into(),
                request_reason: request_reason.into(),
                secret_keys: secret_keys.clone(),
                scope: if self.mode == DaemonMode::Privileged {
                    if cfg!(windows) {
                        "当前用户、该用户管理员及 SYSTEM 命令"
                    } else {
                        "root 及普通用户命令"
                    }
                } else {
                    "当前用户命令（不含管理员 / root）"
                }
                .into(),
                expires_at_ms: Utc::now().timestamp_millis() + 60_000,
            };
            let decision: Decision = if self.mode == DaemonMode::Privileged {
                let pairing = self.pairing().await?;
                match tokio::time::timeout(
                    CONSENT_TIMEOUT,
                    bridge::call(
                        &pairing,
                        DaemonMode::User,
                        bridge::Request::Consent { prompt },
                        Some(CONSENT_TIMEOUT + Duration::from_secs(1)),
                        65_536,
                    ),
                )
                .await
                {
                    Ok(result) => result?,
                    Err(_) => Decision {
                        choice: Choice::Ignore,
                        remember: false,
                        btw: String::new(),
                    },
                }
            } else {
                self.consent.as_ref().ok_or_else(|| ApiError::unavailable("interactive authorization requires the user daemon on a graphical desktop"))?.request(prompt).await?
            };
            btw = decision.btw;
            if decision.choice == Choice::Ignore {
                tokio::time::sleep_until(deadline).await;
                return Ok(json!({"status":"timed_out", "timeout_ms":60_000, "btw":btw}));
            }
            if decision.remember {
                let mut settings = self.settings.lock().await;
                let mut next = settings.saved.clone();
                next.remembered
                    .insert(agent_name.to_owned(), decision.choice == Choice::Accept);
                next.remembered_secrets
                    .insert(agent_name.to_owned(), secret_keys.clone());
                settings.save(next).await?;
            }
            if decision.choice == Choice::Deny {
                return Ok(json!({"status":"denied", "automatic":false, "btw":btw}));
            }
        }
        // Both fresh and remembered approvals have the same truthful result.
        // No assertion is made that a person clicked for this particular call.
        let token = self
            .auth
            .lock()
            .await
            .issue_token(Some(DEFAULT_TOKEN_TTL_SECONDS))?
            .0;
        let entered = match self.enter_with_secrets(&token, &secret_keys).await {
            Ok(entered) => entered,
            Err(error) => {
                let _ = self.revoke_token(&token).await;
                return Err(error);
            }
        };
        let mut result = serde_json::to_value(entered)
            .map_err(|_| ApiError::internal("session serialization failed"))?;
        result["status"] = json!("authorized");
        result["message"] = json!("Session authorized. Use the handle for subsequent calls.");
        result["btw"] = json!(btw);
        Ok(result)
    }

    pub fn user(config: Config, auth: AuthManager) -> Result<Self, ApiError> {
        if config.bind == config.privileged_daemon {
            return Err(ApiError::bad_request(
                "user and privileged daemon addresses must differ",
            ));
        }
        let peer = Peer::new(config.privileged_daemon, config.max_output_bytes)?;
        let mut state = Self::new(config, auth);
        state.mode = DaemonMode::User;
        state.peer = Some(peer);
        Ok(state)
    }

    pub async fn enter(&self, token: &str) -> Result<EnterResult, ApiError> {
        self.enter_with_secrets(token, &[]).await
    }

    pub async fn enter_with_secrets(
        &self,
        token: &str,
        secret_keys: &[String],
    ) -> Result<EnterResult, ApiError> {
        let secret_keys = crate::secrets::normalize_keys(secret_keys)?;
        // Known local tokens (including expired/revoked ones) never fall back to
        // a different authority. A user token cannot acquire privileged access.
        if self.mode == DaemonMode::User && self.auth.lock().await.token_identity(token).is_err() {
            return self.enter_remote(token, &secret_keys).await;
        }
        self.auth.lock().await.verify_token(token)?;
        let identities = self.execution_identities().await;
        let mut environment = None;
        loop {
            // Keep authorization and insertion atomic with respect to token revocation.
            let auth = self.auth.lock().await;
            let authorization = auth.verify_token(token)?;
            let mut sessions = self.sessions.lock().await;
            if sessions.shutting_down {
                return Err(ApiError::internal("server is shutting down"));
            }
            if let Some(handle) = sessions.by_token.get(&authorization.id).cloned() {
                if sessions
                    .by_handle
                    .get(&handle)
                    .is_some_and(|session| !session.shell.is_finished())
                {
                    let existing_keys = sessions.by_handle[&handle].environment.keys();
                    if !secret_keys.is_empty() && secret_keys != existing_keys {
                        return Err(ApiError::bad_request(
                            "the live session has different secret keys; destroy it before entering with a different selection",
                        ));
                    }
                    return Ok(EnterResult {
                        handle,
                        reused: true,
                        secret_keys: existing_keys,
                        shell: ShellKind::native(),
                        platform: std::env::consts::OS,
                        daemon: self.mode,
                        sudo_available: self.mode == DaemonMode::Privileged,
                        privilege_levels: identities.keys().cloned().collect(),
                        identities,
                        expires_at: authorization.expires_at,
                        expires_at_iso: expiry_iso(authorization.expires_at),
                        message: "Reused the token's existing session.".into(),
                    });
                }
                remove_session(&mut sessions, &handle);
            }
            let Some(environment) = environment.take() else {
                // Do not hold authorization/session locks across encrypted IPC.
                // A live session keeps its snapshot even after vault rotation/deletion.
                drop(sessions);
                drop(auth);
                environment = Some(Arc::new(self.resolve_secrets(&secret_keys).await?));
                continue;
            };
            let shell = Shell::spawn_with_environment(
                &self.config.shell,
                self.config.max_output_bytes,
                &environment.0,
            )
            .await?;
            let handle = strong_handle();
            sessions
                .by_token
                .insert(authorization.id.clone(), handle.clone());
            sessions.by_handle.insert(
                handle.clone(),
                Session {
                    token_id: authorization.id,
                    expires_at: authorization.expires_at,
                    shell: Arc::new(shell),
                    environment,
                    remote_handle: None,
                    #[cfg(windows)]
                    administrator: Arc::new(tokio::sync::OnceCell::new()),
                },
            );
            return Ok(EnterResult {
                handle,
                reused: false,
                secret_keys,
                shell: ShellKind::native(),
                platform: std::env::consts::OS,
                daemon: self.mode,
                sudo_available: self.mode == DaemonMode::Privileged,
                privilege_levels: identities.keys().cloned().collect(),
                identities,
                expires_at: authorization.expires_at,
                expires_at_iso: expiry_iso(authorization.expires_at),
                message: format!(
                    "Created a new {} session. Use the handle for subsequent calls.",
                    ShellKind::native().name()
                ),
            });
        }
    }

    async fn enter_remote(
        &self,
        token: &str,
        secret_keys: &[String],
    ) -> Result<EnterResult, ApiError> {
        let peer = self.peer.as_ref().expect("user daemon has a peer");
        let entered = peer.enter(token, secret_keys).await?;
        self.register_remote(entered).await
    }

    pub(crate) async fn attach_remote(&self, handle: &str) -> Result<EnterResult, ApiError> {
        let peer = self.peer.as_ref().ok_or_else(|| {
            ApiError::forbidden("only the user daemon can attach a privileged session")
        })?;
        let inspected = peer.inspect(handle).await?;
        self.register_remote(crate::peer::Entered {
            handle: handle.into(),
            expires_at: inspected.expires_at,
            daemon: "privileged".into(),
            identities: inspected.identities,
            secret_keys: inspected.secret_keys,
        })
        .await
    }

    async fn register_remote(
        &self,
        entered: crate::peer::Entered,
    ) -> Result<EnterResult, ApiError> {
        let peer = self.peer.as_ref().expect("user daemon has a peer");
        let environment = if entered.secret_keys.is_empty() {
            Arc::new(crate::secrets::Environment::default())
        } else {
            let environment: crate::secrets::Environment = bridge::call(
                &self.pairing().await?,
                DaemonMode::Privileged,
                bridge::Request::SessionEnvironment {
                    handle: entered.handle.clone(),
                },
                Some(Duration::from_secs(5)),
                crate::secrets::ENVIRONMENT_RESPONSE_LIMIT,
            )
            .await?;
            if environment.keys() != entered.secret_keys {
                return Err(ApiError::unavailable(
                    "session environment does not match its authorized keys",
                ));
            }
            Arc::new(environment)
        };
        let key = format!("privileged:{}", entered.handle);
        let mut identities = entered.identities;
        if let Ok(identity) = crate::execution::current_identity() {
            identities.insert("current-user".into(), identity);
        }
        let mut sessions = self.sessions.lock().await;
        if sessions.shutting_down {
            drop(sessions);
            let _ = peer.destroy(&entered.handle).await;
            return Err(ApiError::internal("server is shutting down"));
        }
        let existing = sessions.by_token.get(&key).cloned();
        if existing.as_ref().is_some_and(|handle| {
            sessions
                .by_handle
                .get(handle)
                .is_none_or(|session| session.shell.is_finished())
        }) {
            return Err(ApiError::unavailable(
                "previous session is closing; enter again after it closes",
            ));
        }
        let reused = existing.is_some();
        let handle = if let Some(handle) = existing {
            handle
        } else {
            let shell = match Shell::spawn_with_environment(
                &self.config.shell,
                self.config.max_output_bytes,
                &environment.0,
            )
            .await
            {
                Ok(shell) => Arc::new(shell),
                Err(error) => {
                    drop(sessions);
                    let _ = peer.destroy(&entered.handle).await;
                    return Err(error.into());
                }
            };
            let handle = strong_handle();
            sessions.by_token.insert(key.clone(), handle.clone());
            sessions.by_handle.insert(
                handle.clone(),
                Session {
                    token_id: key,
                    expires_at: entered.expires_at,
                    shell: Arc::clone(&shell),
                    environment,
                    remote_handle: Some(entered.handle.clone()),
                    #[cfg(windows)]
                    administrator: Arc::new(tokio::sync::OnceCell::new()),
                },
            );
            // A bounded long poll propagates revocation, shutdown and loss of the
            // privileged authority to the user shell, including running commands.
            let state = self.clone();
            let watched_handle = handle.clone();
            let remote = entered.handle;
            let peer = peer.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        biased;
                        _ = shell.wait_ended() => break,
                        result = peer.validate(&remote, true) => if result.is_err() { break; },
                    }
                }
                remove_session(&mut *state.sessions.lock().await, &watched_handle);
                shell.terminate().await;
                let _ = peer.destroy(&remote).await;
            });
            handle
        };
        Ok(EnterResult {
            handle, reused, secret_keys: entered.secret_keys, shell: ShellKind::native(), platform: std::env::consts::OS,
            daemon: self.mode, sudo_available: true, expires_at: entered.expires_at,
            privilege_levels: identities.keys().cloned().collect(), identities,
            expires_at_iso: expiry_iso(entered.expires_at),
            message: "Each execution identity has separate persistent shell state. Select the identity explicitly for every command.".into(),
        })
    }

    pub(crate) async fn session_environment(
        &self,
        handle: &str,
    ) -> Result<crate::secrets::Environment, ApiError> {
        self.validate_session(handle, false).await?;
        self.sessions
            .lock()
            .await
            .by_handle
            .get(handle)
            .map(|session| (*session.environment).clone())
            .ok_or_else(|| AuthError::InvalidCredential.into())
    }

    #[cfg(any(test, not(windows)))]
    pub async fn run(
        &self,
        handle: &str,
        command: &str,
        sudo: bool,
        requested_timeout: Option<u64>,
    ) -> Result<ExecutionResult, ApiError> {
        self.run_command(
            handle,
            command,
            PrivilegeLevel::from_sudo(sudo),
            RunOptions {
                timeout_seconds: requested_timeout,
                ..RunOptions::default()
            },
        )
        .await
    }

    pub async fn run_command(
        &self,
        handle: &str,
        command: &str,
        privilege_level: PrivilegeLevel,
        options: RunOptions,
    ) -> Result<ExecutionResult, ApiError> {
        // Dropping an HTTP response must not drop cross-backend cleanup. This
        // is the lifetime of this one request, not a background-job API.
        let state = self.clone();
        let handle = handle.to_owned();
        let command = command.to_owned();
        tokio::spawn(async move {
            state
                .run_command_inner(&handle, &command, privilege_level, options)
                .await
        })
        .await
        .map_err(|_| ApiError::internal("command execution worker failed"))?
    }

    async fn run_command_inner(
        &self,
        handle: &str,
        command: &str,
        privilege_level: PrivilegeLevel,
        options: RunOptions,
    ) -> Result<ExecutionResult, ApiError> {
        let requested_timeout = options.timeout_seconds;
        if (cfg!(windows) && privilege_level == PrivilegeLevel::Root)
            || (cfg!(unix)
                && matches!(
                    privilege_level,
                    PrivilegeLevel::System | PrivilegeLevel::Administrator
                ))
        {
            return Err(ApiError::bad_request(
                "execution identity is not supported on this platform",
            ));
        }
        if requested_timeout == Some(0) {
            return Err(ApiError::bad_request(
                "timeout_seconds must be greater than zero",
            ));
        }
        if command.contains('\0') {
            return Err(ApiError::bad_request("command must not contain NUL bytes"));
        }
        if self.mode == DaemonMode::Privileged && privilege_level == PrivilegeLevel::CurrentUser {
            self.validate_session(handle, false).await?;
            let pairing = self.pairing().await?;
            let attached: serde_json::Value = bridge::call(
                &pairing,
                DaemonMode::User,
                bridge::Request::Attach {
                    handle: handle.into(),
                },
                Some(Duration::from_secs(5)),
                65_536,
            )
            .await?;
            let user_handle = attached["handle"]
                .as_str()
                .ok_or_else(|| ApiError::unavailable("missing user session handle"))?;
            let result: Result<ExecutionResult, ApiError> = bridge::call(
                &pairing,
                DaemonMode::User,
                bridge::Request::Run {
                    handle: user_handle.into(),
                    command: command.into(),
                    timeout_seconds: requested_timeout,
                    stdin: options.stdin,
                },
                None,
                self.config
                    .max_output_bytes
                    .saturating_mul(32)
                    .saturating_add(65_536),
            )
            .await;
            if result.as_ref().map_or_else(
                |error| error.status != StatusCode::BAD_REQUEST,
                |result| result.session_ended,
            ) {
                let _ = self.destroy_session(handle).await;
            }
            return result;
        }
        let (shell, remote) = {
            let mut sessions = self.sessions.lock().await;
            let expired = sessions
                .by_handle
                .get(handle)
                .and_then(|session| session.expires_at)
                .is_some_and(|expiry| Utc::now().timestamp() >= expiry);
            if expired {
                remove_session(&mut sessions, handle);
                return Err(AuthError::Expired.into());
            }
            sessions
                .by_handle
                .get(handle)
                .map(|session| (Arc::clone(&session.shell), session.remote_handle.clone()))
                .ok_or_else(|| ApiError::from(AuthError::InvalidCredential))?
        };
        if let Some(remote) = &remote {
            let peer = self.peer.as_ref().expect("remote session has a peer");
            if privilege_level.is_privileged() {
                let result = peer.run(remote, command, privilege_level, &options).await;
                if result.as_ref().map_or_else(
                    |error| error.status != StatusCode::BAD_REQUEST,
                    |result| result.session_ended,
                ) {
                    let _ = self.destroy_session(handle).await;
                }
                return result;
            }
            if let Err(error) = peer.validate(remote, false).await {
                let _ = self.destroy_session(handle).await;
                return Err(error);
            }
        } else if privilege_level.is_privileged() && self.mode == DaemonMode::User {
            return Err(ApiError::forbidden(
                "this user token authorizes current-user execution only; enter with a privileged-daemon authorization for elevated execution",
            ));
        }
        #[cfg(windows)]
        let shell = if privilege_level == PrivilegeLevel::Administrator {
            self.administrator_shell(handle, &shell).await?
        } else {
            shell
        };
        // Unit-level routing tests simulate daemon roles in one account. Real
        // processes (including --allow-unelevated) must match the selected token.
        #[cfg(not(test))]
        if !crate::execution::matches_identity(privilege_level, shell.identity()) {
            return Err(ApiError::forbidden(format!(
                "requested {} is unavailable: actual account is {}; no identity fallback was performed",
                privilege_level.name(),
                shell.identity()["user"]
            )));
        }
        let result = shell.execute_with_options(command, options).await;
        // A rejected command leaves the session healthy; any other error, or a
        // command that ended the shell, invalidates the handle.
        let ended = match &result {
            Ok(execution) => execution.session_ended,
            Err(ShellError::Rejected(_)) => false,
            Err(_) => true,
        };
        if ended {
            let _ = self.destroy_session(handle).await;
        }
        result.map_err(Into::into)
    }

    pub async fn destroy_session(&self, handle: &str) -> Result<(), ApiError> {
        let (shell, remote) = {
            let mut sessions = self.sessions.lock().await;
            let remote = sessions
                .by_handle
                .get(handle)
                .and_then(|s| s.remote_handle.clone());
            let shell = remove_session(&mut sessions, handle)
                .ok_or_else(|| ApiError::from(AuthError::InvalidCredential))?;
            (shell, remote)
        };
        shell.terminate().await;
        if let (Some(peer), Some(remote)) = (&self.peer, remote) {
            // Removal is idempotent: the monitor may already have closed it.
            if let Err(error) = peer.destroy(&remote).await
                && error.status != StatusCode::UNAUTHORIZED
            {
                return Err(error);
            }
        }
        Ok(())
    }

    pub(crate) async fn execution_identities(
        &self,
    ) -> std::collections::BTreeMap<String, serde_json::Value> {
        let mut result = std::collections::BTreeMap::new();
        if let Ok(identity) = crate::execution::current_identity() {
            let level = if self.mode == DaemonMode::User {
                PrivilegeLevel::CurrentUser
            } else {
                PrivilegeLevel::privileged()
            };
            if crate::execution::matches_identity(level, &identity) {
                result.insert(level.name().into(), identity);
            }
        }
        #[cfg(windows)]
        if self.mode == DaemonMode::Privileged
            && let Ok(context) = self.paired_user_context().await
        {
            if let Ok(identity) = serde_json::to_value(&context.identity) {
                result.insert("current-user".into(), identity);
            }
            if let Ok(identity) = localshelld_native_process::administrator_identity(&context)
                && let Ok(identity) = serde_json::to_value(identity)
            {
                result.insert("as-administrator".into(), identity);
            }
        }
        #[cfg(unix)]
        if self.mode == DaemonMode::Privileged
            && let Ok(pairing) = self.pairing().await
        {
            let response: Result<serde_json::Value, _> = bridge::call(
                &pairing,
                DaemonMode::User,
                bridge::Request::Identity,
                Some(Duration::from_secs(2)),
                65_536,
            )
            .await;
            if let Ok(identity) = response {
                result.insert("current-user".into(), identity);
            }
        }
        result
    }

    #[cfg(windows)]
    async fn paired_user_context(
        &self,
    ) -> Result<localshelld_native_process::UserContext, ApiError> {
        let pairing = self.pairing().await?;
        bridge::call(
            &pairing,
            DaemonMode::User,
            bridge::Request::UserContext,
            Some(Duration::from_secs(5)),
            4 * 1024 * 1024,
        )
        .await
    }

    #[cfg(windows)]
    async fn administrator_shell(
        &self,
        handle: &str,
        parent: &Arc<Shell>,
    ) -> Result<Arc<Shell>, ApiError> {
        let (cell, environment) = self
            .sessions
            .lock()
            .await
            .by_handle
            .get(handle)
            .map(|session| (session.administrator.clone(), session.environment.clone()))
            .ok_or(AuthError::InvalidCredential)?;
        let shell: &Arc<Shell> = cell
            .get_or_try_init(|| async {
                let context = self.paired_user_context().await?;
                let shell = Arc::new(
                    Shell::spawn_administrator_with_environment(
                        &self.config.shell,
                        self.config.max_output_bytes,
                        &context,
                        &environment.0,
                    )
                    .await
                    .map_err(|error| {
                        ApiError::unavailable(format!(
                            "cannot create the paired user's administrator shell: {error}"
                        ))
                    })?,
                );
                if parent.is_finished() {
                    shell.terminate().await;
                    return Err(ApiError::from(AuthError::InvalidCredential));
                }
                Ok::<_, ApiError>(shell)
            })
            .await?;
        if parent.is_finished() || shell.is_finished() {
            shell.terminate().await;
            return Err(ApiError::from(AuthError::InvalidCredential));
        }
        Ok(shell.clone())
    }

    pub async fn revoke_token(&self, token: &str) -> Result<(), ApiError> {
        if let Some(peer) = &self.peer
            && self.auth.lock().await.token_identity(token).is_err()
        {
            return peer.revoke(token).await;
        }
        let token_id = {
            let mut auth = self.auth.lock().await;
            let authorization = auth.token_identity(token)?;
            auth.revoke(&authorization.id)?;
            authorization.id
        };
        self.destroy_sessions_for_token(&token_id).await;
        Ok(())
    }

    async fn destroy_sessions_for_token(&self, token_id: &str) {
        let shells = {
            let mut sessions = self.sessions.lock().await;
            let handles: Vec<_> = sessions
                .by_handle
                .iter()
                .filter(|(_, session)| session.token_id == token_id)
                .map(|(handle, _)| handle.clone())
                .collect();
            handles
                .into_iter()
                .filter_map(|handle| remove_session(&mut sessions, &handle))
                .collect::<Vec<_>>()
        };
        for shell in shells {
            shell.terminate().await;
        }
    }

    pub async fn shutdown(&self) {
        let sessions = {
            let mut sessions = self.sessions.lock().await;
            sessions.shutting_down = true;
            sessions.by_token.clear();
            sessions
                .by_handle
                .drain()
                .map(|(_, session)| {
                    session.cancel();
                    session
                })
                .collect::<Vec<_>>()
        };
        for session in sessions {
            session.terminate().await;
            if let (Some(peer), Some(remote)) = (&self.peer, session.remote_handle) {
                let _ = peer.destroy(&remote).await;
            }
        }
    }

    async fn validate_session(&self, handle: &str, watch: bool) -> Result<(), ApiError> {
        if self.mode != DaemonMode::Privileged {
            return Err(ApiError::forbidden(
                "session validation is only served by the privileged daemon",
            ));
        }
        let (shell, expires) = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .by_handle
                .get(handle)
                .ok_or(AuthError::InvalidCredential)?;
            (Arc::clone(&session.shell), session.expires_at)
        };
        let remaining = expires.map(|expiry| expiry.saturating_sub(Utc::now().timestamp()));
        if remaining.is_some_and(|seconds| seconds <= 0) || shell.is_finished() {
            let _ = self.destroy_session(handle).await;
            return Err(AuthError::Expired.into());
        }
        if watch {
            let wait = remaining.unwrap_or(15).min(15) as u64;
            tokio::select! {
                _ = shell.wait_ended() => return Err(AuthError::InvalidCredential.into()),
                _ = tokio::time::sleep(Duration::from_secs(wait)) => {},
            }
            if expires.is_some_and(|expiry| expiry <= Utc::now().timestamp()) {
                let _ = self.destroy_session(handle).await;
                return Err(AuthError::Expired.into());
            }
        }
        Ok(())
    }

    pub(crate) async fn authenticate(&self, credential: &Credential) -> Result<(), ApiError> {
        self.auth.lock().await.verify_credential(credential)?;
        Ok(())
    }
}

fn remove_session(store: &mut SessionStore, handle: &str) -> Option<Session> {
    let session = store.by_handle.remove(handle)?;
    if store
        .by_token
        .get(&session.token_id)
        .is_some_and(|mapped| mapped == handle)
    {
        store.by_token.remove(&session.token_id);
    }
    session.cancel();
    Some(session)
}

fn expiry_iso(expiry: Option<i64>) -> Option<String> {
    expiry
        .and_then(|value| chrono::DateTime::from_timestamp(value, 0))
        .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

#[cfg(test)]
mod dual_daemon_tests;
#[cfg(test)]
mod interactive_tests;

pub(crate) fn strong_handle() -> String {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Deserialize)]
struct TokenBody {
    token: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnterBody {
    token: String,
    #[serde(default)]
    secret_keys: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunBody {
    handle: String,
    command: String,
    #[cfg(not(windows))]
    sudo: bool,
    #[cfg(windows)]
    privilege_level: PrivilegeLevel,
    #[serde(default)]
    stdin: String,
    timeout_seconds: Option<u64>,
}

#[derive(Deserialize)]
struct HandleBody {
    handle: String,
}

#[derive(Deserialize)]
struct IssueBody {
    credential: Option<Credential>,
    ttl_seconds: Option<u64>,
    #[serde(default)]
    permanent: bool,
}

#[derive(Serialize)]
struct IssueResponse {
    token: String,
    record: TokenRecord,
    warning: &'static str,
}

#[derive(Deserialize)]
struct AdminListBody {
    credential: Option<Credential>,
}

#[derive(Deserialize)]
struct AdminRevokeBody {
    credential: Option<Credential>,
    id: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(crate::management::routes())
        .route("/", get(crate::assets::serve))
        .route("/health", get(health))
        // AEAD/base64 expands a valid public JSON request beyond 2 MiB.
        .route(
            "/v1/bridge",
            post(bridge::handle).layer(axum::extract::DefaultBodyLimit::max(4 * 1024 * 1024)),
        )
        .route("/v1/sessions/enter", post(enter))
        .route("/v1/secrets/keys", get(secret_keys))
        .route("/v1/commands/run", post(run))
        .route("/v1/sessions/destroy", post(destroy))
        .route("/v1/sessions/validate", post(validate))
        .route("/v1/sessions/watch", post(watch))
        .route("/v1/tokens/revoke", post(revoke))
        .route("/v1/admin/tokens/issue", post(issue))
        .route("/v1/admin/tokens/list", post(list_tokens))
        .route("/v1/admin/tokens/revoke", post(admin_revoke))
        .route("/mcp", post(mcp::handle))
        .fallback(crate::assets::serve)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            browser_boundary,
        ))
        .with_state(state)
}

async fn browser_boundary(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<Response, ApiError> {
    fn allowed_authority(value: &str, port: u16) -> bool {
        let Ok(authority) = value.parse::<axum::http::uri::Authority>() else {
            return false;
        };
        matches!(authority.host(), "localhost" | "127.0.0.1" | "[::1]")
            && authority.port_u16() == Some(port)
    }
    if let Some(host) = request.headers().get("host")
        && !host
            .to_str()
            .is_ok_and(|host| allowed_authority(host, state.config.bind.port()))
    {
        return Err(ApiError::forbidden(
            "Host must be the daemon's loopback address and port",
        ));
    }
    if let Some(origin) = request.headers().get("origin") {
        let valid = origin
            .to_str()
            .ok()
            .and_then(|origin| origin.parse::<axum::http::Uri>().ok())
            .is_some_and(|uri| {
                uri.scheme_str() == Some("http")
                    && uri.authority().is_some_and(|authority| {
                        allowed_authority(authority.as_str(), state.config.bind.port())
                    })
            });
        if !valid {
            return Err(ApiError::forbidden(
                "cross-origin browser requests are not allowed",
            ));
        }
    }
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        "cache-control",
        axum::http::HeaderValue::from_static("no-store"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        axum::http::HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

async fn health(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<serde_json::Value> {
    Json(
        serde_json::json!({ "status": "ok", "service": "localshelld", "daemon": state.mode, "version": crate::VERSION, "commit": crate::COMMIT }),
    )
}

async fn enter(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(body): Json<EnterBody>,
) -> Result<Json<EnterResult>, ApiError> {
    Ok(Json(
        state
            .enter_with_secrets(&body.token, &body.secret_keys)
            .await?,
    ))
}

async fn secret_keys(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(serde_json::json!({"keys":state.secret_keys().await?})))
}

async fn run(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(body): Json<RunBody>,
) -> Result<Json<ExecutionResult>, ApiError> {
    Ok(Json(
        state
            .run_command(
                &body.handle,
                &body.command,
                {
                    #[cfg(windows)]
                    {
                        body.privilege_level
                    }
                    #[cfg(not(windows))]
                    {
                        PrivilegeLevel::from_sudo(body.sudo)
                    }
                },
                RunOptions {
                    stdin: body.stdin,
                    timeout_seconds: body.timeout_seconds,
                },
            )
            .await?,
    ))
}

async fn validate(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(body): Json<HandleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.validate_session(&body.handle, false).await?;
    let (expires_at, secret_keys) = state
        .sessions
        .lock()
        .await
        .by_handle
        .get(&body.handle)
        .ok_or(AuthError::InvalidCredential)?
        .metadata();
    Ok(Json(
        serde_json::json!({"valid": true, "expires_at": expires_at, "secret_keys":secret_keys, "identities":state.execution_identities().await}),
    ))
}

async fn watch(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(body): Json<HandleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.validate_session(&body.handle, true).await?;
    Ok(Json(serde_json::json!({"valid": true})))
}

async fn destroy(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(body): Json<HandleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.destroy_session(&body.handle).await?;
    Ok(Json(serde_json::json!({ "destroyed": true })))
}

async fn revoke(
    axum::extract::State(state): axum::extract::State<AppState>,
    Json(body): Json<TokenBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.revoke_token(&body.token).await?;
    Ok(Json(serde_json::json!({ "revoked": true })))
}

async fn issue(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
    Json(body): Json<IssueBody>,
) -> Result<Json<IssueResponse>, ApiError> {
    crate::management::authorize(&state, &headers, body.credential.as_ref()).await?;
    let ttl = if body.permanent {
        None
    } else {
        Some(body.ttl_seconds.unwrap_or(DEFAULT_TOKEN_TTL_SECONDS))
    };
    if ttl == Some(0) {
        return Err(ApiError::bad_request(
            "ttl_seconds must be greater than zero",
        ));
    }
    let (token, record) = state.auth.lock().await.issue_token(ttl)?;
    Ok(Json(IssueResponse {
        token,
        record,
        warning: if state.mode == DaemonMode::User {
            if cfg!(windows) {
                "User-authorized current-user execution only. Valid until expiry, revocation or user daemon restart."
            } else {
                "User-authorized current-user execution (sudo=false). Valid until expiry, revocation or user daemon restart."
            }
        } else {
            if cfg!(windows) {
                "User-authorized current-user, as-administrator and SYSTEM execution where available. Valid until expiry, revocation or privileged daemon restart."
            } else {
                "User-authorized administrator/root execution. Valid until expiry, revocation or privileged daemon restart."
            }
        },
    }))
}

async fn list_tokens(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AdminListBody>,
) -> Result<Json<Vec<TokenRecord>>, ApiError> {
    crate::management::authorize(&state, &headers, body.credential.as_ref()).await?;
    let records = state.auth.lock().await.list();
    Ok(Json(records))
}

async fn admin_revoke(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
    Json(body): Json<AdminRevokeBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::management::authorize(&state, &headers, body.credential.as_ref()).await?;
    state.auth.lock().await.revoke(&body.id)?;
    state.destroy_sessions_for_token(&body.id).await;
    Ok(Json(serde_json::json!({ "revoked": true })))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;
    use crate::auth::hash_password;

    pub(super) async fn request(app: &Router, path: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::post(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, body)
    }

    fn test_app() -> Router {
        let config = Config {
            password_hash: hash_password(b"test master password").unwrap(),
            ..Config::default()
        };
        let auth = AuthManager::new(config.password_hash.clone(), None);
        router(AppState::new(config, auth))
    }

    #[tokio::test]
    async fn health_exposes_the_embedded_release_identity() {
        let response = test_app()
            .oneshot(Request::get("/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["version"], crate::VERSION);
        assert_eq!(body["commit"], crate::COMMIT);
    }

    #[tokio::test]
    async fn full_http_lifecycle_reuses_session_and_revokes_access() {
        let app = test_app();
        let credential = json!({ "type": "password", "value": "test master password" });
        let (status, issued) = request(
            &app,
            "/v1/admin/tokens/issue",
            json!({ "credential": credential, "ttl_seconds": 120 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let token = issued["token"].as_str().unwrap();
        assert_eq!(token.len(), 22);
        assert!(token.bytes().all(|byte| byte.is_ascii_alphanumeric()));

        let (_, first) = request(&app, "/v1/sessions/enter", json!({ "token": token })).await;
        let (_, second) = request(&app, "/v1/sessions/enter", json!({ "token": token })).await;
        assert_eq!(first["handle"], second["handle"]);
        assert_eq!(first["reused"], false);
        assert_eq!(second["reused"], true);
        assert_eq!(
            first["shell"],
            serde_json::to_value(ShellKind::native()).unwrap()
        );
        assert_eq!(first["platform"], std::env::consts::OS);
        assert!(first["handle"].as_str().unwrap().len() >= 40);

        let handle = first["handle"].as_str().unwrap();
        let mut command = crate::execution::execution_argument(PrivilegeLevel::privileged());
        command["handle"] = json!(handle);
        command["command"] = json!(if cfg!(windows) {
            "$global:httpState=40+2; $global:httpState"
        } else {
            "httpState=$((40+2)); echo $httpState"
        });
        let (status, result) = request(&app, "/v1/commands/run", command.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert!(result["output"].as_str().unwrap().contains("42"));

        let (status, _) = request(&app, "/v1/tokens/revoke", json!({ "token": token })).await;
        assert_eq!(status, StatusCode::OK);
        command["command"] = json!("echo should-not-run");
        let (status, _) = request(&app, "/v1/commands/run", command).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = request(&app, "/v1/sessions/enter", json!({ "token": token })).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn disconnected_run_still_performs_timeout_cleanup() {
        let mut auth = AuthManager::new("unused".into(), None);
        let (token, _) = auth.issue_token(None).unwrap();
        let state = AppState::new(Config::default(), auth);
        let entered = state.enter(&token).await.unwrap();
        state
            .run(&entered.handle, "echo ready", true, Some(10))
            .await
            .unwrap();
        let request = {
            let state = state.clone();
            let handle = entered.handle.clone();
            tokio::spawn(async move {
                state
                    .run(
                        &handle,
                        if cfg!(windows) {
                            "Start-Sleep -Seconds 30"
                        } else {
                            "sleep 30"
                        },
                        true,
                        Some(1),
                    )
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        request.abort();
        tokio::time::timeout(Duration::from_secs(5), async {
            while state
                .sessions
                .lock()
                .await
                .by_handle
                .contains_key(&entered.handle)
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("a disconnected request must still remove the entire timed-out handle");
        state.shutdown().await;
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_http_rejects_legacy_or_unsupported_privilege_arguments() {
        let app = test_app();
        for arguments in [
            json!({"sudo":true}),
            json!({"sudo":false,"privilege_level":"current-user"}),
            json!({"privilege_level":"root"}),
            json!({"privilege_level":true}),
            json!({}),
        ] {
            let mut body = arguments;
            body["handle"] = json!("invalid");
            body["command"] = json!("echo should-not-run");
            let (status, _) = request(&app, "/v1/commands/run", body).await;
            assert!(matches!(
                status,
                StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY
            ));
        }
    }

    #[tokio::test]
    async fn rejects_invalid_timeout_and_credentials() {
        let app = test_app();
        let (status, _) = request(
            &app,
            "/v1/admin/tokens/issue",
            json!({
                "credential": { "type": "password", "value": "wrong password" },
                "ttl_seconds": 0
            }),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_lists_and_revokes_tokens_by_id() {
        let app = test_app();
        let credential = json!({ "type": "password", "value": "test master password" });
        let (status, issued) = request(
            &app,
            "/v1/admin/tokens/issue",
            json!({ "credential": credential.clone(), "ttl_seconds": 120 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let token = issued["token"].as_str().unwrap();
        let id = issued["record"]["id"].as_str().unwrap();

        let (status, records) = request(
            &app,
            "/v1/admin/tokens/list",
            json!({ "credential": credential.clone() }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(records[0]["id"], id);
        assert!(records[0].get("token").is_none());

        let (status, _) = request(
            &app,
            "/v1/admin/tokens/revoke",
            json!({ "credential": credential, "id": id }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = request(&app, "/v1/sessions/enter", json!({ "token": token })).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn destroy_revoke_and_shutdown_interrupt_unlimited_commands() {
        for action in ["destroy", "revoke", "shutdown"] {
            let config = Config::default();
            let mut auth = AuthManager::new("unused".into(), None);
            let (token, _) = auth.issue_token(None).unwrap();
            let state = AppState::new(config, auth);
            let entered = state.enter(&token).await.unwrap();
            state
                .run(&entered.handle, "echo ready", true, Some(3600))
                .await
                .unwrap();
            assert!(
                state
                    .run(&entered.handle, "echo invalid", true, Some(0))
                    .await
                    .is_err()
            );
            let executing = {
                let state = state.clone();
                let handle = entered.handle.clone();
                tokio::spawn(async move {
                    state
                        .run(
                            &handle,
                            if cfg!(windows) {
                                "while ($true) { Start-Sleep -Milliseconds 100 }"
                            } else {
                                "while :; do :; done"
                            },
                            true,
                            None,
                        )
                        .await
                })
            };
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                match action {
                    "destroy" => state.destroy_session(&entered.handle).await.unwrap(),
                    "revoke" => state.revoke_token(&token).await.unwrap(),
                    _ => state.shutdown().await,
                }
            })
            .await
            .expect("cancellation waited for unlimited command");
            assert!(executing.await.unwrap().unwrap().session_ended);
            assert!(
                state
                    .run(&entered.handle, "echo never", true, None)
                    .await
                    .is_err()
            );
            if action == "destroy" {
                let recreated = state.enter(&token).await.unwrap();
                assert!(!recreated.reused);
                assert_ne!(recreated.handle, entered.handle);
                state.shutdown().await;
            } else {
                assert!(state.enter(&token).await.is_err());
            }
        }
    }

    #[tokio::test]
    async fn enter_replaces_session_that_ended_after_caller_disconnected() {
        let mut auth = AuthManager::new("unused".into(), None);
        let (token, _) = auth.issue_token(None).unwrap();
        let state = AppState::new(Config::default(), auth);
        let first = state.enter(&token).await.unwrap();
        // Simulate a worker ending after its HTTP caller was dropped, so the
        // request handler cannot remove the session mapping itself.
        let shell = Arc::clone(&state.sessions.lock().await.by_handle[&first.handle].shell);
        shell.terminate().await;
        let second = state.enter(&token).await.unwrap();
        assert!(!second.reused);
        assert_ne!(first.handle, second.handle);
        state.shutdown().await;
    }
}
