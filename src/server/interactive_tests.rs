use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::json;

use super::*;
use crate::consent::DesktopEvent;

fn desktop_state(choice: Choice, remember: bool) -> (AppState, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let broker = ConsentBroker::new(move |event| {
        if let DesktopEvent::Show(prompt, reply) = event {
            assert_eq!(prompt.request_reason, "exact <reason>\nsecond line");
            count.fetch_add(1, Ordering::Relaxed);
            let _ = reply.send(Decision {
                choice,
                remember,
                btw: "user note".into(),
            });
        }
        Ok(())
    });
    let state = AppState::user(
        Config {
            bind: "127.0.0.1:32120".parse().unwrap(),
            ..Config::default()
        },
        AuthManager::new("unused".into(), None),
    )
    .unwrap()
    .with_consent(broker);
    (state, calls)
}

#[tokio::test]
async fn accepted_and_remembered_results_are_neutral_and_executable() {
    let (state, calls) = desktop_state(Choice::Accept, true);
    let first = state
        .enter_interactive("agent", "exact <reason>\nsecond line")
        .await
        .unwrap();
    assert_eq!(first["status"], "authorized");
    assert_eq!(first["btw"], "user note");
    assert!(first.get("token").is_none());
    assert!(first.get("automatic").is_none());
    let handle = first["handle"].as_str().unwrap();
    assert!(
        state
            .run(handle, "echo consent-ok", false, None)
            .await
            .unwrap()
            .output
            .contains("consent-ok")
    );
    let second = state
        .enter_interactive("agent", "exact <reason>\nsecond line")
        .await
        .unwrap();
    assert_eq!(second["status"], "authorized");
    assert_eq!(second["btw"], "");
    assert_eq!(second["message"], first["message"]);
    assert!(second.get("automatic").is_none());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    state.settings.lock().await.saved.remembered.clear();
    state
        .enter_interactive("agent", "exact <reason>\nsecond line")
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    state.shutdown().await;
}

#[tokio::test]
async fn remembered_denial_is_explicit_and_never_creates_a_token() {
    let (state, calls) = desktop_state(Choice::Deny, true);
    let first = state
        .enter_interactive("agent", "exact <reason>\nsecond line")
        .await
        .unwrap();
    let second = state
        .enter_interactive("agent", "exact <reason>\nsecond line")
        .await
        .unwrap();
    assert_eq!(first["automatic"], false);
    assert_eq!(first["btw"], "user note");
    assert_eq!(second["automatic"], true);
    assert_eq!(second["status"], "denied");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert!(state.auth.lock().await.list().is_empty());
    assert!(state.sessions.lock().await.by_handle.is_empty());
}

#[tokio::test(start_paused = true)]
async fn ignoring_waits_exactly_sixty_seconds_and_does_not_remember() {
    let (state, _) = desktop_state(Choice::Ignore, true);
    let started = tokio::time::Instant::now();
    let result = state
        .enter_interactive("agent", "exact <reason>\nsecond line")
        .await
        .unwrap();
    assert_eq!(started.elapsed(), CONSENT_TIMEOUT);
    assert_eq!(
        result,
        json!({"status":"timed_out","timeout_ms":60_000,"btw":"user note"})
    );
    assert!(state.settings.lock().await.saved.remembered.is_empty());
    assert!(state.auth.lock().await.list().is_empty());
}

