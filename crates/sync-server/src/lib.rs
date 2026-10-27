#![forbid(unsafe_code)]
mod storage;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use localshelld_sync_protocol::{
    DeviceStart, MAX_BODY_BYTES, Replace, Snapshot, Tokens, validate_credentials,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use storage::{Session, Store, StoredPasskey, digest, random};
use tokio::sync::{Mutex, watch};
use webauthn_rs::prelude::*;

const ACCESS_SECONDS: i64 = 300;
const REFRESH_SECONDS: i64 = 30 * 86400;
const CHALLENGE_SECONDS: i64 = 300;
const MAX_PENDING: usize = 128;
const MAX_SESSIONS: usize = 128;

#[derive(Debug)]
struct Error(StatusCode, &'static str);
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
fn bad(message: &'static str) -> Error {
    Error(StatusCode::BAD_REQUEST, message)
}
fn denied() -> Error {
    Error(
        StatusCode::UNAUTHORIZED,
        "authentication required or expired",
    )
}
fn internal() -> Error {
    Error(StatusCode::INTERNAL_SERVER_ERROR, "state operation failed")
}
type Result<T> = std::result::Result<T, Error>;

enum Challenge {
    Register {
        state: PasskeyRegistration,
        name: String,
        bootstrap: bool,
        session_id: Option<String>,
    },
    Authenticate(PasskeyAuthentication),
}
struct Device {
    user_code: String,
    name: String,
    expires_at: i64,
    last_poll: i64,
    approved: bool,
}
struct Pending {
    challenges: HashMap<String, (i64, Challenge)>,
    devices: HashMap<String, Device>,
    minute: i64,
    starts: u32,
}
impl Pending {
    fn prune(&mut self) {
        let now = Utc::now().timestamp();
        self.challenges.retain(|_, (expiry, _)| *expiry > now);
        self.devices.retain(|_, device| device.expires_at > now);
    }
    fn allow_start(&mut self) -> Result<()> {
        self.prune();
        let minute = Utc::now().timestamp() / 60;
        if minute != self.minute {
            self.minute = minute;
            self.starts = 0;
        }
        self.starts += 1;
        if self.starts > 120 || self.challenges.len() + self.devices.len() >= MAX_PENDING {
            return Err(Error(
                StatusCode::TOO_MANY_REQUESTS,
                "too many pending requests; retry later",
            ));
        }
        Ok(())
    }
    fn take(&mut self, id: &str) -> Result<Challenge> {
        self.prune();
        self.challenges
            .remove(id)
            .map(|(_, value)| value)
            .ok_or_else(|| bad("challenge expired or already used"))
    }
}

#[derive(Clone)]
pub struct App {
    store: Arc<Mutex<Store>>,
    pending: Arc<Mutex<Pending>>,
    webauthn: Arc<Webauthn>,
    origin: String,
    revision: watch::Sender<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Claims {
    sub: String,
    sid: String,
    iss: String,
    aud: String,
    exp: i64,
    iat: i64,
}

impl App {
    pub fn open(directory: &Path, origin: &str) -> anyhow::Result<Self> {
        let url = Url::parse(origin)?;
        anyhow::ensure!(
            url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
                && url.username().is_empty()
                && url.password().is_none(),
            "origin must contain only scheme, host and optional port"
        );
        anyhow::ensure!(
            url.scheme() == "https"
                || (url.scheme() == "http" && url.host_str() == Some("localhost")),
            "HTTPS is required, except http://localhost for development"
        );
        let origin = url.origin().ascii_serialization();
        let webauthn = WebauthnBuilder::new(
            url.host_str()
                .ok_or_else(|| anyhow::anyhow!("missing host"))?,
            &url,
        )?
        .rp_name("localshelld sync")
        .build()?;
        let store = Store::open(directory)?;
        let (revision, _) = watch::channel(store.saved.vault.revision);
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            webauthn: Arc::new(webauthn),
            origin,
            pending: Arc::new(Mutex::new(Pending {
                challenges: HashMap::new(),
                devices: HashMap::new(),
                minute: 0,
                starts: 0,
            })),
            revision,
        })
    }
    fn authenticate(&self, store: &Store, headers: &HeaderMap) -> Result<Claims> {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(denied)?;
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = 0;
        validation.set_audience(&["localshelld-sync"]);
        validation.set_issuer(&[&self.origin]);
        validation.set_required_spec_claims(&["exp", "sub", "iss", "aud"]);
        let claims = jsonwebtoken::decode::<Claims>(
            token,
            &DecodingKey::from_secret(store.saved.jwt_key.as_bytes()),
            &validation,
        )
        .map_err(|_| denied())?
        .claims;
        if claims.sub != store.saved.user_id.to_string()
            || store
                .saved
                .sessions
                .get(&claims.sid)
                .is_none_or(|s| s.expires_at <= Utc::now().timestamp())
        {
            return Err(denied());
        }
        Ok(claims)
    }
    fn browser(&self, store: &Store, headers: &HeaderMap) -> Result<Claims> {
        let claims = self.authenticate(store, headers)?;
        if !store.saved.sessions[&claims.sid].browser {
            return Err(Error(
                StatusCode::FORBIDDEN,
                "sign in with a passkey in the browser",
            ));
        }
        Ok(claims)
    }
    fn tokens(
        &self,
        saved: &storage::Saved,
        sid: &str,
        refresh_token: String,
        refresh_expires_at: i64,
    ) -> Result<Tokens> {
        let now = Utc::now().timestamp();
        let expires_at = (now + ACCESS_SECONDS).min(refresh_expires_at);
        let claims = Claims {
            sub: saved.user_id.to_string(),
            sid: sid.into(),
            iss: self.origin.clone(),
            aud: "localshelld-sync".into(),
            exp: expires_at,
            iat: now,
        };
        let access_token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(saved.jwt_key.as_bytes()),
        )
        .map_err(|_| internal())?;
        Ok(Tokens {
            access_token,
            refresh_token,
            expires_at,
            refresh_expires_at,
        })
    }
    fn new_session(&self, store: &mut Store, name: String, browser: bool) -> Result<Tokens> {
        let now = Utc::now().timestamp();
        let mut next = store.saved.clone();
        next.sessions.retain(|_, session| session.expires_at > now);
        if next.sessions.len() >= MAX_SESSIONS {
            return Err(bad("too many trusted sessions; revoke a session first"));
        }
        let sid = random();
        // Browser sessions are deliberately short-lived and cannot be renewed:
        // every trust-management action is backed by a recent passkey ceremony.
        let expires = now
            + if browser {
                ACCESS_SECONDS
            } else {
                REFRESH_SECONDS
            };
        let refresh_token = if browser {
            String::new()
        } else {
            format!("{sid}.{}", random())
        };
        next.sessions.insert(
            sid.clone(),
            Session {
                name,
                browser,
                created_at: now,
                expires_at: expires,
                refresh_hash: digest(&refresh_token),
                used_refresh: vec![],
            },
        );
        let tokens = self.tokens(&next, &sid, refresh_token, expires)?;
        store.commit(next).map_err(|_| internal())?;
        Ok(tokens)
    }
}

