//! Mutually authenticated loopback IPC for a deliberately paired desktop daemon.
//! A port number alone is not an identity: every message is AEAD protected, is
//! single-use, has a bounded age, and is bound to its direction and request ID.
use std::{collections::HashMap, sync::Arc, time::Duration};

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use axum::{Json, extract::State};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{
    consent::Prompt,
    server::{ApiError, AppState, DaemonMode},
    settings::Pairing,
};

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Ping,
    Identity,
    #[cfg(windows)]
    UserContext,
    Consent {
        prompt: Prompt,
    },
    EnterInteractive {
        agent_name: String,
        request_reason: String,
        #[serde(default)]
        secret_keys: Vec<String>,
    },
    SecretsList,
    SecretPut {
        key: String,
        value: String,
    },
    SecretRemove {
        key: String,
    },
    SecretsResolve {
        keys: Vec<String>,
    },
    SessionEnvironment {
        handle: String,
    },
    Attach {
        handle: String,
    },
    Run {
        handle: String,
        command: String,
        timeout_seconds: Option<u64>,
        #[serde(default)]
        stdin: String,
    },
}

#[derive(Serialize, Deserialize)]
pub struct Envelope {
    nonce: String,
    ciphertext: String,
}

#[derive(Serialize, Deserialize)]
struct Message {
    id: String,
    issued_at_ms: i64,
    payload: Value,
}

#[derive(Serialize, Deserialize)]
struct Reply {
    request_id: String,
    result: Result<Value, Failure>,
}

#[derive(Serialize, Deserialize)]
struct Failure {
    status: u16,
    message: String,
}

#[derive(Clone, Default)]
pub struct ReplayGuard(Arc<Mutex<HashMap<String, i64>>>);

impl ReplayGuard {
    async fn check(&self, message: &Message) -> Result<(), ApiError> {
        let now = chrono::Utc::now().timestamp_millis();
        if message.issued_at_ms.abs_diff(now) > 90_000 {
            return Err(ApiError::forbidden("expired bridge request"));
        }
        let mut seen = self.0.lock().await;
        seen.retain(|_, expiry| *expiry > now);
        if seen.contains_key(&message.id) || seen.len() >= 4096 {
            return Err(ApiError::forbidden(
                "replayed bridge request or bridge rate limit",
            ));
        }
        seen.insert(message.id.clone(), now + 180_001);
        Ok(())
    }
}

pub fn generate_key() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn cipher(key: &str) -> Result<Aes256Gcm, ApiError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(key)
        .map_err(|_| ApiError::bad_request("invalid pairing code"))?;
    Aes256Gcm::new_from_slice(&bytes)
        .map_err(|_| ApiError::bad_request("pairing code must contain 32 random bytes"))
}

pub fn validate_key(key: &str) -> Result<(), ApiError> {
    cipher(key).map(|_| ())
}

fn context(destination: DaemonMode, response: bool) -> String {
    format!(
        "localshelld/bridge/v1/{destination:?}/{}",
        if response { "response" } else { "request" }
    )
}

fn seal<T: Serialize>(key: &str, aad: &str, value: &T) -> Result<Envelope, ApiError> {
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let bytes =
        serde_json::to_vec(value).map_err(|_| ApiError::internal("bridge serialization failed"))?;
    let encrypted = cipher(key)?
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &bytes,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| ApiError::internal("bridge encryption failed"))?;
    Ok(Envelope {
        nonce: URL_SAFE_NO_PAD.encode(nonce),
        ciphertext: URL_SAFE_NO_PAD.encode(encrypted),
    })
}

fn open<T: DeserializeOwned>(key: &str, aad: &str, envelope: Envelope) -> Result<T, ApiError> {
    let nonce: [u8; 12] = URL_SAFE_NO_PAD
        .decode(envelope.nonce)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| ApiError::forbidden("invalid bridge envelope"))?;
    let encrypted = URL_SAFE_NO_PAD
        .decode(envelope.ciphertext)
        .map_err(|_| ApiError::forbidden("invalid bridge envelope"))?;
    let bytes = cipher(key)?
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &encrypted,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| ApiError::forbidden("bridge authentication failed; pair the daemons again"))?;
    serde_json::from_slice(&bytes).map_err(|_| ApiError::bad_request("invalid bridge message"))
}

