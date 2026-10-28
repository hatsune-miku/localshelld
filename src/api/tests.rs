use super::*;
use crate::{
    auth::AuthManager,
    config::Config,
    consent::{Choice, ConsentBroker, Decision, DesktopEvent},
    server::router,
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tower::ServiceExt;

fn user_state() -> (AppState, String) {
    let mut auth = AuthManager::new("unused".into(), None);
    let token = auth.issue_token(None).unwrap().0;
    let state = AppState::user(
        Config {
            bind: "127.0.0.1:32120".parse().unwrap(),
            ..Config::default()
        },
        auth,
    )
    .unwrap();
    (state, token)
}

fn consent_state(choice: Choice, remember: bool) -> (AppState, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    let captured = count.clone();
    let broker = ConsentBroker::new(move |event| {
        if let DesktopEvent::Show(prompt, reply) = event {
            assert_eq!(prompt.request_reason, "fixture <reason>\nsecond line");
            captured.fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Decision {
                choice,
                remember,
                btw: "fixture note".into(),
            });
        }
        Ok(())
    });
    (user_state().0.with_consent(broker), count)
}

fn consent_arguments() -> Value {
    json!({"agent_name":"fixture","request_reason":"fixture <reason>\nsecond line","confirm_text":"OK"})
}

async fn request(
    app: &Router,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
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

async fn http(app: &Router, operation: Operation, arguments: Value) -> (StatusCode, Value) {
    if matches!(operation, Operation::SecretsList) {
        request(app, "GET", operation.path(), None).await
    } else {
        request(app, "POST", operation.path(), Some(arguments)).await
    }
}

async fn mcp(app: &Router, name: &str, arguments: Value) -> Value {
    request(app, "POST", "/mcp", Some(json!({
        "jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}
    }))).await.1
}

fn run_arguments(handle: &str, command: &str) -> Value {
    let mut args = crate::execution::execution_argument(PrivilegeLevel::CurrentUser);
    args["handle"] = json!(handle);
    args["command"] = json!(command);
    args
}

#[tokio::test]
async fn llms_is_public_plain_text_with_live_schemas_on_both_daemons() {
    for state in [
        user_state().0,
        AppState::new(
            Config::default(),
            AuthManager::new("private-verifier-fixture".into(), None),
        ),
    ] {
        state
            .put_secret("PRIVATE_DOC_FIXTURE", "private-doc-secret-value")
            .await
            .unwrap();
        let token = state.auth.lock().await.issue_token(None).unwrap().0;
        let app = router(state.clone());
        // No Authorization, Cookie, MCP session header, token, or handle.
        let response = app
            .clone()
            .oneshot(
                Request::get("/llms.txt")
                    .header("host", state.config.bind.to_string())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["content-type"],
            "text/plain; charset=utf-8"
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.starts_with("# trustd API\n"));
        assert!(text.contains(&format!("Base URL: http://{}\n", state.config.bind)));
        let mode = serde_json::to_value(state.mode).unwrap();
        assert!(text.contains(&format!("Daemon: {}\n", mode.as_str().unwrap())));
        assert!(text.contains(&format!("Platform: {}\n", std::env::consts::OS)));
        assert!(text.contains(&format!("Version: {}\n", crate::VERSION)));
        assert!(text.contains(&format!("Commit: {}\n", crate::COMMIT)));
        for secret in [
            "PRIVATE_DOC_FIXTURE",
            "private-doc-secret-value",
            "private-verifier-fixture",
            token.as_str(),
        ] {
            assert!(!text.contains(secret));
        }
        let operations_text = text.split("## Operations\n").nth(1).unwrap();
        let published_schemas: Vec<Value> = operations_text
            .split("```json\n")
            .skip(1)
            .map(|block| serde_json::from_str(block.split("\n```").next().unwrap()).unwrap())
            .collect();
        let (_, capabilities) = request(&app, "GET", "/v1/capabilities", None).await;
        let operations = capabilities["operations"].as_array().unwrap();
        assert_eq!(published_schemas.len(), operations.len() * 2);
        for (operation, schemas) in operations.iter().zip(published_schemas.chunks_exact(2)) {
            assert!(operations_text.contains(&format!(
                "### {} {}\n\nMCP tool: `{}`\n",
                operation["method"].as_str().unwrap(),
                operation["path"].as_str().unwrap(),
                operation["name"].as_str().unwrap(),
            )));
            assert_eq!(schemas[0], operation["inputSchema"]);
            assert_eq!(schemas[1], operation["outputSchema"]);
        }
        // Publishing docs must not open protected execution/management routes.
        assert_eq!(
            http(&app, Operation::Run, run_arguments("invalid", "echo never"))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(&app, "GET", "/v1/admin/settings", None).await.0,
            StatusCode::UNAUTHORIZED
        );
        let head = app
            .clone()
            .oneshot(Request::head("/llms.txt").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()["content-type"], "text/plain; charset=utf-8");
        assert!(
            head.into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );
        assert_eq!(
            request(&app, "POST", "/llms.txt", None).await.0,
            StatusCode::METHOD_NOT_ALLOWED
        );
        state.shutdown().await;
    }
}

#[tokio::test]
async fn capabilities_publish_all_mcp_schemas_and_registered_http_operations() {
    let (state, _) = user_state();
    let app = router(state.clone());
    let (status, capabilities) = request(&app, "GET", "/v1/capabilities", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(capabilities["version"], crate::VERSION);
    assert_eq!(capabilities["commit"], crate::COMMIT);
    assert_eq!(capabilities["daemon"], "user");
    assert_eq!(capabilities["interactive_timeout_ms"], 60_000);
    assert_eq!(capabilities["cancellation"]["disconnect_cancels"], false);
    let tools = tool_definitions();
    let operations = capabilities["operations"].as_array().unwrap();
    assert_eq!(operations.len(), tools.as_array().unwrap().len());
    for (entry, tool) in operations.iter().zip(tools.as_array().unwrap()) {
        let mut stripped = entry.clone();
        stripped.as_object_mut().unwrap().remove("method");
        stripped.as_object_mut().unwrap().remove("path");
        assert_eq!(&stripped, tool);
        let (status, _) = request(
            &app,
            entry["method"].as_str().unwrap(),
            entry["path"].as_str().unwrap(),
            Some(json!({})),
        )
        .await;
        assert_ne!(status, StatusCode::NOT_FOUND);
        assert_ne!(status, StatusCode::METHOD_NOT_ALLOWED);
    }
    state.shutdown().await;
}

#[tokio::test]
async fn preauthorization_secrets_execution_and_lifecycle_are_interoperable() {
    let (state, token) = user_state();
    state
        .put_secret("TEST_API_SECRET", "fixture-secret-value")
        .await
        .unwrap();
    let app = router(state.clone());
    let (status, keys) = http(&app, Operation::SecretsList, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        keys,
        mcp(&app, "secrets_list", json!({})).await["result"]["structuredContent"]
    );
    assert_eq!(keys, json!({"keys":["TEST_API_SECRET"]}));

    let arguments = json!({"token":token,"confirm_text":"OK","secret_keys":["TEST_API_SECRET"]});
    let (status, entered) = http(&app, Operation::EnterPreauthorized, arguments.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(entered["reused"], false);
    assert!(!entered.to_string().contains("fixture-secret-value"));
    let (_, reused) = http(&app, Operation::EnterPreauthorized, arguments.clone()).await;
    assert_eq!(
        reused,
        mcp(&app, "enter_preauthorized", arguments).await["result"]["structuredContent"]
    );
    assert_eq!(reused["reused"], true);
    let handle = entered["handle"].as_str().unwrap();
    let (_, legacy) = request(
        &app,
        "POST",
        "/v1/sessions/enter",
        Some(json!({"token":token})),
    )
    .await;
    assert_eq!(legacy["handle"], handle);

    let command = if cfg!(windows) {
        "[Console]::Out.Write([Console]::In.ReadToEnd()); [Console]::Error.Write('fixture stderr')"
    } else {
        "cat; printf 'fixture stderr' >&2"
    };
    let mut args = run_arguments(handle, command);
    args["stdin"] = json!("fixture stdin\n");
    args["timeout_seconds"] = json!(10);
    let (status, mut output) = http(&app, Operation::Run, args.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(output["stdout"], "fixture stdin\n");
    assert_eq!(output["stderr"], "fixture stderr");
    let mut rpc_output = mcp(&app, "run", args).await["result"]["structuredContent"].clone();
    output.as_object_mut().unwrap().remove("duration_ms");
    rpc_output.as_object_mut().unwrap().remove("duration_ms");
    assert_eq!(output, rpc_output);

    let elevated = crate::execution::execution_argument(PrivilegeLevel::privileged());
    let mut forbidden = run_arguments(handle, "echo must-not-run");
    forbidden
        .as_object_mut()
        .unwrap()
        .extend(elevated.as_object().unwrap().clone());
    assert_eq!(
        http(&app, Operation::Run, forbidden.clone()).await.0,
        StatusCode::FORBIDDEN
    );
    assert!(mcp(&app, "run", forbidden).await.get("error").is_some());

    assert_eq!(
        mcp(&app, "destroy", json!({"handle":handle})).await["result"]["structuredContent"],
        json!({"destroyed":true})
    );
    assert_eq!(
        http(&app, Operation::Run, run_arguments(handle, "echo never"))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (_, next) = http(
        &app,
        Operation::EnterPreauthorized,
        json!({"token":token,"confirm_text":"OK"}),
    )
    .await;
    assert_ne!(next["handle"], handle);
    assert_eq!(
        http(&app, Operation::Revoke, json!({"token":token}))
            .await
            .1,
        json!({"revoked":true})
    );
    assert!(
        mcp(
            &app,
            "run",
            run_arguments(next["handle"].as_str().unwrap(), "echo never")
        )
        .await
        .get("error")
        .is_some()
    );
    state.shutdown().await;
}

#[tokio::test]
async fn plaintext_reads_share_scoped_authorization_and_snapshots_across_transports() {
    for state in [
        user_state().0,
        AppState::new(Config::default(), AuthManager::new("unused".into(), None)),
    ] {
        let token = state.auth.lock().await.issue_token(None).unwrap().0;
        let value = "fixture secret \" ' \\ \r\n中文 $env:NOT_CODE";
        state.put_secret("TEST_READ_SECRET", value).await.unwrap();
        state
            .put_secret("TEST_OTHER_SECRET", "not-authorized")
            .await
            .unwrap();
        let app = router(state.clone());
        let (_, entered) = http(
            &app,
            Operation::EnterPreauthorized,
            json!({
                "token":token,"confirm_text":"OK","secret_keys":["TEST_READ_SECRET"]
            }),
        )
        .await;
        assert!(!entered.to_string().contains(value));
        let handle = entered["handle"].as_str().unwrap();
        let arguments = json!({"handle":handle,"key":"TEST_READ_SECRET"});
        let (status, revealed) = http(&app, Operation::SecretsGet, arguments.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(revealed, json!({"key":"TEST_READ_SECRET","value":value}));
        assert_eq!(
            revealed,
            mcp(&app, "secrets_get", arguments.clone()).await["result"]["structuredContent"]
        );
        for key in ["TEST_OTHER_SECRET", "MISSING_SECRET"] {
            let denied = json!({"handle":handle,"key":key});
            let (status, error) = http(&app, Operation::SecretsGet, denied.clone()).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(
                error,
                json!({"error":"secret key is not authorized for this session"})
            );
            assert_eq!(
                error["error"],
                mcp(&app, "secrets_get", denied).await["error"]["message"]
            );
        }
        // No implicit vault access, even with a valid execution session.
        let other_token = state.auth.lock().await.issue_token(None).unwrap().0;
        let other = state.enter(&other_token).await.unwrap();
        assert_eq!(
            http(
                &app,
                Operation::SecretsGet,
                json!({"handle":other.handle,"key":"TEST_READ_SECRET"})
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            http(
                &app,
                Operation::SecretsGet,
                json!({"handle":"invalid","key":"TEST_READ_SECRET"})
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            request(&app, "GET", Operation::SecretsGet.path(), None)
                .await
                .0,
            StatusCode::METHOD_NOT_ALLOWED
        );
        let response = app
            .clone()
            .oneshot(
                Request::post(Operation::SecretsGet.path())
                    .header("content-type", "application/json")
                    .body(Body::from(arguments.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["content-type"], "application/json");
        // Reads reveal the approved snapshot, not a newly rotated/deleted vault value.
        state
            .put_secret("TEST_READ_SECRET", "rotated-value")
            .await
            .unwrap();
        state.remove_secret("TEST_READ_SECRET").await.unwrap();
        assert_eq!(
            http(&app, Operation::SecretsGet, arguments.clone()).await.1,
            revealed
        );
        state.destroy_session(handle).await.unwrap();
        assert_eq!(
            http(&app, Operation::SecretsGet, arguments).await.0,
            StatusCode::UNAUTHORIZED
        );
        state
            .put_secret("TEST_READ_SECRET", "fresh-value")
            .await
            .unwrap();
        let fresh = state
            .enter_with_secrets(&token, &["TEST_READ_SECRET".into()])
            .await
            .unwrap();
        let arguments = json!({"handle":fresh.handle,"key":"TEST_READ_SECRET"});
        assert_eq!(
            http(&app, Operation::SecretsGet, arguments.clone()).await.1["value"],
            "fresh-value"
        );
        state.revoke_token(&token).await.unwrap();
        assert_eq!(
            http(&app, Operation::SecretsGet, arguments.clone()).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert!(
            mcp(&app, "secrets_get", arguments)
                .await
                .get("error")
                .is_some()
        );
        state.shutdown().await;
    }
}

#[tokio::test]
async fn http_interactive_approval_uses_the_same_owner_decision_and_secret_selection() {
    let (state, calls) = consent_state(Choice::Accept, true);
    state
        .put_secret("TEST_API_SECRET", "fixture-private")
        .await
        .unwrap();
    let app = router(state.clone());
    let mut args = consent_arguments();
    args["secret_keys"] = json!(["TEST_API_SECRET"]);
    let (status, result) = http(&app, Operation::EnterInteractive, args.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["status"], "authorized");
    assert_eq!(result["btw"], "fixture note");
    assert_eq!(result["secret_keys"], json!(["TEST_API_SECRET"]));
    assert!(result.get("token").is_none());
    assert!(!result.to_string().contains("fixture-private"));
    assert_eq!(
        mcp(
            &app,
            "secrets_get",
            json!({"handle":result["handle"],"key":"TEST_API_SECRET"})
        )
        .await["result"]["structuredContent"],
        json!({"key":"TEST_API_SECRET","value":"fixture-private"})
    );
    let rpc = mcp(
        &app,
        "run",
        run_arguments(result["handle"].as_str().unwrap(), "echo http-authorized"),
    )
    .await;
    assert!(
        rpc["result"]["structuredContent"]["stdout"]
            .as_str()
            .unwrap()
            .contains("http-authorized")
    );
    let remembered = mcp(&app, "enter_interactive", args).await;
    assert_eq!(
        remembered["result"]["structuredContent"]["status"],
        "authorized"
    );
    assert_eq!(remembered["result"]["structuredContent"]["btw"], "");
    assert!(
        remembered["result"]["structuredContent"]
            .get("automatic")
            .is_none()
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    state.shutdown().await;
}

#[tokio::test]
async fn http_denial_and_remembered_denial_are_not_transport_errors() {
    let (state, calls) = consent_state(Choice::Deny, true);
    let app = router(state.clone());
    let (status, result) = http(&app, Operation::EnterInteractive, consent_arguments()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        result,
        json!({"status":"denied","automatic":false,"btw":"fixture note"})
    );
    let (_, remembered) = http(&app, Operation::EnterInteractive, consent_arguments()).await;
    let rpc = mcp(&app, "enter_interactive", consent_arguments()).await;
    assert_eq!(remembered, rpc["result"]["structuredContent"]);
    assert_eq!(remembered["automatic"], true);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    state.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn http_interactive_ignore_preserves_the_fixed_deadline_and_note() {
    let (state, _) = consent_state(Choice::Ignore, true);
    let app = router(state.clone());
    let started = tokio::time::Instant::now();
    let (status, result) = http(&app, Operation::EnterInteractive, consent_arguments()).await;
    assert_eq!(started.elapsed(), Duration::from_secs(60));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        result,
        json!({"status":"timed_out","timeout_ms":60000,"btw":"fixture note"})
    );
    assert!(state.settings.lock().await.saved.remembered.is_empty());
    state.shutdown().await;
}

#[tokio::test]
async fn both_transports_reject_invalid_arguments_before_authorization_or_execution() {
    let (state, calls) = consent_state(Choice::Accept, false);
    let app = router(state.clone());
    let cases = [
        ("secrets_get", json!({})),
        ("secrets_get", json!({"handle":"fixture-private"})),
        ("secrets_get", json!({"handle":123,"key":"TEST_SECRET"})),
        ("secrets_get", json!({"handle":"fixture-private","key":123})),
        (
            "secrets_get",
            json!({"handle":"fixture-private","key":"lowercase"}),
        ),
        (
            "secrets_get",
            json!({"handle":"fixture-private","key":"TEST_SECRET","token":"fixture-private"}),
        ),
        ("enter_interactive", json!(null)),
        ("enter_interactive", json!([])),
        ("enter_interactive", json!({})),
        (
            "enter_interactive",
            json!({"agent_name":"fixture","request_reason":"reason","confirm_text":"fixture-private"}),
        ),
        (
            "enter_interactive",
            json!({"agent_name":"fixture","request_reason":"reason","confirm_text":"OK","choice":"accept"}),
        ),
        (
            "enter_interactive",
            json!({"agent_name":"","request_reason":"reason","confirm_text":"OK"}),
        ),
        (
            "enter_interactive",
            json!({"agent_name":"fixture","request_reason":"reason","confirm_text":"OK","secret_keys":null}),
        ),
        (
            "enter_interactive",
            json!({"agent_name":"fixture","request_reason":"reason","confirm_text":"OK","secret_keys":["lowercase"]}),
        ),
        ("enter_preauthorized", json!({"token":"fixture-private"})),
        (
            "enter_preauthorized",
            json!({"token":123,"confirm_text":"OK"}),
        ),
        (
            "run",
            json!({"handle":"fixture-private","command":"echo never"}),
        ),
        (
            "destroy",
            json!({"handle":"fixture-private","fixture-private":"unexpected"}),
        ),
        ("revoke", json!({"token":123})),
    ];
    for (name, arguments) in cases {
        let (status, error) =
            http(&app, Operation::from_name(name).unwrap(), arguments.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(
            error["error"],
            mcp(&app, name, arguments).await["error"]["message"]
        );
        assert!(!error.to_string().contains("fixture-private"));
    }
    for (field, value) in [
        ("stdin", Value::Null),
        ("timeout_seconds", json!(0)),
        ("timeout_seconds", json!(-1)),
        ("timeout_seconds", json!("10")),
    ] {
        let mut arguments = run_arguments("invalid", "echo never");
        arguments[field] = value;
        let (status, error) = http(&app, Operation::Run, arguments.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            error["error"],
            mcp(&app, "run", arguments).await["error"]["message"]
        );
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    state.shutdown().await;
}

#[tokio::test]
async fn new_http_routes_keep_the_loopback_browser_boundary() {
    let (state, calls) = consent_state(Choice::Accept, false);
    let app = router(state.clone());
    for path in [
        "/llms.txt",
        "/v1/capabilities",
        Operation::SecretsGet.path(),
        Operation::EnterInteractive.path(),
        Operation::EnterPreauthorized.path(),
    ] {
        for (header, value) in [
            ("origin", "https://evil.example"),
            ("origin", "http://localhost:32119"),
            ("host", "evil.example:32120"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(if matches!(path, "/v1/capabilities" | "/llms.txt") {
                            "GET"
                        } else {
                            "POST"
                        })
                        .uri(path)
                        .header("content-type", "application/json")
                        .header(header, value)
                        .body(Body::from(consent_arguments().to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }
    let response = app
        .oneshot(
            Request::get("/v1/capabilities")
                .header("host", "localhost:32120")
                .header("origin", "http://localhost:32120")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    state.shutdown().await;
}

#[tokio::test]
async fn http_destroy_cancels_an_inflight_run_and_invalidates_its_handle() {
    let (state, token) = user_state();
    let app = router(state.clone());
    let (_, entered) = http(
        &app,
        Operation::EnterPreauthorized,
        json!({"token":token,"confirm_text":"OK"}),
    )
    .await;
    let handle = entered["handle"].as_str().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("started");
    let path = marker.to_string_lossy();
    let command = if cfg!(windows) {
        format!(
            "[System.IO.File]::WriteAllText('{}','started'); Start-Sleep -Seconds 30",
            path.replace('\'', "''")
        )
    } else {
        format!(
            "printf started > '{}'; sleep 30",
            path.replace('\'', "'\\''")
        )
    };
    let running = {
        let app = app.clone();
        let args = run_arguments(handle, &command);
        tokio::spawn(async move { http(&app, Operation::Run, args).await })
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        http(&app, Operation::Destroy, json!({"handle":"not-the-handle"}))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert!(!running.is_finished());
    assert_eq!(
        http(&app, Operation::Destroy, json!({"handle":handle}))
            .await
            .1,
        json!({"destroyed":true})
    );
    let (status, result) = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["session_ended_reason"], "cancelled");
    assert_eq!(result["session_alive"], false);
    assert_eq!(
        http(&app, Operation::Run, run_arguments(handle, "echo never"))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    state.shutdown().await;
}