pub fn router(app: App) -> Router {
    Router::new()
        .route(
            "/health",
            get(|| async { Json(json!({"service":"localshelld-sync", "protocol":1})) }),
        )
        .route("/sync/v1/info", get(info))
        .route("/sync/v1/passkeys/register/start", post(register_start))
        .route("/sync/v1/passkeys/register/finish", post(register_finish))
        .route(
            "/sync/v1/passkeys/authenticate/start",
            post(authenticate_start),
        )
        .route(
            "/sync/v1/passkeys/authenticate/finish",
            post(authenticate_finish),
        )
        .route("/sync/v1/passkeys/remove", post(passkey_remove))
        .route("/sync/v1/session/refresh", post(refresh))
        .route("/sync/v1/session/revoke", post(revoke))
        .route("/sync/v1/sessions", get(sessions))
        .route("/sync/v1/device/start", post(device_start))
        .route("/sync/v1/device/inspect", post(device_inspect))
        .route("/sync/v1/device/approve", post(device_approve))
        .route("/sync/v1/device/poll", post(device_poll))
        .route("/sync/v1/vault", get(vault_get).put(vault_put))
        .route("/sync/v1/events", get(events))
        .fallback(assets)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn_with_state(app.clone(), security))
        .with_state(app)
}

async fn security(State(app): State<App>, request: Request, next: Next) -> Response {
    if request
        .headers()
        .get(header::ORIGIN)
        .is_some_and(|v| v.to_str().ok() != Some(app.origin.as_str()))
    {
        return Error(StatusCode::FORBIDDEN, "origin is not allowed").into_response();
    }
    let mut response = next.run(request).await;
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; font-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    ] {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(name),
            axum::http::HeaderValue::from_static(value),
        );
    }
    response
}

