use super::*;
use axum::{body::Body, http::Request};
use tower::ServiceExt;

fn fixture() -> App {
    Arc::new(Inner {
        token: "fixture-token".into(),
        authority: "127.0.0.1:31991".into(),
        paths: MachinePaths {
            program: PathBuf::from("fixture-program"),
            data: PathBuf::from("fixture-data"),
            config: PathBuf::from("fixture-data/system/config.toml"),
        },
        executable: PathBuf::from("fixture.exe"),
        binary_hash: "a".repeat(64),
        user_config: PathBuf::from("fixture-user.toml"),
        owner: "fixture".into(),
        busy: AtomicBool::new(false),
        job: Mutex::new(Job::default()),
        shutdown: tokio::sync::Notify::new(),
    })
}

#[tokio::test]
async fn maintenance_requires_exact_host_origin_and_launch_token() {
    for (host, origin, auth, expected) in [
        (
            "evil.example:31991",
            "http://127.0.0.1:31991",
            "Bearer fixture-token",
            StatusCode::FORBIDDEN,
        ),
        (
            "127.0.0.1:31991",
            "https://evil.example",
            "Bearer fixture-token",
            StatusCode::FORBIDDEN,
        ),
        (
            "127.0.0.1:31991",
            "null",
            "Bearer fixture-token",
            StatusCode::FORBIDDEN,
        ),
        (
            "127.0.0.1:31991",
            "http://127.0.0.1:31991",
            "",
            StatusCode::UNAUTHORIZED,
        ),
        (
            "127.0.0.1:31991",
            "http://127.0.0.1:31991",
            "Bearer wrong-token",
            StatusCode::UNAUTHORIZED,
        ),
        (
            "127.0.0.1:31991",
            "http://127.0.0.1:31991",
            "Bearer fixture-token",
            StatusCode::OK,
        ),
    ] {
        let response = router(fixture())
            .oneshot(
                Request::post("/setup/v1/close")
                    .header("host", host)
                    .header("origin", origin)
                    .header("authorization", auth)
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
}

#[tokio::test]
async fn setup_surface_is_bounded_and_has_no_shell_or_daemon_routes() {
    for path in ["/v1/run", "/mcp", "/", "/index.html", "/sync.html"] {
        let response = router(fixture())
            .oneshot(
                Request::get(path)
                    .header("host", "127.0.0.1:31991")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    let state = fixture();
    let response = router(state.clone())
        .oneshot(
            Request::post("/setup/v1/install")
                .header("host", "127.0.0.1:31991")
                .header("authorization", "Bearer fixture-token")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    "{{\"password\":\"{}\"}}",
                    "x".repeat(9000)
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!state.busy.load(Ordering::SeqCst));
    let response = router(state.clone())
        .oneshot(
            Request::post("/setup/v1/install")
                .header("host", "127.0.0.1:31991")
                .header("authorization", "Bearer fixture-token")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"password":"test","command":"arbitrary"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!state.busy.load(Ordering::SeqCst));
}

#[tokio::test]
async fn active_installation_cannot_be_closed_or_started_twice() {
    let state = fixture();
    state.busy.store(true, Ordering::SeqCst);
    assert!(start_job(state.clone(), Operation::Install, Zeroizing::new(None)).is_err());
    let response = router(state)
        .oneshot(
            Request::post("/setup/v1/close")
                .header("host", "127.0.0.1:31991")
                .header("authorization", "Bearer fixture-token")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[test]
fn plan_never_accepts_arbitrary_programs_or_installation_targets() {
    let value = json!({"protocol":1,"operation":"install","binary_hash":"a".repeat(64),"password_hash":null,"owner":"fixture","user_config":"user.toml","system_bind":"127.0.0.1:32119"});
    assert!(serde_json::from_value::<Plan>(value.clone()).is_ok());
    for key in ["command", "program", "system_config", "download_url"] {
        let mut malicious = value.clone();
        malicious[key] = json!("arbitrary");
        assert!(serde_json::from_value::<Plan>(malicious).is_err());
    }
}
