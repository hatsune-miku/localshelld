use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::*;
use crate::{auth::AuthManager, config::Config, server::router};

fn stored_state(directory: &std::path::Path) -> AppState {
    let config = Config {
        password_hash: auth::hash_password(b"test").unwrap(),
        ..Config::default()
    };
    let path = directory.join("config.toml");
    config.save(&path).unwrap();
    let auth = AuthManager::new(config.password_hash.clone(), None);
    AppState::new(config, auth).with_storage(path).unwrap()
}

async fn call(
    app: &Router,
    path: &str,
    body: Option<Value>,
    session: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = if body.is_some() {
        Request::post(path)
    } else {
        Request::get(path)
    };
    request = request.header("content-type", "application/json");
    if let Some(session) = session {
        request = request.header("authorization", format!("Bearer {session}"));
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(
                    body.map(|body| Body::from(body.to_string()))
                        .unwrap_or_else(Body::empty),
                )
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn login(app: &Router, password: &str) -> String {
    let (status, body) = call(
        app,
        "/v1/admin/login",
        Some(json!({"credential":{"type":"password","value":password}})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["session"].as_str().unwrap().into()
}

#[tokio::test]
async fn password_update_is_atomic_accepts_empty_and_invalidates_management_sessions() {
    let directory = tempfile::tempdir().unwrap();
    let state = stored_state(directory.path());
    let app = router(state.clone());
    let session = login(&app, "test").await;
    assert_eq!(
        call(&app, "/v1/admin/settings", None, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (_, issued) = call(
        &app,
        "/v1/admin/tokens/issue",
        Some(json!({})),
        Some(&session),
    )
    .await;
    assert_eq!(
        call(&app, "/v1/admin/settings", None, issued["token"].as_str())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _) = call(
        &app,
        "/v1/admin/password",
        Some(json!({"password":""})),
        Some(&session),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        call(&app, "/v1/admin/settings", None, Some(&session))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let new_session = login(&app, "").await;
    assert_eq!(
        call(&app, "/v1/admin/settings", None, Some(&new_session))
            .await
            .0,
        StatusCode::OK
    );
    let persisted = Config::load(&directory.path().join("config.toml")).unwrap();
    let mut restarted = AuthManager::new(persisted.password_hash, None);
    restarted
        .verify_credential(&Credential {
            kind: auth::CredentialKind::Password,
            value: "".into(),
        })
        .unwrap();
    assert!(
        restarted
            .verify_credential(&Credential {
                kind: auth::CredentialKind::Password,
                value: "test".into()
            })
            .is_err()
    );
    assert!(
        state
            .auth
            .lock()
            .await
            .verify_token(issued["token"].as_str().unwrap())
            .is_ok()
    );
}

#[tokio::test]
async fn totp_enrollment_requires_confirmation_then_can_be_disabled() {
    let directory = tempfile::tempdir().unwrap();
    let state = stored_state(directory.path());
    let app = router(state.clone());
    let session = login(&app, "test").await;
    let (_, started) = call(
        &app,
        "/v1/admin/totp/start",
        Some(json!({})),
        Some(&session),
    )
    .await;
    let secret = totp_rs::Secret::Encoded(started["secret"].as_str().unwrap().into())
        .to_bytes()
        .unwrap();
    assert!(state.settings.lock().await.config.totp_secret.is_none());
    let code = auth::create_totp(&secret)
        .unwrap()
        .generate_current()
        .unwrap();
    let (status, _) = call(
        &app,
        "/v1/admin/totp/finish",
        Some(json!({"enrollment_id":started["enrollment_id"],"code":code})),
        Some(&session),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let credential = json!({"type":"totp","value":code});
    assert_eq!(
        call(
            &app,
            "/v1/admin/login",
            Some(json!({"credential":credential})),
            None
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &app,
            "/v1/admin/totp/finish",
            Some(json!({"enrollment_id":started["enrollment_id"],"code":code})),
            Some(&session)
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    call(
        &app,
        "/v1/admin/totp/remove",
        Some(json!({})),
        Some(&session),
    )
    .await;
    assert_eq!(
        call(
            &app,
            "/v1/admin/login",
            Some(json!({"credential":credential})),
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(
        Config::load(&directory.path().join("config.toml"))
            .unwrap()
            .totp_secret
            .is_none()
    );
}

#[tokio::test]
async fn remembered_reset_is_authenticated_and_persisted() {
    let directory = tempfile::tempdir().unwrap();
    let state = stored_state(directory.path());
    state
        .settings
        .lock()
        .await
        .saved
        .remembered
        .insert("agent".into(), true);
    let app = router(state.clone());
    assert_eq!(
        call(&app, "/v1/admin/remembered/reset", Some(json!({})), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let session = login(&app, "test").await;
    let (status, result) = call(
        &app,
        "/v1/admin/remembered/reset",
        Some(json!({})),
        Some(&session),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["cleared"], 1);
    let saved: crate::settings::SavedSettings =
        crate::storage::load_json(&directory.path().join("config.authorization.json")).unwrap();
    assert!(saved.remembered.is_empty());
}

#[tokio::test]
async fn passkey_registration_requires_login_and_keeps_state_server_side() {
    let directory = tempfile::tempdir().unwrap();
    let state = stored_state(directory.path());
    let app = router(state.clone());
    assert_eq!(
        call(
            &app,
            "/v1/admin/passkeys/register/start",
            Some(json!({"name":"test"})),
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let session = login(&app, "test").await;
    let (status, body) = call(
        &app,
        "/v1/admin/passkeys/register/start",
        Some(json!({"name":"test"})),
        Some(&session),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["options"]["publicKey"]["rp"]["id"], "localhost");
    assert_eq!(
        body["options"]["publicKey"]["authenticatorSelection"]["userVerification"],
        "required"
    );
    assert!(body.get("state").is_none());
    let challenge = body["challenge_id"].as_str().unwrap();
    assert!(state.management.lock().await.take(challenge).is_ok());
    assert!(state.management.lock().await.take(challenge).is_err());
    let origins = webauthn(&state).unwrap().get_allowed_origins().to_vec();
    assert_eq!(origins[0].as_str(), "http://localhost:32119/");
}

#[tokio::test]
async fn rejects_foreign_browser_origins_and_dns_rebinding_hosts() {
    let app = router(AppState::new(
        Config::default(),
        AuthManager::new("unused".into(), None),
    ));
    for (name, value) in [
        ("origin", "http://evil.example"),
        ("host", "evil.example:32119"),
        ("origin", "http://localhost:12345"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::get("/health")
                    .header(name, value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
