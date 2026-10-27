use super::*;
use crate::{
    auth::AuthManager,
    consent::{ConsentBroker, DesktopEvent},
    sync::SavedSync,
};
use axum::{Json, Router, extract::State, routing::post};
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone)]
struct Relay {
    desktop_wins: bool,
    done: mpsc::UnboundedSender<Value>,
}
async fn create() -> Json<Value> {
    Json(json!({"created":true}))
}
async fn poll(State(relay): State<Relay>) -> Json<Value> {
    if relay.desktop_wins {
        std::future::pending::<()>().await;
    }
    Json(json!({"decision":{"choice":"accept","remember":true,"btw":"phone note"}}))
}
async fn finish(State(relay): State<Relay>, Json(body): Json<Value>) -> Json<Value> {
    let _ = relay.done.send(body);
    Json(json!({"closed":true}))
}
async fn channels(desktop_wins: bool) {
    let (done, mut completed) = mpsc::unbounded_channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://localhost:{}", listener.local_addr().unwrap().port());
    let router = Router::new()
        .route("/sync/v1/telegram/request", post(create))
        .route("/sync/v1/telegram/poll", post(poll))
        .route("/sync/v1/telegram/finish", post(finish))
        .with_state(Relay { desktop_wins, done });
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let (events, mut received) = mpsc::unbounded_channel();
    let held = Arc::new(std::sync::Mutex::new(None));
    let broker = ConsentBroker::new(move |event| {
        match event {
            DesktopEvent::Show(_, reply) => {
                let _ = events.send("show");
                if desktop_wins {
                    let _ = reply.send(Decision {
                        choice: Choice::Deny,
                        remember: false,
                        btw: "desktop note".into(),
                    });
                } else {
                    *held.lock().unwrap() = Some(reply);
                }
            }
            DesktopEvent::Close(_) => {
                let _ = events.send("close");
            }
            _ => {}
        }
        Ok(())
    });
    let state = AppState::new(
        config::Config::default(),
        AuthManager::new("unused".into(), None),
    )
    .with_consent(broker);
    {
        let mut settings = state.settings.lock().await;
        let tokens = localshelld_sync_protocol::Tokens {
            access_token: "test-access".into(),
            refresh_token: "test-refresh".into(),
            expires_at: chrono::Utc::now().timestamp() + 600,
            refresh_expires_at: chrono::Utc::now().timestamp() + 86400,
        };
        settings.saved.sync = Some(SavedSync {
            url,
            revision: 0,
            tokens: config::seal(&serde_json::to_vec(&tokens).unwrap(), &settings.seal_key)
                .unwrap(),
        });
    }
    let prompt = Prompt {
        id: "arbitration".into(),
        agent_name: "test".into(),
        request_reason: "test only".into(),
        scope: "user".into(),
        secret_keys: vec![],
        expires_at_ms: chrono::Utc::now().timestamp_millis() + 60000,
    };
    let result = tokio::time::timeout(Duration::from_secs(5), state.request_consent(prompt))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        result.choice,
        if desktop_wins {
            Choice::Deny
        } else {
            Choice::Accept
        }
    );
    assert_eq!(received.recv().await, Some("show"));
    assert_eq!(received.recv().await, Some("close"));
    let completion = tokio::time::timeout(Duration::from_secs(5), completed.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        completion["completion"],
        if desktop_wins { "other" } else { "telegram" }
    );
    assert_eq!(completion["id"], "arbitration");
    state.shutdown().await;
    server.abort();
}
#[tokio::test]
async fn telegram_winner_closes_the_native_popup() {
    channels(false).await;
}
#[tokio::test]
async fn desktop_winner_completes_even_late_telegram_delivery() {
    channels(true).await;
}

#[tokio::test]
async fn encrypted_configuration_does_not_enter_credential_exports() {
    let state = AppState::new(
        config::Config::default(),
        AuthManager::new("unused".into(), None),
    );
    let config = Config {
        bot_token: "123456:abcdefghijklmnopqrstuvwxyz".into(),
        bot_username: "fixture_bot".into(),
        enabled: false,
        binding: None,
    };
    state
        .save_local_telegram(Some(config.clone()))
        .await
        .unwrap();
    let settings = state.settings.lock().await;
    assert!(read_config(&settings).unwrap().is_some());
    assert!(
        !serde_json::to_string(&settings.saved)
            .unwrap()
            .contains(&config.bot_token)
    );
    drop(settings);
    assert!(state.export_secrets().await.unwrap().credentials.is_empty());
    state.shutdown().await;
}
