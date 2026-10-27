use super::*;
use crate::{auth::AuthManager, config::Config};

fn state() -> (AppState, String) {
    let mut auth = AuthManager::new("unused".into(), None);
    let token = auth.issue_token(None).unwrap().0;
    (
        AppState::user(
            Config {
                bind: "127.0.0.1:32120".parse().unwrap(),
                ..Config::default()
            },
            auth,
        )
        .unwrap(),
        token,
    )
}

fn read_alpha() -> &'static str {
    if cfg!(windows) {
        "[Console]::Write($env:TEST_SECRET_ALPHA)"
    } else {
        "printf '%s' \"$TEST_SECRET_ALPHA\""
    }
}

#[test]
fn validates_portable_uppercase_names_and_bounded_lists() {
    for key in [
        "",
        "lowercase",
        "9KEY",
        "A-B",
        "A=B",
        "A\nB",
        "密码",
        "LOCALSHELLD_SESSION",
        "BASH_ENV",
        "ENV",
    ] {
        assert!(validate_key(key).is_err(), "{key}");
    }
    assert!(validate_key(&"A".repeat(129)).is_err());
    assert!(validate_key("_API_KEY_2").is_ok());
    assert_eq!(
        normalize_keys(&["B".into(), "A".into(), "B".into()]).unwrap(),
        ["A", "B"]
    );
    assert!(normalize_keys(&vec!["A".into(); 65]).is_err());
}

#[tokio::test]
async fn values_are_sealed_and_invalid_writes_leave_existing_credentials_intact() {
    let (state, _) = state();
    const VALUE: &str = "fixture-only-value-汉字-123";
    state.put_secret("TEST_SECRET_ALPHA", VALUE).await.unwrap();
    let serialized = serde_json::to_string(&state.settings.lock().await.saved).unwrap();
    assert!(serialized.contains("TEST_SECRET_ALPHA"));
    assert!(!serialized.contains(VALUE));
    for value in [
        "".to_owned(),
        "bad\0value".into(),
        "x".repeat(MAX_VALUE_BYTES + 1),
    ] {
        assert!(state.put_secret("TEST_SECRET_ALPHA", &value).await.is_err());
    }
    assert_eq!(
        state
            .resolve_secrets(&["TEST_SECRET_ALPHA".into()])
            .await
            .unwrap()
            .0["TEST_SECRET_ALPHA"],
        VALUE
    );
    assert!(state.resolve_secrets(&["MISSING".into()]).await.is_err());
    assert!(
        state
            .enter_with_secrets("invalid", &["TEST_SECRET_ALPHA".into()])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn preauthorized_session_injects_only_selected_values_and_reuses_its_snapshot() {
    let (state, token) = state();
    let value = "fixture-only ' \" $ ` ; & \r\n第二行";
    state.put_secret("TEST_SECRET_ALPHA", value).await.unwrap();
    state
        .put_secret("TEST_SECRET_BETA", "not-requested")
        .await
        .unwrap();
    let keys = vec!["TEST_SECRET_ALPHA".into()];
    let entered = state.enter_with_secrets(&token, &keys).await.unwrap();
    assert_eq!(entered.secret_keys, keys);
    assert!(!serde_json::to_string(&entered).unwrap().contains(value));
    assert_eq!(
        state
            .run(&entered.handle, read_alpha(), false, None)
            .await
            .unwrap()
            .stdout,
        value
    );
    let absent = if cfg!(windows) {
        "[Console]::Write([Environment]::GetEnvironmentVariable('TEST_SECRET_BETA') -eq $null)"
    } else {
        "test -z \"${TEST_SECRET_BETA+x}\" && printf True"
    };
    assert_eq!(
        state
            .run(&entered.handle, absent, false, None)
            .await
            .unwrap()
            .stdout,
        "True"
    );
    state
        .put_secret("TEST_SECRET_ALPHA", "rotated")
        .await
        .unwrap();
    assert_eq!(
        state
            .enter_with_secrets(&token, &keys)
            .await
            .unwrap()
            .handle,
        entered.handle
    );
    state.remove_secret("TEST_SECRET_ALPHA").await.unwrap();
    assert_eq!(
        state
            .enter_with_secrets(&token, &keys)
            .await
            .unwrap()
            .handle,
        entered.handle
    );
    assert_eq!(state.enter(&token).await.unwrap().secret_keys, keys);
    assert_eq!(
        state
            .run(&entered.handle, read_alpha(), false, None)
            .await
            .unwrap()
            .stdout,
        value
    );
    assert!(
        state
            .enter_with_secrets(&token, &["TEST_SECRET_BETA".into()])
            .await
            .is_err()
    );
    let second_token = state.auth.lock().await.issue_token(None).unwrap().0;
    let isolated = state.enter(&second_token).await.unwrap();
    assert_eq!(
        state
            .run(&isolated.handle, read_alpha(), false, None)
            .await
            .unwrap()
            .stdout,
        ""
    );
    state.destroy_session(&entered.handle).await.unwrap();
    assert!(state.enter_with_secrets(&token, &keys).await.is_err());
    state
        .put_secret("TEST_SECRET_ALPHA", "new-session-value")
        .await
        .unwrap();
    let fresh = state.enter_with_secrets(&token, &keys).await.unwrap();
    assert_ne!(fresh.handle, entered.handle);
    assert_eq!(
        state
            .run(&fresh.handle, read_alpha(), false, None)
            .await
            .unwrap()
            .stdout,
        "new-session-value"
    );
    state.revoke_token(&token).await.unwrap();
    assert!(
        state
            .run(&fresh.handle, read_alpha(), false, None)
            .await
            .is_err()
    );
    state.shutdown().await;
}
