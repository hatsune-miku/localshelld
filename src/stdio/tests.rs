use super::*;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use std::{collections::HashSet, sync::atomic::AtomicUsize};

#[derive(Default)]
struct Fake {
    sessions: Mutex<HashSet<String>>,
    sequence: AtomicUsize,
    requests: Mutex<Vec<Value>>,
    ended: tokio::sync::Notify,
    delayed: AtomicBool,
    release: tokio::sync::Notify,
    reused: AtomicBool,
    destroy_failed: AtomicBool,
}

async fn enter(State(state): State<Arc<Fake>>, Json(args): Json<Value>) -> Json<Value> {
    state.requests.lock().await.push(args);
    if state.delayed.load(Ordering::SeqCst) {
        state.release.notified().await;
    }
    let handle = format!(
        "private-handle-{}",
        state.sequence.fetch_add(1, Ordering::SeqCst)
    );
    state.sessions.lock().await.insert(handle.clone());
    Json(
        json!({"handle":handle,"status":"authorized","btw":"owner note","reused":state.reused.load(Ordering::SeqCst)}),
    )
}

async fn destroy(
    State(state): State<Arc<Fake>>,
    Json(args): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    if state.destroy_failed.load(Ordering::SeqCst) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let removed = state
        .sessions
        .lock()
        .await
        .remove(args["handle"].as_str().unwrap());
    if !removed {
        return Err(StatusCode::UNAUTHORIZED);
    }
    state.ended.notify_waiters();
    Ok(Json(json!({"destroyed":true})))
}

async fn run(
    State(state): State<Arc<Fake>>,
    Json(args): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let waiting = state.ended.notified();
    let handle = args["handle"].as_str().ok_or(StatusCode::BAD_REQUEST)?;
    if !state.sessions.lock().await.contains(handle) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    state.requests.lock().await.push(args.clone());
    if args["command"] == "wait" {
        waiting.await;
    }
    Ok(Json(
        json!({"stdout":"ok","session_ended":args["command"] == "wait"}),
    ))
}

async fn fake() -> (SocketAddr, Arc<Fake>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(Fake::default());
    let router = Router::new()
        .route("/v1/sessions/enter/interactive", post(enter))
        .route("/v1/sessions/enter/preauthorized", post(enter))
        .route("/v1/sessions/destroy", post(destroy))
        .route("/v1/commands/run", post(run))
        .route(
            "/v1/secrets/keys",
            get(|| async { Json(json!({"keys":["TEST_TOKEN"]})) }),
        )
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (address, state, task)
}

fn approval() -> Value {
    json!({"name":"enter_interactive","arguments":{"agent_name":"Test","request_reason":"test","confirm_text":"OK"}})
}

fn command(value: &str) -> Value {
    let mut args = json!({"command":value});
    if cfg!(windows) {
        args["privilege_level"] = json!("current-user");
    } else {
        args["sudo"] = json!(false);
    }
    json!({"name":"run","arguments":args})
}

async fn call(adapter: &Adapter, params: Value) -> Result<Value> {
    let (_tx, rx) = watch::channel(false);
    adapter.call(params, rx).await
}

#[test]
fn no_bearer_fields_or_remote_endpoint_in_model_contract() {
    for token in [None, Some(Zeroizing::new("private-token".into()))] {
        let adapter = Adapter::new("127.0.0.1:32120".parse().unwrap(), token).unwrap();
        for entry in adapter.definitions().as_array().unwrap() {
            for schema in ["inputSchema", "outputSchema"] {
                let text = entry[schema].to_string();
                assert!(!text.contains("\"handle\""));
                assert!(!text.contains("\"token\""));
                assert!(!text.contains("private-token"));
            }
        }
    }
    assert!(Adapter::new("192.0.2.1:32120".parse().unwrap(), None).is_err());
    assert!(Adapter::new("127.0.0.1:0".parse().unwrap(), None).is_err());
}