async fn info(State(app): State<App>) -> Json<Value> {
    let store = app.store.lock().await;
    Json(json!({"protocol":1,"initialized":!store.saved.passkeys.is_empty(),"origin":app.origin}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterStart {
    name: String,
    #[serde(default)]
    bootstrap_code: String,
}
async fn register_start(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<RegisterStart>,
) -> Result<Json<Value>> {
    if body.name.trim().is_empty() || body.name.len() > 80 {
        return Err(bad("passkey name must contain 1–80 bytes"));
    }
    let mut pending = app.pending.lock().await;
    pending.allow_start()?;
    let store = app.store.lock().await;
    let bootstrap = store.saved.passkeys.is_empty();
    let session_id = if bootstrap {
        if store.saved.bootstrap_hash.as_deref() != Some(digest(&body.bootstrap_code).as_str()) {
            return Err(denied());
        }
        None
    } else {
        Some(app.browser(&store, &headers)?.sid)
    };
    if store.saved.passkeys.len() >= 32 {
        return Err(bad("at most 32 passkeys can be registered"));
    }
    let exclude = Some(
        store
            .saved
            .passkeys
            .iter()
            .map(|key| key.key.cred_id().clone())
            .collect(),
    );
    let (options, state) = app
        .webauthn
        .start_passkey_registration(store.saved.user_id, "owner", "localshelld owner", exclude)
        .map_err(|_| bad("could not start passkey registration"))?;
    let id = random();
    pending.challenges.insert(
        id.clone(),
        (
            Utc::now().timestamp() + CHALLENGE_SECONDS,
            Challenge::Register {
                state,
                name: body.name.trim().into(),
                bootstrap,
                session_id,
            },
        ),
    );
    Ok(Json(json!({"challenge_id":id,"options":options})))
}

#[derive(Deserialize)]
struct RegisterFinish {
    challenge_id: String,
    credential: RegisterPublicKeyCredential,
}
async fn register_finish(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<RegisterFinish>,
) -> Result<Json<Tokens>> {
    let challenge = app.pending.lock().await.take(&body.challenge_id)?;
    let Challenge::Register {
        state,
        name,
        bootstrap,
        session_id,
    } = challenge
    else {
        return Err(bad("wrong ceremony"));
    };
    let key = app
        .webauthn
        .finish_passkey_registration(&body.credential, &state)
        .map_err(|_| bad("passkey registration failed"))?;
    let mut store = app.store.lock().await;
    if bootstrap {
        if !store.saved.passkeys.is_empty() {
            return Err(Error(StatusCode::CONFLICT, "already initialized"));
        }
    } else if Some(app.browser(&store, &headers)?.sid) != session_id {
        return Err(denied());
    }
    if store.saved.passkeys.len() >= 32
        || store
            .saved
            .passkeys
            .iter()
            .any(|existing| existing.key.cred_id() == key.cred_id())
    {
        return Err(bad("passkey already registered or limit reached"));
    }
    let mut next = store.saved.clone();
    next.bootstrap_hash = None;
    next.passkeys.push(StoredPasskey {
        id: random(),
        name,
        key,
    });
    store.commit(next).map_err(|_| internal())?;
    store.remove_bootstrap_file();
    Ok(Json(app.new_session(&mut store, "Browser".into(), true)?))
}

async fn authenticate_start(State(app): State<App>) -> Result<Json<Value>> {
    let mut pending = app.pending.lock().await;
    pending.allow_start()?;
    let store = app.store.lock().await;
    if store.saved.passkeys.is_empty() {
        return Err(bad("initialize the service first"));
    }
    let keys: Vec<_> = store
        .saved
        .passkeys
        .iter()
        .map(|key| key.key.clone())
        .collect();
    let (options, state) = app
        .webauthn
        .start_passkey_authentication(&keys)
        .map_err(|_| bad("could not start authentication"))?;
    let id = random();
    pending.challenges.insert(
        id.clone(),
        (
            Utc::now().timestamp() + CHALLENGE_SECONDS,
            Challenge::Authenticate(state),
        ),
    );
    Ok(Json(json!({"challenge_id":id,"options":options})))
}

#[derive(Deserialize)]
struct AuthenticateFinish {
    challenge_id: String,
    credential: PublicKeyCredential,
}
async fn authenticate_finish(
    State(app): State<App>,
    Json(body): Json<AuthenticateFinish>,
) -> Result<Json<Tokens>> {
    let Challenge::Authenticate(state) = app.pending.lock().await.take(&body.challenge_id)? else {
        return Err(bad("wrong ceremony"));
    };
    let auth = app
        .webauthn
        .finish_passkey_authentication(&body.credential, &state)
        .map_err(|_| denied())?;
    let mut store = app.store.lock().await;
    let mut next = store.saved.clone();
    let key = next
        .passkeys
        .iter_mut()
        .find(|key| key.key.cred_id() == auth.cred_id())
        .ok_or_else(denied)?;
    key.key.update_credential(&auth);
    store.commit(next).map_err(|_| internal())?;
    Ok(Json(app.new_session(&mut store, "Browser".into(), true)?))
}

#[derive(Deserialize)]
struct Id {
    id: String,
}
async fn passkey_remove(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Id>,
) -> Result<Json<Value>> {
    let mut store = app.store.lock().await;
    app.browser(&store, &headers)?;
    if store.saved.passkeys.len() <= 1 {
        return Err(bad("the last passkey cannot be removed"));
    }
    let mut next = store.saved.clone();
    next.passkeys.retain(|key| key.id != body.id);
    store.commit(next).map_err(|_| internal())?;
    Ok(Json(json!({"removed":true})))
}

#[derive(Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
struct Refresh {
    refresh_token: String,
}
async fn refresh(State(app): State<App>, Json(body): Json<Refresh>) -> Result<Json<Tokens>> {
    let (sid, _) = body.refresh_token.split_once('.').ok_or_else(denied)?;
    let hash = digest(&body.refresh_token);
    let mut store = app.store.lock().await;
    let mut next = store.saved.clone();
    let session = next.sessions.get_mut(sid).ok_or_else(denied)?;
    if session.browser || session.expires_at <= Utc::now().timestamp() {
        return Err(denied());
    }
    if session.used_refresh.contains(&hash) {
        next.sessions.remove(sid);
        store.commit(next).map_err(|_| internal())?;
        return Err(denied());
    }
    if session.refresh_hash != hash {
        return Err(denied());
    }
    if session.used_refresh.len() >= 12_000 {
        return Err(bad("session renewal limit reached; sign in again"));
    }
    let refresh_token = format!("{sid}.{}", random());
    session.used_refresh.push(hash);
    session.refresh_hash = digest(&refresh_token);
    let expires = session.expires_at;
    let result = app.tokens(&next, sid, refresh_token, expires)?;
    store.commit(next).map_err(|_| internal())?;
    Ok(Json(result))
}

async fn sessions(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>> {
    let store = app.store.lock().await;
    app.browser(&store, &headers)?;
    Ok(Json(
        json!({"passkeys":store.saved.passkeys.iter().map(|key| json!({"id":key.id,"name":key.name})).collect::<Vec<_>>(),
        "sessions":store.saved.sessions.iter().filter(|(_,s)| !s.browser && s.expires_at > Utc::now().timestamp()).map(|(id,s)| json!({"id":id,"name":s.name,"created_at":s.created_at,"expires_at":s.expires_at})).collect::<Vec<_>>()}),
    ))
}
async fn revoke(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Id>,
) -> Result<Json<Value>> {
    let mut store = app.store.lock().await;
    let claims = app.authenticate(&store, &headers)?;
    let target = if body.id.is_empty() {
        &claims.sid
    } else {
        &body.id
    };
    if target != &claims.sid {
        app.browser(&store, &headers)?;
    }
    let mut next = store.saved.clone();
    next.sessions.remove(target);
    store.commit(next).map_err(|_| internal())?;
    // Wake long polls so a revoked device is rejected without waiting for timeout.
    app.revision.send_replace(store.saved.vault.revision);
    Ok(Json(json!({"revoked":true})))
}

#[derive(Deserialize)]
struct DeviceName {
    name: String,
}
async fn device_start(
    State(app): State<App>,
    Json(body): Json<DeviceName>,
) -> Result<Json<DeviceStart>> {
    if body.name.trim().is_empty() || body.name.len() > 80 {
        return Err(bad("device name must contain 1–80 bytes"));
    }
    let mut pending = app.pending.lock().await;
    pending.allow_start()?;
    let device_code = random();
    let user_code = uuid::Uuid::new_v4().simple().to_string()[..12].to_uppercase();
    pending.devices.insert(
        digest(&device_code),
        Device {
            user_code: user_code.clone(),
            name: body.name.trim().into(),
            expires_at: Utc::now().timestamp() + 600,
            last_poll: 0,
            approved: false,
        },
    );
    Ok(Json(DeviceStart {
        device_code,
        user_code: user_code.clone(),
        verification_uri: format!("{}/#device={user_code}", app.origin),
        expires_in: 600,
        interval: 3,
    }))
}

#[derive(Deserialize)]
struct UserCode {
    user_code: String,
}
async fn device_inspect(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<UserCode>,
) -> Result<Json<Value>> {
    let mut pending = app.pending.lock().await;
    pending.prune();
    let store = app.store.lock().await;
    app.browser(&store, &headers)?;
    let device = pending
        .devices
        .values()
        .find(|d| d.user_code == body.user_code)
        .ok_or_else(|| bad("device request expired"))?;
    Ok(Json(
        json!({"name":device.name,"user_code":device.user_code,"approved":device.approved}),
    ))
}
async fn device_approve(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<UserCode>,
) -> Result<Json<Value>> {
    let mut pending = app.pending.lock().await;
    pending.prune();
    let store = app.store.lock().await;
    app.browser(&store, &headers)?;
    let device = pending
        .devices
        .values_mut()
        .find(|d| d.user_code == body.user_code)
        .ok_or_else(|| bad("device request expired"))?;
    device.approved = true;
    Ok(Json(json!({"approved":true})))
}
#[derive(Deserialize, zeroize::Zeroize, zeroize::ZeroizeOnDrop)]
struct DeviceCode {
    device_code: String,
}
async fn device_poll(State(app): State<App>, Json(body): Json<DeviceCode>) -> Result<Response> {
    let mut pending = app.pending.lock().await;
    pending.prune();
    let hash = digest(&body.device_code);
    let device = pending
        .devices
        .get_mut(&hash)
        .ok_or_else(|| bad("device request expired or already used"))?;
    let now = Utc::now().timestamp();
    if now - device.last_poll < 2 {
        return Err(Error(
            StatusCode::TOO_MANY_REQUESTS,
            "poll no more than once every 3 seconds",
        ));
    }
    device.last_poll = now;
    if !device.approved {
        return Ok((StatusCode::ACCEPTED, Json(json!({"pending":true}))).into_response());
    }
    let name = device.name.clone();
    let mut store = app.store.lock().await;
    let tokens = app.new_session(&mut store, name, false)?;
    pending.devices.remove(&hash);
    Ok(Json(tokens).into_response())
}

async fn vault_get(State(app): State<App>, headers: HeaderMap) -> Result<Json<Snapshot>> {
    let store = app.store.lock().await;
    app.authenticate(&store, &headers)?;
    Ok(Json(store.saved.vault.clone()))
}
async fn vault_put(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Replace>,
) -> Result<Json<Snapshot>> {
    validate_credentials(&body.credentials).map_err(bad)?;
    let mut store = app.store.lock().await;
    app.authenticate(&store, &headers)?;
    if body.expected_revision != store.saved.vault.revision {
        return Err(Error(
            StatusCode::CONFLICT,
            "revision conflict; fetch the current vault and retry",
        ));
    }
    let mut next = store.saved.clone();
    next.vault.revision = next.vault.revision.checked_add(1).ok_or_else(internal)?;
    next.vault.credentials = body.credentials.clone();
    store.commit(next).map_err(|_| internal())?;
    app.revision.send_replace(store.saved.vault.revision);
    Ok(Json(store.saved.vault.clone()))
}

#[derive(Deserialize)]
struct After {
    after: u64,
}
async fn events(
    State(app): State<App>,
    headers: HeaderMap,
    Query(query): Query<After>,
) -> Result<Json<Value>> {
    let mut receiver = app.revision.subscribe();
    let claims = {
        let store = app.store.lock().await;
        app.authenticate(&store, &headers)?
    };
    if *receiver.borrow_and_update() == query.after {
        let seconds = (claims.exp - Utc::now().timestamp()).clamp(1, 25) as u64;
        let _ = tokio::time::timeout(Duration::from_secs(seconds), receiver.changed()).await;
    }
    let store = app.store.lock().await;
    app.authenticate(&store, &headers)?;
    Ok(Json(json!({"revision":store.saved.vault.revision})))
}

async fn assets(uri: axum::http::Uri) -> Response {
    static ASSETS: include_dir::Dir<'_> =
        include_dir::include_dir!("$CARGO_MANIFEST_DIR/../../web/dist");
    let path = if uri.path() == "/" {
        "sync.html"
    } else {
        uri.path().trim_start_matches('/')
    };
    if path != "sync.html" && !path.starts_with("assets/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(file) = ASSETS.get_file(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [(
            header::CONTENT_TYPE,
            mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string(),
        )],
        file.contents(),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
