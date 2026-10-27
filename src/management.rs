//! Human-facing management sessions and credential enrollment. Execution tokens
//! cannot authenticate here; WebAuthn challenge state never leaves the server.
use std::{collections::HashMap, net::SocketAddr, time::Duration};

use axum::{
    Json, Router,
    extract::State,
    http::HeaderMap,
    routing::{get, post},
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use webauthn_rs::prelude::*;
use zeroize::Zeroizing;

use crate::{
    auth::{self, AuthError, Credential},
    bridge,
    config::{load_or_create_seal_key, seal, seal_key_path},
    server::{ApiError, AppState, DaemonMode, strong_handle},
    settings::{Pairing, StoredPasskey},
};

const SESSION_SECONDS: i64 = 600;
const CHALLENGE_SECONDS: i64 = 300;
const MAX_CHALLENGES: usize = 64;

enum Pending {
    Totp {
        owner: String,
        secret: Zeroizing<Vec<u8>>,
    },
    Registration {
        owner: String,
        name: String,
        state: PasskeyRegistration,
    },
    Authentication(PasskeyAuthentication),
}

#[derive(Default)]
pub struct Management {
    sessions: HashMap<String, i64>,
    challenges: HashMap<String, (i64, Pending)>,
}

impl Management {
    fn prune(&mut self) {
        let now = Utc::now().timestamp();
        self.sessions.retain(|_, expiry| *expiry > now);
        self.challenges.retain(|_, (expiry, _)| *expiry > now);
    }

    fn session(&mut self) -> Result<Value, ApiError> {
        self.prune();
        if self.sessions.len() >= 128 {
            return Err(ApiError::unavailable("too many management sessions"));
        }
        let session = strong_handle();
        let expires_at = Utc::now().timestamp() + SESSION_SECONDS;
        self.sessions.insert(session.clone(), expires_at);
        Ok(json!({"session":session, "expires_at":expires_at}))
    }

    fn insert(&mut self, pending: Pending) -> Result<String, ApiError> {
        self.prune();
        if self.challenges.len() >= MAX_CHALLENGES {
            return Err(ApiError::unavailable(
                "too many pending enrollments; retry later",
            ));
        }
        let id = strong_handle();
        self.challenges.insert(
            id.clone(),
            (Utc::now().timestamp() + CHALLENGE_SECONDS, pending),
        );
        Ok(id)
    }

    fn take(&mut self, id: &str) -> Result<Pending, ApiError> {
        self.prune();
        self.challenges
            .remove(id)
            .map(|(_, pending)| pending)
            .ok_or_else(|| ApiError::bad_request("challenge expired or already used; start again"))
    }
}

fn bearer(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| AuthError::InvalidCredential.into())
}

async fn owner(state: &AppState, headers: &HeaderMap) -> Result<String, ApiError> {
    let bearer = bearer(headers)?;
    let mut management = state.management.lock().await;
    management.prune();
    if management.sessions.contains_key(bearer) {
        Ok(bearer.into())
    } else {
        Err(AuthError::InvalidCredential.into())
    }
}

pub async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    credential: Option<&Credential>,
) -> Result<(), ApiError> {
    if let Some(credential) = credential {
        return state.authenticate(credential).await;
    }
    owner(state, headers).await.map(|_| ())
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/admin/login", post(login))
        .route("/v1/admin/logout", post(logout))
        .route("/v1/admin/settings", get(settings))
        .route("/v1/admin/secrets", get(secrets))
        .route("/v1/admin/secrets/put", post(secret_put))
        .route("/v1/admin/secrets/remove", post(secret_remove))
        .route("/v1/admin/secrets/export", post(secret_export))
        .route(
            "/v1/admin/secrets/import",
            post(secret_import).layer(axum::extract::DefaultBodyLimit::max(
                localshelld_sync_protocol::MAX_BODY_BYTES,
            )),
        )
        .route("/v1/admin/sync", post(sync))
        .route("/v1/admin/password", post(password))
        .route("/v1/admin/totp/start", post(totp_start))
        .route("/v1/admin/totp/finish", post(totp_finish))
        .route("/v1/admin/totp/remove", post(totp_remove))
        .route("/v1/admin/passkeys/register/start", post(register_start))
        .route("/v1/admin/passkeys/register/finish", post(register_finish))
        .route(
            "/v1/admin/passkeys/authenticate/start",
            post(authenticate_start),
        )
        .route(
            "/v1/admin/passkeys/authenticate/finish",
            post(authenticate_finish),
        )
        .route("/v1/admin/passkeys/remove", post(passkey_remove))
        .route("/v1/admin/pairing/new", post(pairing_new))
        .route("/v1/admin/pairing/connect", post(pairing_connect))
        .route("/v1/admin/pairing/remove", post(pairing_remove))
        .route("/v1/admin/remembered/reset", post(remembered_reset))
}

