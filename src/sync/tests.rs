use super::*;
use crate::{auth::AuthManager, config::Config};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::{Mutex, watch};

#[derive(Clone)]
struct Mock {
    snapshot: Arc<Mutex<Snapshot>>,
    unavailable: Arc<AtomicBool>,
    conflict: Arc<AtomicBool>,
    renewals: Arc<AtomicUsize>,
    revisions: watch::Sender<u64>,
}

async fn get_vault(State(mock): State<Mock>, headers: HeaderMap) -> Response {
    if mock.unavailable.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if !headers.contains_key("authorization") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    Json(mock.snapshot.lock().await.clone()).into_response()
}
async fn put_vault(State(mock): State<Mock>, Json(body): Json<Replace>) -> Response {
    if mock.unavailable.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let mut snapshot = mock.snapshot.lock().await;
    if mock.conflict.swap(false, Ordering::SeqCst) {
        snapshot.revision += 1;
        snapshot.credentials = BTreeMap::from([("OTHER_DEVICE".into(), "concurrent value".into())]);
    }
    if snapshot.revision != body.expected_revision {
        return StatusCode::CONFLICT.into_response();
    }
    snapshot.revision += 1;
    snapshot.credentials = body.credentials.clone();
    mock.revisions.send_replace(snapshot.revision);
    Json(snapshot.clone()).into_response()
}
async fn renew(State(mock): State<Mock>) -> Json<Tokens> {
    mock.renewals.fetch_add(1, Ordering::SeqCst);
    Json(tokens(false))
}
async fn events(
    State(mock): State<Mock>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    let mut receiver = mock.revisions.subscribe();
    let after = query["after"].parse::<u64>().unwrap();
    if *receiver.borrow_and_update() == after {
        let _ = tokio::time::timeout(Duration::from_millis(100), receiver.changed()).await;
    }
    Json(json!({"revision":mock.snapshot.lock().await.revision})).into_response()
}
fn tokens(expired: bool) -> Tokens {
    Tokens {
        access_token: "fixture-access".into(),
        refresh_token: "fixture-refresh".into(),
        expires_at: Utc::now().timestamp() + if expired { 1 } else { 600 },
        refresh_expires_at: Utc::now().timestamp() + 86400,
    }
}
async fn server() -> (Mock, String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://localhost:{}", listener.local_addr().unwrap().port());
    let (revisions, _) = watch::channel(1);
    let mock = Mock {
        snapshot: Arc::new(Mutex::new(Snapshot {
            revision: 1,
            credentials: BTreeMap::from([("CLOUD_TOKEN".into(), "cloud value".into())]),
        })),
        unavailable: Arc::new(AtomicBool::new(false)),
        conflict: Arc::new(AtomicBool::new(false)),
        renewals: Arc::new(AtomicUsize::new(0)),
        revisions,
    };
    let app = Router::new()
        .route("/sync/v1/vault", get(get_vault).put(put_vault))
        .route("/sync/v1/session/refresh", post(renew))
        .route("/sync/v1/events", get(events))
        .with_state(mock.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (mock, url, task)
}
async fn device(url: &str, expired: bool) -> AppState {
    let state = AppState::new(Config::default(), AuthManager::new("unused".into(), None));
    state
        .put_secret("OLD_LOCAL", "old local value")
        .await
        .unwrap();
    let mut settings = state.settings.lock().await;
    settings.saved.sync = Some(SavedSync {
        url: url.into(),
        tokens: config::seal(
            &serde_json::to_vec(&tokens(expired)).unwrap(),
            &settings.seal_key,
        )
        .unwrap(),
        revision: 0,
    });
    drop(settings);
    state
}

#[tokio::test]
async fn cloud_is_authority_and_unavailable_service_never_falls_back_to_local_writes() {
    let (mock, url, server) = server().await;
    let a = device(&url, false).await;
    let b = device(&url, false).await;
    assert_eq!(a.secret_keys().await.unwrap(), ["CLOUD_TOKEN"]);
    assert!(
        !a.settings
            .lock()
            .await
            .saved
            .secrets
            .contains_key("OLD_LOCAL")
    );
    a.put_secret("SECOND_TOKEN", "second value").await.unwrap();
    assert_eq!(
        b.secret_keys().await.unwrap(),
        ["CLOUD_TOKEN", "SECOND_TOKEN"]
    );
    assert_eq!(mock.snapshot.lock().await.revision, 2);
    mock.unavailable.store(true, Ordering::SeqCst);
    assert!(a.put_secret("UNCOMMITTED", "never stored").await.is_err());
    assert!(a.secret_keys().await.is_err());
    assert!(a.resolve_secrets(&["CLOUD_TOKEN".into()]).await.is_err());
    assert!(a.export_secrets().await.is_err());
    assert!(
        !a.settings
            .lock()
            .await
            .saved
            .secrets
            .contains_key("UNCOMMITTED")
    );
    a.sync_action(Action::Disconnect).await.unwrap();
    assert!(a.settings.lock().await.saved.sync.is_none());
    a.put_secret("OFFLINE_TOKEN", "offline value")
        .await
        .unwrap();
    assert!(
        a.secret_keys()
            .await
            .unwrap()
            .contains(&"OFFLINE_TOKEN".to_owned())
    );
    server.abort();
}

#[tokio::test]
async fn conflict_reconciles_cloud_snapshot_without_overwriting_another_device() {
    let (mock, url, server) = server().await;
    let a = device(&url, false).await;
    mock.conflict.store(true, Ordering::SeqCst);
    assert_eq!(
        a.put_secret("MY_EDIT", "not committed")
            .await
            .unwrap_err()
            .status_code(),
        409
    );
    assert_eq!(a.secret_keys().await.unwrap(), ["OTHER_DEVICE"]);
    assert_eq!(mock.snapshot.lock().await.credentials.len(), 1);
    server.abort();
}

#[tokio::test]
async fn concurrent_reads_rotate_refresh_token_only_once_and_store_it_encrypted() {
    let (mock, url, server) = server().await;
    let a = device(&url, true).await;
    let (first, second) = tokio::join!(a.secret_keys(), a.secret_keys());
    assert!(first.is_ok() && second.is_ok());
    assert_eq!(mock.renewals.load(Ordering::SeqCst), 1);
    let serialized = serde_json::to_string(&a.settings.lock().await.saved).unwrap();
    assert!(!serialized.contains("fixture-access"));
    assert!(!serialized.contains("fixture-refresh"));
    server.abort();
}

#[tokio::test]
async fn worker_overwrites_idle_client_cache_after_cloud_notification() {
    let (mock, url, server) = server().await;
    let a = device(&url, false).await;
    a.start_sync_worker().await;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if a.settings
                .lock()
                .await
                .saved
                .secrets
                .contains_key("CLOUD_TOKEN")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    {
        let mut snapshot = mock.snapshot.lock().await;
        snapshot.credentials =
            BTreeMap::from([("NOTIFIED_TOKEN".into(), "from second device".into())]);
        snapshot.revision += 1;
        mock.revisions.send_replace(snapshot.revision);
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if a.settings
                .lock()
                .await
                .saved
                .secrets
                .contains_key("NOTIFIED_TOKEN")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        !a.settings
            .lock()
            .await
            .saved
            .secrets
            .contains_key("CLOUD_TOKEN")
    );
    a.stop_sync_worker().await;
    server.abort();
}

#[test]
fn validates_sync_origin_without_permitting_insecure_remote_urls() {
    assert_eq!(
        normalize_url("https://sync.example.com/").unwrap(),
        "https://sync.example.com"
    );
    assert_eq!(
        normalize_url("http://localhost:32121").unwrap(),
        "http://localhost:32121"
    );
    for invalid in [
        "http://sync.example.com",
        "https://sync.example.com/path",
        "https://user:password@sync.example.com",
        "https://sync.example.com?token=hello",
    ] {
        assert!(normalize_url(invalid).is_err());
    }
}