#[tokio::test]
async fn browser_cannot_submit_desktop_approval_and_mcp_checks_required_fields() {
    let (state, calls) = desktop_state(Choice::Accept, true);
    let app = router(state);
    let (status, _) =
        super::tests::request(&app, "/v1/consent/approve", json!({"allow":true})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for args in [
        json!({}),
        json!({"agent_name":"agent","request_reason":"reason","confirm_text":"NO"}),
        json!({"agent_name":"","request_reason":"reason","confirm_text":"OK"}),
    ] {
        let (_, result) = super::tests::request(&app, "/mcp", json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"localshelld_enter_interactive","arguments":args}})).await;
        assert!(result.get("error").is_some());
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn remembered_shell_approval_does_not_silently_authorize_new_credentials() {
    let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = prompts.clone();
    let (mut state, _) = desktop_state(Choice::Accept, true);
    state.consent = Some(ConsentBroker::new(move |event| {
        if let DesktopEvent::Show(prompt, reply) = event {
            captured.lock().unwrap().push(prompt.secret_keys);
            let _ = reply.send(Decision {
                choice: Choice::Accept,
                remember: true,
                btw: String::new(),
            });
        }
        Ok(())
    }));
    state
        .settings
        .lock()
        .await
        .saved
        .remembered
        .insert("agent".into(), true);
    state
        .put_secret("JENKINS_CI_TOKEN", "fixture-jenkins")
        .await
        .unwrap();
    state
        .put_secret("OPENAI_API_KEY", "fixture-openai")
        .await
        .unwrap();
    let first = state
        .enter_interactive_with_secrets("agent", "build", &["JENKINS_CI_TOKEN".into()])
        .await
        .unwrap();
    assert_eq!(first["secret_keys"], json!(["JENKINS_CI_TOKEN"]));
    assert!(!first.to_string().contains("fixture-jenkins"));
    assert_eq!(prompts.lock().unwrap().len(), 1);
    state
        .enter_interactive_with_secrets("agent", "build again", &["JENKINS_CI_TOKEN".into()])
        .await
        .unwrap();
    assert_eq!(prompts.lock().unwrap().len(), 1);
    state
        .enter_interactive_with_secrets(
            "agent",
            "also use OpenAI",
            &["OPENAI_API_KEY".into(), "JENKINS_CI_TOKEN".into()],
        )
        .await
        .unwrap();
    assert_eq!(
        *prompts.lock().unwrap(),
        vec![
            vec!["JENKINS_CI_TOKEN"],
            vec!["JENKINS_CI_TOKEN", "OPENAI_API_KEY"]
        ]
    );
    let count = state.auth.lock().await.list().len();
    assert!(
        state
            .enter_interactive_with_secrets("agent", "unknown", &["MISSING".into()])
            .await
            .is_err()
    );
    assert_eq!(state.auth.lock().await.list().len(), count);
    assert_eq!(prompts.lock().unwrap().len(), 2);
    state.shutdown().await;
}

#[tokio::test]
async fn denied_secret_request_creates_neither_token_nor_shell() {
    let (state, _) = desktop_state(Choice::Deny, false);
    state
        .put_secret("JENKINS_CI_TOKEN", "fixture-only")
        .await
        .unwrap();
    let result = state
        .enter_interactive_with_secrets(
            "agent",
            "exact <reason>\nsecond line",
            &["JENKINS_CI_TOKEN".into()],
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "denied");
    assert!(state.auth.lock().await.list().is_empty());
    assert!(state.sessions.lock().await.by_handle.is_empty());
}