#[tokio::test]
async fn holds_handle_in_process_and_keeps_independent_instances_isolated() {
    let (address, state, server) = fake().await;
    let first = Adapter::new(address, None).unwrap();
    let second = Adapter::new(address, None).unwrap();
    assert!(call(&first, command("echo")).await.is_err());
    let result = call(&first, approval()).await.unwrap();
    assert!(!result.to_string().contains("private-handle"));
    assert_eq!(result["btw"], "owner note");
    assert!(call(&first, approval()).await.is_err());
    assert!(call(&second, command("echo")).await.is_err());
    call(&second, approval()).await.unwrap();
    call(&first, command("echo")).await.unwrap();
    call(&second, command("echo")).await.unwrap();
    let requests = state.requests.lock().await;
    assert_ne!(requests[2]["handle"], requests[3]["handle"]);
    drop(requests);
    call(&first, json!({"name":"destroy"})).await.unwrap();
    assert_eq!(state.sessions.lock().await.len(), 1);
    call(&second, command("echo")).await.unwrap();
    let mut forged = command("echo");
    forged["arguments"]["handle"] = json!("forged-handle");
    assert!(call(&second, forged).await.is_err());
    second.close().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn explicit_close_forgets_expired_handle_but_retains_unknown_cleanup() {
    let (address, state, server) = fake().await;
    let adapter = Adapter::new(address, None).unwrap();
    call(&adapter, approval()).await.unwrap();
    state.destroy_failed.store(true, Ordering::SeqCst);
    assert!(adapter.close().await.is_err());
    assert!(adapter.handle.lock().await.is_some());
    assert!(call(&adapter, approval()).await.is_err());

    state.destroy_failed.store(false, Ordering::SeqCst);
    state.sessions.lock().await.clear();
    assert!(call(&adapter, command("echo")).await.is_err());
    // No automatic authorization or session substitution after expiry.
    assert!(call(&adapter, approval()).await.is_err());
    adapter.close().await.unwrap();
    assert!(adapter.handle.lock().await.is_none());
    call(&adapter, approval()).await.unwrap();
    call(&adapter, command("echo")).await.unwrap();
    adapter.close().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn cancellation_closes_only_the_originating_adapters_session() {
    let (address, state, server) = fake().await;
    let first = Arc::new(Adapter::new(address, None).unwrap());
    let second = Adapter::new(address, None).unwrap();
    call(&first, approval()).await.unwrap();
    call(&second, approval()).await.unwrap();
    let (cancel, receiver) = watch::channel(false);
    let worker = first.clone();
    let running = tokio::spawn(async move { worker.call(command("wait"), receiver).await });
    while state.requests.lock().await.len() < 3 {
        tokio::task::yield_now().await;
    }
    cancel.send(true).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(3), running)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(first.handle.lock().await.is_none());
    assert_eq!(state.sessions.lock().await.len(), 1);
    call(&second, command("echo")).await.unwrap();
    second.close().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn cancelled_authorization_cleans_up_late_approval() {
    let (address, state, server) = fake().await;
    state.delayed.store(true, Ordering::SeqCst);
    let adapter = Arc::new(Adapter::new(address, None).unwrap());
    let (cancel, receiver) = watch::channel(false);
    let worker = adapter.clone();
    let pending = tokio::spawn(async move { worker.call(approval(), receiver).await });
    while state.requests.lock().await.is_empty() {
        tokio::task::yield_now().await;
    }
    cancel.send(true).unwrap();
    state.release.notify_one();
    assert!(pending.await.unwrap().is_err());
    assert!(state.sessions.lock().await.is_empty());
    assert!(adapter.handle.lock().await.is_none());
    server.abort();
}

#[tokio::test]
async fn rejects_reused_preauthorized_session_without_destroying_other_owner() {
    let (address, state, server) = fake().await;
    state.reused.store(true, Ordering::SeqCst);
    let adapter = Adapter::new(address, Some(Zeroizing::new("configured-token".into()))).unwrap();
    let result = call(
        &adapter,
        json!({"name":"enter_preauthorized","arguments":{"confirm_text":"OK"}}),
    )
    .await;
    assert!(result.is_err());
    assert!(!result.unwrap_err().to_string().contains("configured-token"));
    assert!(adapter.handle.lock().await.is_none());
    assert_eq!(state.sessions.lock().await.len(), 1);
    assert_eq!(state.requests.lock().await[0]["token"], "configured-token");
    server.abort();
}

#[tokio::test]
async fn stdio_is_newline_json_and_eof_destroys_the_session() {
    let (address, state, server) = fake().await;
    let adapter = Adapter::new(address, None).unwrap();
    let (client, io) = tokio::io::duplex(65536);
    let (input, output) = tokio::io::split(io);
    let worker = tokio::spawn(adapter.serve(input, output));
    let (read, mut write) = tokio::io::split(client);
    let mut read = BufReader::new(read);
    write_message(
        &mut write,
        &json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    )
    .await
    .unwrap();
    let mut line = String::new();
    read.read_line(&mut line).await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["result"]["serverInfo"]["name"],
        "trustd-stdio"
    );
    write_message(
        &mut write,
        &json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":approval()}),
    )
    .await
    .unwrap();
    line.clear();
    read.read_line(&mut line).await.unwrap();
    assert!(!line.contains("private-handle"));
    assert_eq!(state.sessions.lock().await.len(), 1);
    write.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(state.sessions.lock().await.is_empty());
    server.abort();
}

#[tokio::test]
async fn framing_rejects_oversized_and_unterminated_messages() {
    for value in [vec![b'x'; MAX_MESSAGE + 1], b"{}".to_vec()] {
        assert!(
            read_message(&mut BufReader::new(value.as_slice()), &mut Vec::new())
                .await
                .is_err()
        );
    }
    let mut partial = b"{\"jsonrpc\":".to_vec();
    let message = read_message(&mut BufReader::new(&b"\"2.0\"}\n"[..]), &mut partial)
        .await
        .unwrap()
        .unwrap();
    assert!(serde_json::from_slice::<Value>(&message).is_ok());
}