async fn secrets(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    Ok(Json(json!({"keys":state.secret_keys().await?})))
}

async fn secret_export(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    owner(&state, &headers).await?;
    Ok((
        [(
            "content-disposition",
            "attachment; filename=localshelld-credentials.json",
        )],
        Json(state.export_secrets().await?),
    ))
}

async fn secret_import(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(file): Json<localshelld_sync_protocol::CredentialFile>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    state.import_secrets(file).await?;
    Ok(Json(json!({"imported":true})))
}

async fn sync(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(action): Json<crate::sync::Action>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    Ok(Json(state.sync_action(action).await?))
}

#[derive(Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]
struct SecretPut {
    key: String,
    value: String,
}

async fn secret_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SecretPut>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    state.put_secret(&body.key, &body.value).await?;
    Ok(Json(json!({"saved":true})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretKey {
    key: String,
}

async fn secret_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<SecretKey>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    state.remove_secret(&body.key).await?;
    Ok(Json(json!({"removed":true})))
}

#[derive(Deserialize)]
struct Login {
    credential: Credential,
}

async fn login(
    State(state): State<AppState>,
    Json(body): Json<Login>,
) -> Result<Json<Value>, ApiError> {
    state.authenticate(&body.credential).await?;
    Ok(Json(state.management.lock().await.session()?))
}

async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let owner = owner(&state, &headers).await?;
    state.management.lock().await.sessions.remove(&owner);
    Ok(Json(json!({"logged_out":true})))
}

async fn settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    let settings = state.settings.lock().await;
    Ok(Json(json!({
        "daemon":state.mode, "totp_enabled":settings.config.totp_secret.is_some(),
        "passkeys":settings.saved.passkeys.iter().map(|key| json!({"id":key.id,"name":key.name,"created_at":key.created_at})).collect::<Vec<_>>(),
        "remembered":settings.saved.remembered,
        "paired_peer":settings.saved.pairing.as_ref().map(|pair| pair.peer.to_string()),
        "desktop_available":state.consent.is_some(),
        "passkey_origin":format!("http://localhost:{}",state.config.bind.port())
    })))
}

#[derive(Deserialize)]
struct Password {
    password: String,
}

async fn password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Password>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    let hash = tokio::task::spawn_blocking(move || auth::hash_password(body.password.as_bytes()))
        .await
        .map_err(|_| ApiError::internal("password worker failed"))?
        .map_err(|_| ApiError::internal("password hashing failed"))?;
    let mut settings = state.settings.lock().await;
    let mut config = settings.config.clone();
    config.password_hash = hash.clone();
    settings.save_config(config).await?;
    state.auth.lock().await.replace_password(hash);
    *state.management.lock().await = Management::default();
    Ok(Json(json!({"updated":true,"reauthenticate":true})))
}

async fn totp_start(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let owner = owner(&state, &headers).await?;
    let secret = auth::generate_totp_secret();
    let mut totp =
        auth::create_totp(&secret).map_err(|_| ApiError::internal("TOTP enrollment failed"))?;
    if state.mode == DaemonMode::User {
        totp.account_name = "local-user".into();
    }
    let id = state
        .management
        .lock()
        .await
        .insert(Pending::Totp { owner, secret })?;
    Ok(Json(
        json!({"enrollment_id":id,"secret":totp.get_secret_base32(),"uri":totp.get_url(),"expires_in":CHALLENGE_SECONDS}),
    ))
}

#[derive(Deserialize)]
struct TotpFinish {
    enrollment_id: String,
    code: String,
}