pub async fn call<T: DeserializeOwned>(
    pairing: &Pairing,
    destination: DaemonMode,
    request: Request,
    timeout: Option<Duration>,
    response_limit: usize,
) -> Result<T, ApiError> {
    if !pairing.peer.ip().is_loopback() {
        return Err(ApiError::bad_request("bridge peer must be on loopback"));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let message = Message {
        id: id.clone(),
        issued_at_ms: chrono::Utc::now().timestamp_millis(),
        payload: serde_json::to_value(request)
            .map_err(|_| ApiError::internal("bridge serialization failed"))?,
    };
    let envelope = seal(&pairing.key, &context(destination, false), &message)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .build()
        .map_err(|_| ApiError::internal("bridge client failed"))?;
    let mut builder = client
        .post(format!("http://{}/v1/bridge", pairing.peer))
        .json(&envelope);
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    let mut response = builder.send().await.map_err(|_| {
        ApiError::unavailable("paired daemon unavailable; operation was not retried")
    })?;
    if !response.status().is_success() {
        return Err(ApiError::unavailable(
            "paired daemon rejected the bridge; check pairing and daemon availability",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        ApiError::unavailable("bridge response interrupted; operation was not retried")
    })? {
        if bytes.len().saturating_add(chunk.len()) > response_limit {
            return Err(ApiError::unavailable("bridge response exceeds size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let envelope = serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::unavailable("invalid bridge response"))?;
    let reply: Reply = open(&pairing.key, &context(destination, true), envelope)?;
    if reply.request_id != id {
        return Err(ApiError::forbidden(
            "bridge response belongs to another request",
        ));
    }
    let result = reply
        .result
        .map_err(|error| ApiError::with_status(error.status, error.message))?;
    serde_json::from_value(result).map_err(|_| ApiError::unavailable("invalid bridge reply"))
}

pub async fn handle(
    State(state): State<AppState>,
    Json(envelope): Json<Envelope>,
) -> Result<Json<Envelope>, ApiError> {
    let pairing = state.pairing().await?;
    let message: Message = open(&pairing.key, &context(state.mode, false), envelope)?;
    state.bridge_replay.check(&message).await?;
    let request: Request = serde_json::from_value(message.payload)
        .map_err(|_| ApiError::bad_request("unknown bridge operation"))?;
    let result = dispatch(&state, request).await.map_err(|error| Failure {
        status: error.status_code(),
        message: error.to_string(),
    });
    let reply = Reply {
        request_id: message.id,
        result,
    };
    Ok(Json(seal(
        &pairing.key,
        &context(state.mode, true),
        &reply,
    )?))
}

async fn dispatch(state: &AppState, request: Request) -> Result<Value, ApiError> {
    match (state.mode, request) {
        (DaemonMode::User, Request::Identity) => crate::execution::current_identity()
            .map_err(|error| ApiError::internal(error.to_string())),
        #[cfg(windows)]
        (DaemonMode::User, Request::UserContext) => {
            let context = localshelld_native_process::current_user_context()
                .map_err(|error| ApiError::unavailable(error.to_string()))?;
            serde_json::to_value(context)
                .map_err(|_| ApiError::internal("user context serialization failed"))
        }
        (_, Request::Ping) => {
            Ok(json!({"daemon": state.mode, "desktop_available": state.consent.is_some()}))
        }
        (DaemonMode::User, Request::Consent { prompt }) => {
            crate::consent::validate_request(&prompt.agent_name, &prompt.request_reason)?;
            crate::secrets::normalize_keys(&prompt.secret_keys)?;
            let broker = state
                .consent
                .as_ref()
                .ok_or_else(|| ApiError::unavailable("user daemon has no graphical desktop"))?;
            serde_json::to_value(broker.request(prompt).await?)
                .map_err(|_| ApiError::internal("decision serialization failed"))
        }
        (
            DaemonMode::Privileged,
            Request::EnterInteractive {
                agent_name,
                request_reason,
                secret_keys,
            },
        ) => {
            Box::pin(state.enter_interactive_with_secrets(
                &agent_name,
                &request_reason,
                &secret_keys,
            ))
            .await
        }
        (DaemonMode::User, Request::SecretsList) => Ok(json!(Box::pin(state.secret_keys()).await?)),
        (DaemonMode::User, Request::SecretPut { key, value }) => {
            let value = zeroize::Zeroizing::new(value);
            Box::pin(state.put_secret(&key, &value)).await?;
            Ok(Value::Null)
        }
        (DaemonMode::User, Request::SecretRemove { key }) => {
            Box::pin(state.remove_secret(&key)).await?;
            Ok(Value::Null)
        }
        (DaemonMode::User, Request::SecretsResolve { keys }) => {
            serde_json::to_value(Box::pin(state.resolve_secrets(&keys)).await?)
                .map_err(|_| ApiError::internal("environment serialization failed"))
        }
        (DaemonMode::Privileged, Request::SessionEnvironment { handle }) => {
            serde_json::to_value(state.session_environment(&handle).await?)
                .map_err(|_| ApiError::internal("environment serialization failed"))
        }
        (DaemonMode::User, Request::Attach { handle }) => {
            let entered = state.attach_remote(&handle).await?;
            serde_json::to_value(entered)
                .map_err(|_| ApiError::internal("session serialization failed"))
        }
        (
            DaemonMode::User,
            Request::Run {
                handle,
                command,
                timeout_seconds,
                stdin,
            },
        ) => {
            // This endpoint never executes elevated commands in the desktop.
            let result = Box::pin(state.run_command(
                &handle,
                &command,
                crate::execution::PrivilegeLevel::CurrentUser,
                crate::execution::RunOptions {
                    stdin,
                    timeout_seconds,
                },
            ))
            .await?;
            serde_json::to_value(result)
                .map_err(|_| ApiError::internal("execution serialization failed"))
        }
        _ => Err(ApiError::forbidden(
            "bridge operation is not available for this daemon role",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn envelope_rejects_wrong_keys_directions_and_replays() {
        let key = generate_key();
        let message = Message {
            id: "once".into(),
            issued_at_ms: chrono::Utc::now().timestamp_millis(),
            payload: json!({}),
        };
        let aad = context(DaemonMode::User, false);
        let sealed = seal(&key, &aad, &message).unwrap();
        let opened: Message = open(&key, &aad, sealed).unwrap();
        let guard = ReplayGuard::default();
        guard.check(&opened).await.unwrap();
        assert!(guard.check(&opened).await.is_err());
        assert!(
            open::<Message>(&generate_key(), &aad, seal(&key, &aad, &message).unwrap()).is_err()
        );
        assert!(
            open::<Message>(
                &key,
                &context(DaemonMode::Privileged, false),
                seal(&key, &aad, &message).unwrap()
            )
            .is_err()
        );
        assert!(
            open::<Message>(
                &key,
                &context(DaemonMode::User, true),
                seal(&key, &aad, &message).unwrap()
            )
            .is_err()
        );
        assert!(
            open::<Message>(
                &key,
                &aad,
                Envelope {
                    nonce: "invalid".into(),
                    ciphertext: "".into()
                }
            )
            .is_err()
        );
    }
}
