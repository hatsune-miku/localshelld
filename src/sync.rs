//! Cloud authority is opt-in. Every read checks the server and every mutation
//! commits a complete snapshot with CAS before replacing the encrypted cache.
//! A single mutex serializes refresh-token rotation and local vault operations.
use std::{collections::BTreeMap, time::Duration};

use chrono::Utc;
use localshelld_sync_protocol::{
    CredentialFile, DeviceStart, MAX_BODY_BYTES, Replace, Snapshot, Tokens, validate_credentials,
};
use reqwest::{Client, Method};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::{
    bridge, config,
    server::{ApiError, AppState, DaemonMode},
    settings::Settings,
};

#[derive(Clone, Serialize, Deserialize)]
pub struct SavedSync {
    pub url: String,
    pub tokens: config::SealedSecret,
    pub revision: u64,
}

struct PendingConnection {
    url: String,
    device: DeviceStart,
    expires_at: i64,
    // If the cloud exchange succeeds but the first fetch/write fails, keep
    // the one-use result in memory so that retry does not consume it twice.
    tokens: Option<Tokens>,
}

#[derive(Default)]
pub struct Runtime {
    pending: Option<PendingConnection>,
    last_error: Option<String>,
    last_sync: Option<i64>,
    worker: Option<tokio::task::AbortHandle>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Status,
    Start { url: String, name: String },
    Poll,
    Disconnect,
    Refresh,
}

pub enum Change<'a> {
    Put(&'a str, &'a str),
    Remove(&'a str),
    Replace(&'a CredentialFile),
}

pub fn normalize_url(value: &str) -> Result<String, ApiError> {
    let url = reqwest::Url::parse(value.trim())
        .map_err(|_| ApiError::bad_request("invalid sync service URL"))?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https"
            || (url.scheme() == "http" && url.host_str() == Some("localhost")))
    {
        return Err(ApiError::bad_request(
            "use an HTTPS origin without credentials, path, query or fragment; http://localhost is allowed for development",
        ));
    }
    Ok(url.origin().ascii_serialization())
}

fn client() -> Result<Client, ApiError> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| ApiError::internal("sync HTTP client initialization failed"))
}