#[tokio::test]
async fn paired_privileged_front_door_runs_ordinary_commands_only_in_user_shell() {
    let root_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let user_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root_address = root_listener.local_addr().unwrap();
    let user_address = user_listener.local_addr().unwrap();
    let mut auth = AuthManager::new("unused".into(), None);
    let (token, _) = auth.issue_token(None).unwrap();
    let root = AppState::new(
        Config {
            bind: root_address,
            ..Config::default()
        },
        auth,
    );
    let user = AppState::user(
        Config {
            bind: user_address,
            privileged_daemon: root_address,
            ..Config::default()
        },
        AuthManager::new("unused".into(), None),
    )
    .unwrap();
    let key = bridge::generate_key();
    root.settings.lock().await.saved.pairing = Some(Pairing {
        key: key.clone(),
        peer: user_address,
    });
    user.settings.lock().await.saved.pairing = Some(Pairing {
        key,
        peer: root_address,
    });
    let root_app = router(root.clone());
    let user_app = router(user.clone());
    let root_server =
        tokio::spawn(async move { axum::serve(root_listener, root_app).await.unwrap() });
    let user_server =
        tokio::spawn(async move { axum::serve(user_listener, user_app).await.unwrap() });
    user.put_secret("TEST_SHARED_SECRET", "fixture-shared-snapshot")
        .await
        .unwrap();
    assert_eq!(
        root.secret_keys().await.unwrap(),
        user.secret_keys().await.unwrap()
    );
    root.put_secret("TEST_SECOND_SECRET", "fixture-second")
        .await
        .unwrap();
    assert!(
        user.secret_keys()
            .await
            .unwrap()
            .contains(&"TEST_SECOND_SECRET".into())
    );
    let entered = root
        .enter_with_secrets(&token, &["TEST_SHARED_SECRET".into()])
        .await
        .unwrap();
    // Rotate before lazily attaching the ordinary-user shell: every identity
    // must still inherit the original authorization's snapshot.
    user.put_secret("TEST_SHARED_SECRET", "rotated-after-enter")
        .await
        .unwrap();
    let attached = user
        .enter_with_secrets(&token, &["TEST_SHARED_SECRET".into()])
        .await
        .unwrap();
    for (state, handle) in [(&root, &entered.handle), (&user, &attached.handle)] {
        assert_eq!(
            state
                .secret_value(handle, "TEST_SHARED_SECRET")
                .await
                .unwrap(),
            "fixture-shared-snapshot"
        );
        assert_eq!(
            state
                .secret_value(handle, "TEST_SECOND_SECRET")
                .await
                .unwrap_err()
                .status,
            StatusCode::FORBIDDEN
        );
    }
    for sudo in [true, false] {
        let command = if cfg!(windows) {
            "[Console]::Write($env:TEST_SHARED_SECRET)"
        } else {
            "printf '%s' \"$TEST_SHARED_SECRET\""
        };
        assert_eq!(
            root.run(&entered.handle, command, sudo, None)
                .await
                .unwrap()
                .stdout,
            "fixture-shared-snapshot"
        );
    }
    for (sudo, marker) in [(true, "root-marker"), (false, "user-marker")] {
        let command = if cfg!(windows) {
            format!("$marker='{marker}'; $marker")
        } else {
            format!("marker='{marker}'; echo $marker")
        };
        assert_eq!(
            root.run(&entered.handle, &command, sudo, None)
                .await
                .unwrap()
                .output
                .trim(),
            marker
        );
    }
    let command = if cfg!(windows) {
        "$marker"
    } else {
        "echo $marker"
    };
    assert_eq!(
        root.run(&entered.handle, command, true, None)
            .await
            .unwrap()
            .output
            .trim(),
        "root-marker"
    );
    assert_eq!(
        root.run(&entered.handle, command, false, None)
            .await
            .unwrap()
            .output
            .trim(),
        "user-marker"
    );
    root.revoke_token(&token).await.unwrap();
    assert!(
        root.secret_value(&entered.handle, "TEST_SHARED_SECRET")
            .await
            .is_err()
    );
    assert!(
        user.secret_value(&attached.handle, "TEST_SHARED_SECRET")
            .await
            .is_err()
    );
    assert!(
        root.run(&entered.handle, "echo never", false, None)
            .await
            .is_err()
    );
    // A user-side snapshot must not remain readable if its authority is unreachable.
    let next_token = root.auth.lock().await.issue_token(None).unwrap().0;
    let next = user
        .enter_with_secrets(&next_token, &["TEST_SHARED_SECRET".into()])
        .await
        .unwrap();
    let unavailable_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unavailable_address = unavailable_listener.local_addr().unwrap();
    drop(unavailable_listener);
    let mut disconnected = user.clone();
    disconnected.peer =
        Some(crate::peer::Peer::new(unavailable_address, user.config.max_output_bytes).unwrap());
    assert_eq!(
        disconnected
            .secret_value(&next.handle, "TEST_SHARED_SECRET")
            .await
            .unwrap_err()
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(
        !user
            .sessions
            .lock()
            .await
            .by_handle
            .contains_key(&next.handle)
    );
    root.shutdown().await;
    user.shutdown().await;
    root_server.abort();
    user_server.abort();
}
