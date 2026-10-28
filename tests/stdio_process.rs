//! Exercise the shipped stdio entry point against a disposable loopback fixture.
use axum::{Json, Router, extract::State, routing::post};
use serde_json::{Value, json};
use std::{
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::Notify,
};

#[derive(Default)]
struct Fixture {
    entered: Notify,
    run: Notify,
    release: Notify,
    destroyed: AtomicBool,
    delayed: AtomicBool,
}

async fn enter(State(state): State<Arc<Fixture>>, Json(args): Json<Value>) -> Json<Value> {
    assert_eq!(args["token"], "fixture-owner-token");
    state.entered.notify_one();
    if state.delayed.load(Ordering::SeqCst) {
        state.release.notified().await;
    }
    Json(
        json!({"handle":"fixture-private-handle","reused":false,"shell":"fixture","platform":std::env::consts::OS,"daemon":"user","identities":{},"privilege_levels":[],"expires_at":null,"expires_at_iso":null}),
    )
}

async fn run(State(state): State<Arc<Fixture>>, Json(args): Json<Value>) -> Json<Value> {
    assert_eq!(args["handle"], "fixture-private-handle");
    state.run.notify_one();
    state.release.notified().await;
    Json(json!({"stdout":"fixture-result","session_ended":false}))
}

async fn destroy(State(state): State<Arc<Fixture>>, Json(args): Json<Value>) -> Json<Value> {
    assert_eq!(args["handle"], "fixture-private-handle");
    state.destroyed.store(true, Ordering::SeqCst);
    Json(json!({"destroyed":true}))
}

async fn fixture() -> (String, Arc<Fixture>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(Fixture::default());
    let router = Router::new()
        .route("/v1/sessions/enter/preauthorized", post(enter))
        .route("/v1/commands/run", post(run))
        .route("/v1/sessions/destroy", post(destroy))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (address, state, task)
}

fn adapter(endpoint: &str) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_trustd-stdio"));
    command
        .args(["--endpoint", endpoint, "--token-env", "TRUSTD_TEST_TOKEN"])
        .env("TRUSTD_TEST_TOKEN", "fixture-owner-token")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    command.spawn().unwrap()
}

async fn send(write: &mut tokio::process::ChildStdin, id: usize, method: &str, params: Value) {
    let text = format!(
        "{}\n",
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
    );
    write.write_all(text.as_bytes()).await.unwrap();
    write.flush().await.unwrap();
}

async fn receive(read: &mut BufReader<tokio::process::ChildStdout>) -> Value {
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), read.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert!(!line.contains("fixture-owner-token"));
    assert!(!line.contains("fixture-private-handle"));
    serde_json::from_str(&line).unwrap()
}

#[tokio::test]
async fn binary_hides_credentials_survives_partial_input_and_cleans_up_on_eof() {
    let (endpoint, state, server) = fixture().await;
    let mut child = adapter(&endpoint);
    let mut write = child.stdin.take().unwrap();
    let mut read = BufReader::new(child.stdout.take().unwrap());
    send(
        &mut write,
        1,
        "initialize",
        json!({"protocolVersion":"2025-06-18"}),
    )
    .await;
    assert_eq!(
        receive(&mut read).await["result"]["serverInfo"]["name"],
        "trustd-stdio"
    );
    send(&mut write, 2, "tools/list", json!({})).await;
    let listed = receive(&mut read).await;
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 7);
    send(
        &mut write,
        3,
        "tools/call",
        json!({"name":"enter_preauthorized","arguments":{"confirm_text":"OK"}}),
    )
    .await;
    assert_eq!(receive(&mut read).await["result"]["isError"], false);
    let mut args = json!({"command":"fixture-command"});
    if cfg!(windows) {
        args["privilege_level"] = json!("current-user");
    } else {
        args["sudo"] = json!(false);
    }
    send(
        &mut write,
        4,
        "tools/call",
        json!({"name":"run","arguments":args}),
    )
    .await;
    state.run.notified().await;
    // A response arriving between two parts of the next line must not discard input.
    write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,")
        .await
        .unwrap();
    write.flush().await.unwrap();
    state.release.notify_one();
    assert_eq!(receive(&mut read).await["id"], 4);
    write.write_all(b"\"method\":\"ping\"}\n").await.unwrap();
    write.flush().await.unwrap();
    assert_eq!(receive(&mut read).await["id"], 5);
    drop(write);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(state.destroyed.load(Ordering::SeqCst));
    server.abort();
}

#[tokio::test]
async fn eof_during_authorization_destroys_the_late_session_before_exit() {
    let (endpoint, state, server) = fixture().await;
    state.delayed.store(true, Ordering::SeqCst);
    let mut child = adapter(&endpoint);
    let mut write = child.stdin.take().unwrap();
    let mut read = BufReader::new(child.stdout.take().unwrap());
    send(&mut write, 1, "initialize", json!({})).await;
    receive(&mut read).await;
    send(
        &mut write,
        2,
        "tools/call",
        json!({"name":"enter_preauthorized","arguments":{"confirm_text":"OK"}}),
    )
    .await;
    state.entered.notified().await;
    drop(write);
    state.release.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(state.destroyed.load(Ordering::SeqCst));
    server.abort();
}

#[test]
fn entry_points_do_not_mix_maintenance_and_daemon_roles() {
    for (binary, absent) in [
        (env!("CARGO_BIN_EXE_trustd"), "serve"),
        (env!("CARGO_BIN_EXE_trustd-user"), "install"),
        (env!("CARGO_BIN_EXE_trustd-stdio"), "install"),
    ] {
        let mut command = std::process::Command::new(binary);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let result = command.arg(absent).output().unwrap();
        assert!(!result.status.success());
    }
}