async fn totp_finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<TotpFinish>,
) -> Result<Json<Value>, ApiError> {
    let session = owner(&state, &headers).await?;
    let Pending::Totp { owner, secret } =
        state.management.lock().await.take(&body.enrollment_id)?
    else {
        return Err(ApiError::bad_request("wrong enrollment type"));
    };
    if owner != session {
        return Err(AuthError::InvalidCredential.into());
    }
    let totp =
        auth::create_totp(&secret).map_err(|_| ApiError::internal("invalid TOTP enrollment"))?;
    if !totp.check_current(body.code.trim()).unwrap_or(false) {
        return Err(ApiError::bad_request(
            "incorrect TOTP code; start enrollment again",
        ));
    }
    let mut settings = state.settings.lock().await;
    let path = settings
        .config_path
        .clone()
        .ok_or_else(|| ApiError::unavailable("configuration storage unavailable"))?;
    let key = tokio::task::spawn_blocking(move || load_or_create_seal_key(&seal_key_path(&path)))
        .await
        .map_err(|_| ApiError::internal("seal key worker failed"))?
        .map_err(|_| ApiError::internal("seal key unavailable"))?;
    let mut config = settings.config.clone();
    config.totp_secret =
        Some(seal(&secret, &key).map_err(|_| ApiError::internal("TOTP encryption failed"))?);
    settings.save_config(config).await?;
    state.auth.lock().await.replace_totp(Some(secret));
    Ok(Json(json!({"enabled":true})))
}

async fn totp_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    let mut settings = state.settings.lock().await;
    let mut config = settings.config.clone();
    config.totp_secret = None;
    settings.save_config(config).await?;
    state.auth.lock().await.replace_totp(None);
    Ok(Json(json!({"enabled":false})))
}

fn webauthn(state: &AppState) -> Result<Webauthn, ApiError> {
    let origin = Url::parse(&format!("http://localhost:{}", state.config.bind.port()))
        .map_err(|_| ApiError::internal("invalid passkey origin"))?;
    WebauthnBuilder::new("localhost", &origin)
        .map_err(|_| ApiError::internal("WebAuthn initialization failed"))?
        .rp_name("localshelld")
        .allow_any_port(false)
        .build()
        .map_err(|_| ApiError::internal("WebAuthn initialization failed"))
}

#[derive(Deserialize)]
struct Name {
    name: String,
}

async fn register_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Name>,
) -> Result<Json<Value>, ApiError> {
    let owner = owner(&state, &headers).await?;
    if body.name.trim().is_empty() || body.name.len() > 256 {
        return Err(ApiError::bad_request(
            "passkey name must contain 1–256 bytes",
        ));
    }
    let mut settings = state.settings.lock().await;
    if settings.saved.passkeys.len() >= 32 {
        return Err(ApiError::bad_request("at most 32 passkeys are supported"));
    }
    let mut next = settings.saved.clone();
    let user_id = *next.user_id.get_or_insert_with(uuid::Uuid::new_v4);
    settings.save(next).await?;
    let exclude = settings
        .saved
        .passkeys
        .iter()
        .map(|key| key.key.cred_id().clone())
        .collect();
    let account = if state.mode == DaemonMode::User {
        "local-user"
    } else {
        "local-admin"
    };
    let (options, registration) = webauthn(&state)?
        .start_passkey_registration(user_id, account, account, Some(exclude))
        .map_err(|_| ApiError::internal("passkey registration could not start"))?;
    drop(settings);
    let id = state
        .management
        .lock()
        .await
        .insert(Pending::Registration {
            owner,
            name: body.name,
            state: registration,
        })?;
    Ok(Json(json!({"challenge_id":id,"options":options})))
}

#[derive(Deserialize)]
struct RegisterFinish {
    challenge_id: String,
    credential: RegisterPublicKeyCredential,
}

async fn register_finish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RegisterFinish>,
) -> Result<Json<Value>, ApiError> {
    let session = owner(&state, &headers).await?;
    let Pending::Registration {
        owner,
        name,
        state: registration,
    } = state.management.lock().await.take(&body.challenge_id)?
    else {
        return Err(ApiError::bad_request("wrong challenge type"));
    };
    if owner != session {
        return Err(AuthError::InvalidCredential.into());
    }
    let key = webauthn(&state)?
        .finish_passkey_registration(&body.credential, &registration)
        .map_err(|_| ApiError::forbidden("passkey registration verification failed"))?;
    let mut settings = state.settings.lock().await;
    if settings.saved.passkeys.len() >= 32 {
        return Err(ApiError::bad_request("at most 32 passkeys are supported"));
    }
    if settings
        .saved
        .passkeys
        .iter()
        .any(|entry| entry.key.cred_id() == key.cred_id())
    {
        return Err(ApiError::bad_request("passkey is already registered"));
    }
    let mut next = settings.saved.clone();
    next.passkeys.push(StoredPasskey {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        created_at: Utc::now().timestamp(),
        key,
    });
    settings.save(next).await?;
    Ok(Json(json!({"registered":true})))
}

