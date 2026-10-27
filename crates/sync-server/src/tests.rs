use super::*;
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(
            builder
                .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
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

async fn setup() -> (tempfile::TempDir, App, Router, Tokens, Tokens) {
    let directory = tempfile::tempdir().unwrap();
    let app = App::open(directory.path(), "https://sync.example.com").unwrap();
    let (browser, device) = {
        let mut store = app.store.lock().await;
        (
            app.new_session(&mut store, "Test browser".into(), true)
                .unwrap(),
            app.new_session(&mut store, "Test device".into(), false)
                .unwrap(),
        )
    };
    let router = router(app.clone());
    (directory, app, router, browser, device)
}

#[test]
fn second_server_cannot_start_a_competing_bot_worker_on_the_same_store() {
    let directory = tempfile::tempdir().unwrap();
    let first = App::open(directory.path(), "https://sync.example.com").unwrap();
    assert!(App::open(directory.path(), "https://sync.example.com").is_err());
    drop(first);
    assert!(App::open(directory.path(), "https://sync.example.com").is_ok());
}

#[tokio::test]
async fn telegram_configuration_is_private_cas_persisted_and_legacy_compatible() {
    let (directory, state, app, _, device) = setup().await;
    let config = localshelld_telegram::Config {
        bot_token: "123456:abcdefghijklmnopqrstuvwxyz".into(),
        bot_username: "fixture_bot".into(),
        enabled: false,
        binding: None,
    };
    {
        let mut store = state.store.lock().await;
        let mut next = store.saved.clone();
        next.vault.telegram = Some(config.clone());
        store.commit(next).unwrap();
    }
    let command = json!({"expected_revision":0,"command":{"action":"status"}});
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/telegram",
            Some(command.clone()),
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, body) = call(
        &app,
        "POST",
        "/sync/v1/telegram",
        Some(command),
        Some(&device.access_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["configured"], true);
    assert!(!body.to_string().contains(&config.bot_token));
    let (_, old) = call(
        &app,
        "GET",
        "/sync/v1/vault",
        None,
        Some(&device.access_token),
    )
    .await;
    assert!(old.get("telegram").is_none());
    let mut headers = HeaderMap::new();
    headers.insert("x-localshelld-features", "telegram-v1".parse().unwrap());
    assert_eq!(
        wire_snapshot(&state.store.lock().await.saved.vault, &headers)["telegram"]["bot_token"],
        config.bot_token
    );
    let (status, _) = call(
        &app,
        "PUT",
        "/sync/v1/vault",
        Some(json!({"expected_revision":0,"credentials":{"KEY":"new"}})),
        Some(&device.access_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(state.store.lock().await.saved.vault.telegram == Some(config.clone()));
    let (status, _) = call(
        &app,
        "POST",
        "/sync/v1/telegram",
        Some(json!({"expected_revision":0,"command":{"action":"remove"}})),
        Some(&device.access_token),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    state.telegram.stop().await;
    drop(app);
    drop(state);
    assert!(
        !std::fs::read(directory.path().join("state.aes"))
            .unwrap()
            .windows(config.bot_token.len())
            .any(|v| v == config.bot_token.as_bytes())
    );
    let reopened = App::open(directory.path(), "https://sync.example.com").unwrap();
    assert!(reopened.store.lock().await.saved.vault.telegram == Some(config));
}

#[tokio::test]
async fn telegram_binding_capability_belongs_to_the_cloud_device_session() {
    let (_directory, state, app, browser, device) = setup().await;
    let config = localshelld_telegram::Config {
        bot_token: "123456:abcdefghijklmnopqrstuvwxyz".into(),
        bot_username: "fixture_bot".into(),
        enabled: false,
        binding: None,
    };
    {
        let mut store = state.store.lock().await;
        let mut next = store.saved.clone();
        next.vault.telegram = Some(config);
        store.commit(next).unwrap();
    }
    // Generate without starting an external Telegram poller in this test.
    let (_, body) = call(
        &app,
        "POST",
        "/sync/v1/telegram",
        Some(json!({"expected_revision":0,"command":{"action":"status"}})),
        Some(&device.access_token),
    )
    .await;
    assert_eq!(body["configured"], true);
    state.telegram.stop().await;
    let sid = {
        let store = state.store.lock().await;
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {}", device.access_token).parse().unwrap(),
        );
        state.authenticate(&store, &headers).unwrap().sid
    };
    // A fake code, even presented by another authenticated device, is rejected.
    for token in [&device.access_token, &browser.access_token] {
        assert_eq!(
            call(
                &app,
                "POST",
                "/sync/v1/telegram",
                Some(
                    json!({"expected_revision":0,"command":{"action":"bind_poll","code":"unknown"}})
                ),
                Some(token)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    assert!(!sid.is_empty());
    state.telegram.stop().await;
}

#[tokio::test]
async fn vault_requires_authentication_and_origin_is_enforced() {
    let (_dir, _state, app, _, device) = setup().await;
    for (method, path, body) in [
        ("GET", "/sync/v1/vault", None),
        (
            "PUT",
            "/sync/v1/vault",
            Some(json!({"expected_revision":0,"credentials":{}})),
        ),
        ("GET", "/sync/v1/sessions", None),
        (
            "POST",
            "/sync/v1/device/approve",
            Some(json!({"user_code":"wrong"})),
        ),
    ] {
        assert_eq!(
            call(&app, method, path, body, None).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        call(
            &app,
            "GET",
            "/sync/v1/sessions",
            None,
            Some(&device.access_token)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let response = app
        .oneshot(
            Request::get("/sync/v1/info")
                .header("origin", "https://attacker.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn bootstrap_requires_the_private_code_and_never_returns_it() {
    let directory = tempfile::tempdir().unwrap();
    let app = router(App::open(directory.path(), "https://sync.example.com").unwrap());
    let code = std::fs::read_to_string(directory.path().join("bootstrap-code")).unwrap();
    let (_, info) = call(&app, "GET", "/sync/v1/info", None, None).await;
    assert_eq!(info["initialized"], false);
    assert!(!info.to_string().contains(&code));
    let path = "/sync/v1/passkeys/register/start";
    assert_eq!(
        call(
            &app,
            "POST",
            path,
            Some(json!({"name":"first","bootstrap_code":"wrong"})),
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, challenge) = call(
        &app,
        "POST",
        path,
        Some(json!({"name":"first","bootstrap_code":code})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        challenge["options"]["publicKey"]["rp"]["id"],
        "sync.example.com"
    );
    assert!(!challenge.to_string().contains(&code));
}

#[tokio::test]
async fn concurrent_full_snapshot_writes_use_compare_and_swap_and_wake_readers() {
    let (_directory, state, app, _, device) = setup().await;
    let mut revision = state.revision.subscribe();
    let token = Some(device.access_token.as_str());
    let a = json!({"expected_revision":0,"credentials":{"TOKEN_A":"value-a"}});
    let b = json!({"expected_revision":0,"credentials":{"TOKEN_B":"value-b"}});
    let (a, b) = tokio::join!(
        call(&app, "PUT", "/sync/v1/vault", Some(a), token),
        call(&app, "PUT", "/sync/v1/vault", Some(b), token)
    );
    assert!(
        (a.0 == StatusCode::OK && b.0 == StatusCode::CONFLICT)
            || (b.0 == StatusCode::OK && a.0 == StatusCode::CONFLICT)
    );
    tokio::time::timeout(Duration::from_secs(1), revision.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*revision.borrow(), 1);
    let (_, snapshot) = call(&app, "GET", "/sync/v1/vault", None, token).await;
    assert_eq!(snapshot["revision"], 1);
    assert_eq!(snapshot["credentials"].as_object().unwrap().len(), 1);
    let (status, empty) = call(
        &app,
        "PUT",
        "/sync/v1/vault",
        Some(json!({"expected_revision":1,"credentials":{}})),
        token,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty["revision"], 2);
    assert_eq!(empty["credentials"], json!({}));
    assert_eq!(
        call(
            &app,
            "PUT",
            "/sync/v1/vault",
            Some(json!({"expected_revision":2,"credentials":{"lower":"no"}})),
            token
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn refresh_rotation_replay_revokes_the_entire_device_session() {
    let (_directory, _state, app, _, device) = setup().await;
    let body = json!({"refresh_token":device.refresh_token});
    let (status, renewed) = call(
        &app,
        "POST",
        "/sync/v1/session/refresh",
        Some(body.clone()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(renewed["refresh_token"], body["refresh_token"]);
    assert_eq!(
        call(
            &app,
            "GET",
            "/sync/v1/vault",
            None,
            renewed["access_token"].as_str()
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, "POST", "/sync/v1/session/refresh", Some(body), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &app,
            "GET",
            "/sync/v1/vault",
            None,
            renewed["access_token"].as_str()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &app,
            "GET",
            "/sync/v1/vault",
            None,
            Some(&device.access_token)
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn forged_refresh_does_not_revoke_a_known_session_and_jwt_audience_is_checked() {
    let (_directory, state, app, _, device) = setup().await;
    let sid = device.refresh_token.split_once('.').unwrap().0;
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/session/refresh",
            Some(json!({"refresh_token":format!("{sid}.invalid")})),
            None
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &app,
            "GET",
            "/sync/v1/vault",
            None,
            Some(&device.access_token)
        )
        .await
        .0,
        StatusCode::OK
    );
    let store = state.store.lock().await;
    let mut claims = Claims {
        sub: store.saved.user_id.to_string(),
        sid: sid.into(),
        iss: state.origin.clone(),
        aud: "wrong".into(),
        exp: Utc::now().timestamp() + 30,
        iat: Utc::now().timestamp(),
    };
    let key = EncodingKey::from_secret(store.saved.jwt_key.as_bytes());
    let wrong_audience =
        jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &key).unwrap();
    claims.aud = "localshelld-sync".into();
    claims.iss = "https://wrong.example".into();
    let wrong_issuer = jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &key).unwrap();
    claims.iss = state.origin.clone();
    claims.exp = Utc::now().timestamp() - 10;
    let expired = jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &key).unwrap();
    drop(store);
    for token in [wrong_audience, wrong_issuer, expired] {
        assert_eq!(
            call(&app, "GET", "/sync/v1/vault", None, Some(&token))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
}

#[tokio::test]
async fn device_approval_requires_a_browser_and_poll_is_single_use() {
    let (_directory, state, app, browser, device) = setup().await;
    let (status, started) = call(
        &app,
        "POST",
        "/sync/v1/device/start",
        Some(json!({"name":"Laptop"})),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let user_code = json!({"user_code":started["user_code"]});
    let code = json!({"device_code":started["device_code"]});
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/device/poll",
            Some(code.clone()),
            None
        )
        .await
        .0,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/device/approve",
            Some(user_code.clone()),
            Some(&device.access_token)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/device/approve",
            Some(user_code),
            Some(&browser.access_token)
        )
        .await
        .0,
        StatusCode::OK
    );
    for pending in state.pending.lock().await.devices.values_mut() {
        pending.last_poll = 0;
    }
    let (status, tokens) = call(
        &app,
        "POST",
        "/sync/v1/device/poll",
        Some(code.clone()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(tokens["access_token"].is_string());
    assert_eq!(
        call(&app, "POST", "/sync/v1/device/poll", Some(code), None)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &app,
            "GET",
            "/sync/v1/vault",
            None,
            tokens["access_token"].as_str()
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn encrypted_state_survives_restart_and_only_one_writer_can_open_it() {
    let (directory, state, app, _, device) = setup().await;
    let secret = "fixture-secret-never-plaintext";
    assert_eq!(
        call(
            &app,
            "PUT",
            "/sync/v1/vault",
            Some(json!({"expected_revision":0,"credentials":{"TOKEN":secret}})),
            Some(&device.access_token)
        )
        .await
        .0,
        StatusCode::OK
    );
    let encrypted = std::fs::read(directory.path().join("state.aes")).unwrap();
    assert!(
        !encrypted
            .windows(secret.len())
            .any(|bytes| bytes == secret.as_bytes())
    );
    assert!(App::open(directory.path(), "https://sync.example.com").is_err());
    drop(app);
    drop(state);
    let restarted = router(App::open(directory.path(), "https://sync.example.com").unwrap());
    let (status, snapshot) = call(
        &restarted,
        "GET",
        "/sync/v1/vault",
        None,
        Some(&device.access_token),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["credentials"]["TOKEN"], secret);
    assert_eq!(snapshot["revision"], 1);
}

#[tokio::test]
async fn long_poll_returns_after_write_and_rechecks_revocation() {
    let (_directory, _state, app, browser, device) = setup().await;
    let polling = {
        let app = app.clone();
        let token = device.access_token.clone();
        tokio::spawn(async move {
            call(&app, "GET", "/sync/v1/events?after=0", None, Some(&token)).await
        })
    };
    tokio::task::yield_now().await;
    let sid = device.refresh_token.split_once('.').unwrap().0;
    assert_eq!(
        call(
            &app,
            "POST",
            "/sync/v1/session/revoke",
            Some(json!({"id":sid})),
            Some(&browser.access_token)
        )
        .await
        .0,
        StatusCode::OK
    );
    let response = tokio::time::timeout(Duration::from_secs(2), polling)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.0, StatusCode::UNAUTHORIZED);
}

#[test]
fn production_origin_requires_https_and_no_extra_url_parts() {
    for origin in [
        "http://sync.example.com",
        "https://sync.example.com/path",
        "https://user:password@sync.example.com",
        "https://sync.example.com?token=hello",
    ] {
        let directory = tempfile::tempdir().unwrap();
        assert!(App::open(directory.path(), origin).is_err());
    }
}