async fn request<T: DeserializeOwned>(
    method: Method,
    url: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
    timeout: Option<Duration>,
) -> Result<T, ApiError> {
    let mut request = client()?.request(method, format!("{url}/sync/v1/{path}"));
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    if let Some(timeout) = timeout {
        request = request.timeout(timeout);
    }
    let mut response = request
        .send()
        .await
        .map_err(|_| ApiError::unavailable("同步服务不可用，凭据未离线修改。"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(ApiError::with_status(
            status.as_u16(),
            match status.as_u16() {
                401 | 403 => "同步登录已失效，请重新连接服务。",
                409 => "云端凭据已变化，请同步后重试。",
                429 => "同步请求过于频繁，请稍后重试。",
                _ => "同步服务拒绝请求，请检查连接后重试。",
            }
            .into(),
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        return Err(ApiError::unavailable(
            "sync response exceeds the size limit",
        ));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ApiError::unavailable("sync response interrupted"))?
    {
        if bytes.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(ApiError::unavailable(
                "sync response exceeds the size limit",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::unavailable("invalid sync service response"))
}

fn read_local(settings: &Settings) -> Result<Snapshot, ApiError> {
    let mut snapshot = Snapshot {
        revision: settings.saved.sync.as_ref().map_or(0, |sync| sync.revision),
        credentials: BTreeMap::new(),
    };
    for (key, sealed) in &settings.saved.secrets {
        let value = config::unseal(sealed, &settings.seal_key)
            .map_err(|_| ApiError::internal("credential decryption failed"))?;
        let value = std::str::from_utf8(&value)
            .map_err(|_| ApiError::internal("invalid credential encoding"))?;
        snapshot.credentials.insert(key.clone(), value.into());
    }
    Ok(snapshot)
}

fn encrypted_vault(
    settings: &Settings,
    snapshot: &Snapshot,
) -> Result<BTreeMap<String, config::SealedSecret>, ApiError> {
    validate_credentials(&snapshot.credentials).map_err(ApiError::bad_request)?;
    snapshot
        .credentials
        .iter()
        .map(|(key, value)| {
            Ok((
                key.clone(),
                config::seal(value.as_bytes(), &settings.seal_key)
                    .map_err(|_| ApiError::internal("credential encryption failed"))?,
            ))
        })
        .collect()
}

impl AppState {
    // Caller holds self.sync across all helpers suffixed _locked.
    async fn tokens_locked(&self) -> Result<Option<(SavedSync, Tokens)>, ApiError> {
        let settings = self.settings.lock().await;
        let Some(saved) = settings.saved.sync.clone() else {
            return Ok(None);
        };
        let bytes = config::unseal(&saved.tokens, &settings.seal_key)
            .map_err(|_| ApiError::internal("sync token decryption failed"))?;
        let tokens: Tokens = serde_json::from_slice(&bytes)
            .map_err(|_| ApiError::internal("invalid sync tokens"))?;
        drop(settings);
        if tokens.expires_at > Utc::now().timestamp() + 40 {
            return Ok(Some((saved, tokens)));
        }
        let next: Tokens = request(
            Method::POST,
            &saved.url,
            "session/refresh",
            None,
            Some(&json!({"refresh_token":tokens.refresh_token})),
            None,
        )
        .await?;
        let mut settings = self.settings.lock().await;
        let mut state = settings.saved.clone();
        let sync = state
            .sync
            .as_mut()
            .ok_or_else(|| ApiError::internal("sync configuration disappeared"))?;
        sync.tokens = config::seal(
            &Zeroizing::new(
                serde_json::to_vec(&next)
                    .map_err(|_| ApiError::internal("token encoding failed"))?,
            ),
            &settings.seal_key,
        )
        .map_err(|_| ApiError::internal("token encryption failed"))?;
        let result = sync.clone();
        settings.save(state).await?;
        Ok(Some((result, next)))
    }

    async fn save_snapshot_locked(&self, snapshot: &Snapshot) -> Result<(), ApiError> {
        let mut settings = self.settings.lock().await;
        validate_credentials(&snapshot.credentials).map_err(ApiError::bad_request)?;
        let old = read_local(&settings)?;
        if old.revision == snapshot.revision && old.credentials == snapshot.credentials {
            return Ok(());
        }
        let mut next = settings.saved.clone();
        next.secrets = encrypted_vault(&settings, snapshot)?;
        if let Some(sync) = &mut next.sync {
            sync.revision = snapshot.revision;
        }
        settings.save(next).await
    }

    async fn read_vault_locked(&self) -> Result<Snapshot, ApiError> {
        let Some((saved, tokens)) = self.tokens_locked().await? else {
            return read_local(&*self.settings.lock().await);
        };
        let snapshot: Snapshot = request(
            Method::GET,
            &saved.url,
            "vault",
            Some(&tokens.access_token),
            None,
            None,
        )
        .await?;
        self.save_snapshot_locked(&snapshot).await?;
        Ok(snapshot)
    }

    pub(crate) async fn read_vault(&self) -> Result<Snapshot, ApiError> {
        let mut runtime = self.sync.lock().await;
        let result = self.read_vault_locked().await;
        report(&mut runtime, &result);
        result
    }

    pub(crate) async fn change_vault(&self, change: Change<'_>) -> Result<(), ApiError> {
        let mut runtime = self.sync.lock().await;
        let result = async {
            let mut snapshot = self.read_vault_locked().await?;
            match change {
                Change::Put(key, value) => {
                    snapshot.credentials.insert(key.into(), value.into());
                }
                Change::Remove(key) => {
                    snapshot.credentials.remove(key);
                }
                Change::Replace(file) => {
                    file.validate().map_err(ApiError::bad_request)?;
                    snapshot.credentials = file.credentials.clone();
                }
            }
            validate_credentials(&snapshot.credentials).map_err(ApiError::bad_request)?;
            if let Some((saved, tokens)) = self.tokens_locked().await? {
                let body = Replace {
                    expected_revision: snapshot.revision,
                    credentials: snapshot.credentials.clone(),
                };
                let result = request::<Snapshot>(
                    Method::PUT,
                    &saved.url,
                    "vault",
                    Some(&tokens.access_token),
                    Some(&json!(body)),
                    None,
                )
                .await;
                match result {
                    Ok(accepted) => {
                        // Even when a follow-up fetch fails, retain the server's
                        // acknowledged snapshot; never install an unacknowledged edit.
                        self.save_snapshot_locked(&accepted).await.map_err(|_| {
                            ApiError::unavailable(
                                "云端已保存，但本机缓存写入失败。请修复本机存储后重新同步。",
                            )
                        })?;
                        self.read_vault_locked().await.map_err(|_| {
                            ApiError::unavailable(
                                "云端已保存，但后续同步失败。请恢复连接后重新同步。",
                            )
                        })?;
                    }
                    Err(error) => {
                        // A timeout can happen after a successful server commit.
                        // Reconcile from the authority; never replay blindly.
                        let _ = self.read_vault_locked().await;
                        return Err(error);
                    }
                }
            } else {
                self.save_snapshot_locked(&snapshot).await?;
            }
            Ok(())
        }
        .await;
        report(&mut runtime, &result);
        result
    }

    pub(crate) async fn export_secrets(&self) -> Result<CredentialFile, ApiError> {
        if let Some(pairing) = self.vault_peer().await {
            return bridge::call(
                &pairing,
                DaemonMode::User,
                bridge::Request::SecretsExport,
                Some(Duration::from_secs(45)),
                crate::secrets::ENVIRONMENT_RESPONSE_LIMIT,
            )
            .await;
        }
        let snapshot = self.read_vault().await?;
        Ok(CredentialFile::new(snapshot.credentials.clone()))
    }

    pub(crate) async fn import_secrets(&self, file: CredentialFile) -> Result<(), ApiError> {
        file.validate().map_err(ApiError::bad_request)?;
        if let Some(pairing) = self.vault_peer().await {
            return bridge::call(
                &pairing,
                DaemonMode::User,
                bridge::Request::SecretsImport { file },
                Some(Duration::from_secs(75)),
                65_536,
            )
            .await;
        }
        self.change_vault(Change::Replace(&file)).await
    }

    pub(crate) async fn sync_action(&self, action: Action) -> Result<Value, ApiError> {
        if let Some(pairing) = self.vault_peer().await {
            return bridge::call(
                &pairing,
                DaemonMode::User,
                bridge::Request::Sync { action },
                Some(Duration::from_secs(60)),
                65_536,
            )
            .await;
        }
        let mut runtime = self.sync.lock().await;
        match action {
            Action::Status => {
                let settings = self.settings.lock().await;
                Ok(
                    json!({"url":settings.saved.sync.as_ref().map(|sync| &sync.url),"revision":settings.saved.sync.as_ref().map(|sync|sync.revision),
                    "last_sync":runtime.last_sync,"error":runtime.last_error}),
                )
            }
            Action::Start { url, name } => {
                let url = normalize_url(&url)?;
                let info: Value = request(Method::GET, &url, "info", None, None, None).await?;
                if info["protocol"] != 1 {
                    return Err(ApiError::bad_request("unsupported sync protocol"));
                }
                let device: DeviceStart = request(
                    Method::POST,
                    &url,
                    "device/start",
                    None,
                    Some(&json!({"name":name})),
                    None,
                )
                .await?;
                // Do not open an arbitrary remote-supplied URL with a token.
                let verification = reqwest::Url::parse(&device.verification_uri)
                    .map_err(|_| ApiError::bad_request("invalid device verification URL"))?;
                if verification.origin().ascii_serialization() != url
                    || device.expires_in > 900
                    || device.user_code.len() > 80
                {
                    return Err(ApiError::bad_request(
                        "invalid device authorization response",
                    ));
                }
                let response = json!({"verification_uri":device.verification_uri,"user_code":device.user_code,"expires_in":device.expires_in,"interval":device.interval.max(3)});
                runtime.pending = Some(PendingConnection {
                    url,
                    expires_at: Utc::now().timestamp() + device.expires_in as i64,
                    device,
                    tokens: None,
                });
                Ok(response)
            }
            Action::Poll => {
                let pending = runtime
                    .pending
                    .as_mut()
                    .ok_or_else(|| ApiError::bad_request("start a sync connection first"))?;
                if pending.expires_at <= Utc::now().timestamp() {
                    runtime.pending = None;
                    return Err(ApiError::bad_request("device authorization expired"));
                }
                if pending.tokens.is_none() {
                    let value: Value = request(
                        Method::POST,
                        &pending.url,
                        "device/poll",
                        None,
                        Some(&json!({"device_code":pending.device.device_code})),
                        None,
                    )
                    .await?;
                    if value["pending"] == true {
                        return Ok(json!({"pending":true}));
                    }
                    pending.tokens =
                        Some(serde_json::from_value(value).map_err(|_| {
                            ApiError::unavailable("invalid device session response")
                        })?);
                }
                let tokens = pending.tokens.as_ref().expect("assigned above");
                let snapshot: Snapshot = request(
                    Method::GET,
                    &pending.url,
                    "vault",
                    Some(&tokens.access_token),
                    None,
                    None,
                )
                .await?;
                let mut settings = self.settings.lock().await;
                let mut next = settings.saved.clone();
                next.secrets = encrypted_vault(&settings, &snapshot)?;
                let sealed = config::seal(
                    &Zeroizing::new(
                        serde_json::to_vec(tokens)
                            .map_err(|_| ApiError::internal("token encoding failed"))?,
                    ),
                    &settings.seal_key,
                )
                .map_err(|_| ApiError::internal("token encryption failed"))?;
                next.sync = Some(SavedSync {
                    url: pending.url.clone(),
                    tokens: sealed,
                    revision: snapshot.revision,
                });
                settings.save(next).await?;
                runtime.pending = None;
                runtime.last_error = None;
                runtime.last_sync = Some(Utc::now().timestamp());
                Ok(json!({"connected":true}))
            }
            Action::Disconnect => {
                // Retain the last confirmed cache for explicit offline use.
                // Revocation is best effort; the server's device list can revoke
                // this session later when the connection is unavailable.
                if let Ok(Some((saved, tokens))) = self.tokens_locked().await {
                    let _ = request::<Value>(
                        Method::POST,
                        &saved.url,
                        "session/revoke",
                        Some(&tokens.access_token),
                        Some(&json!({"id":""})),
                        None,
                    )
                    .await;
                }
                let mut settings = self.settings.lock().await;
                let mut next = settings.saved.clone();
                next.sync = None;
                settings.save(next).await?;
                runtime.pending = None;
                runtime.last_error = None;
                runtime.last_sync = None;
                Ok(json!({"offline":true}))
            }
            Action::Refresh => {
                let result = self.read_vault_locked().await;
                report(&mut runtime, &result);
                result.map(|snapshot| json!({"revision":snapshot.revision}))
            }
        }
    }

    pub async fn start_sync_worker(&self) {
        let mut runtime = self.sync.lock().await;
        if runtime.worker.is_some() {
            return;
        }
        let state = self.clone();
        let worker = tokio::spawn(async move {
            loop {
                if state.vault_peer().await.is_some() {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
                let connection = {
                    let mut runtime = state.sync.lock().await;
                    let result = state.tokens_locked().await;
                    if result.is_err() {
                        report(&mut runtime, &result);
                    }
                    result.ok().flatten()
                };
                let Some((saved, tokens)) = connection else {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                };
                // Never hold the mutation/refresh lock during a 25-second wait.
                let result: Result<Value, _> = request(
                    Method::GET,
                    &saved.url,
                    &format!("events?after={}", saved.revision),
                    Some(&tokens.access_token),
                    None,
                    Some(Duration::from_secs(30)),
                )
                .await;
                let mut runtime = state.sync.lock().await;
                if state
                    .settings
                    .lock()
                    .await
                    .saved
                    .sync
                    .as_ref()
                    .map(|s| s.url.as_str())
                    != Some(saved.url.as_str())
                {
                    continue;
                }
                match result {
                    Ok(event) => {
                        // Also fetch after each heartbeat: this validates persisted
                        // caches after restart and handles restored server snapshots.
                        let result = state.read_vault_locked().await;
                        report(&mut runtime, &result);
                        let _ = event;
                    }
                    Err(error) => {
                        runtime.last_error = Some(error.to_string());
                    }
                }
                let retry = runtime.last_error.is_some();
                drop(runtime);
                if retry {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        });
        runtime.worker = Some(worker.abort_handle());
    }

    pub(crate) async fn stop_sync_worker(&self) {
        if let Some(worker) = self.sync.lock().await.worker.take() {
            worker.abort();
        }
    }
}

fn report<T>(runtime: &mut Runtime, result: &Result<T, ApiError>) {
    match result {
        Ok(_) => {
            runtime.last_sync = Some(Utc::now().timestamp());
            runtime.last_error = None;
        }
        Err(error) => {
            runtime.last_error = Some(error.to_string());
        }
    }
}

#[cfg(test)]
mod tests;