async fn authenticate_start(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let keys = state
        .settings
        .lock()
        .await
        .saved
        .passkeys
        .iter()
        .map(|entry| entry.key.clone())
        .collect::<Vec<_>>();
    if keys.is_empty() {
        return Err(ApiError::bad_request(
            "no passkeys registered; sign in with Master Password or TOTP first",
        ));
    }
    let (options, authentication) = webauthn(&state)?
        .start_passkey_authentication(&keys)
        .map_err(|_| ApiError::internal("passkey authentication could not start"))?;
    let id = state
        .management
        .lock()
        .await
        .insert(Pending::Authentication(authentication))?;
    Ok(Json(json!({"challenge_id":id,"options":options})))
}

#[derive(Deserialize)]
struct AuthenticateFinish {
    challenge_id: String,
    credential: PublicKeyCredential,
}

async fn authenticate_finish(
    State(state): State<AppState>,
    Json(body): Json<AuthenticateFinish>,
) -> Result<Json<Value>, ApiError> {
    let Pending::Authentication(authentication) =
        state.management.lock().await.take(&body.challenge_id)?
    else {
        return Err(ApiError::bad_request("wrong challenge type"));
    };
    let result = webauthn(&state)?
        .finish_passkey_authentication(&body.credential, &authentication)
        .map_err(|_| ApiError::forbidden("passkey authentication verification failed"))?;
    let mut settings = state.settings.lock().await;
    let mut next = settings.saved.clone();
    let key = next
        .passkeys
        .iter_mut()
        .find(|entry| entry.key.cred_id() == result.cred_id())
        .ok_or_else(|| ApiError::forbidden("passkey was removed"))?;
    key.key.update_credential(&result);
    settings.save(next).await?;
    Ok(Json(state.management.lock().await.session()?))
}

#[derive(Deserialize)]
struct Id {
    id: String,
}

async fn passkey_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Id>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    let mut settings = state.settings.lock().await;
    let mut next = settings.saved.clone();
    next.passkeys.retain(|key| key.id != body.id);
    settings.save(next).await?;
    Ok(Json(json!({"removed":true})))
}

async fn pairing_new(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    if state.mode != DaemonMode::User {
        return Err(ApiError::bad_request(
            "generate the pairing code in the user daemon management UI",
        ));
    }
    let key = bridge::generate_key();
    let mut settings = state.settings.lock().await;
    let mut next = settings.saved.clone();
    next.pairing = Some(Pairing {
        key: key.clone(),
        peer: state.config.privileged_daemon,
    });
    settings.save(next).await?;
    Ok(Json(
        json!({"code":key,"user_daemon":state.config.bind.to_string()}),
    ))
}

#[derive(Deserialize)]
struct Connect {
    code: String,
    user_daemon: SocketAddr,
}

async fn pairing_connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Connect>,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    if state.mode != DaemonMode::Privileged {
        return Err(ApiError::bad_request(
            "complete pairing in the privileged daemon management UI",
        ));
    }
    bridge::validate_key(&body.code)?;
    let pairing = Pairing {
        key: body.code,
        peer: body.user_daemon,
    };
    let status: Value = bridge::call(
        &pairing,
        DaemonMode::User,
        bridge::Request::Ping,
        Some(Duration::from_secs(5)),
        65_536,
    )
    .await?;
    if status["daemon"] != "user" {
        return Err(ApiError::bad_request("peer is not the user daemon"));
    }
    let mut settings = state.settings.lock().await;
    let mut next = settings.saved.clone();
    next.pairing = Some(pairing);
    settings.save(next).await?;
    Ok(Json(
        json!({"paired":true,"desktop_available":status["desktop_available"]}),
    ))
}

async fn pairing_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    let mut settings = state.settings.lock().await;
    let mut next = settings.saved.clone();
    next.pairing = None;
    settings.save(next).await?;
    Ok(Json(json!({"paired":false})))
}

async fn remembered_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    owner(&state, &headers).await?;
    let mut settings = state.settings.lock().await;
    let count = settings.saved.remembered.len();
    let mut next = settings.saved.clone();
    next.remembered.clear();
    next.remembered_secrets.clear();
    settings.save(next).await?;
    Ok(Json(json!({"cleared":count})))
}

#[cfg(test)]
mod tests;
