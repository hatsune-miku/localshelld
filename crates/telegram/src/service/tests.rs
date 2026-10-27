use super::*;
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::post,
};

#[derive(Clone, Default)]
struct Fixture {
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    fail: Arc<Mutex<bool>>,
}
async fn handler(
    State(fixture): State<Fixture>,
    Path(method): Path<String>,
    Json(body): Json<Value>,
) -> Json<Value> {
    fixture.calls.lock().await.push((method.clone(), body));
    if *fixture.fail.lock().await {
        return Json(json!({"ok":false,"description":"sensitive upstream response"}));
    }
    let result = match method.as_str() {
        "sendMessage" => json!({"message_id":fixture.calls.lock().await.len() as i64}),
        "getUpdates" => json!([]),
        _ => json!(true),
    };
    Json(json!({"ok":true,"result":result}))
}
fn config() -> Config {
    Config {
        bot_token: "123456:abcdefghijklmnopqrstuvwxyz".into(),
        bot_username: "fixture_bot".into(),
        enabled: true,
        binding: Some(Binding {
            user_id: 42,
            chat_id: 42,
        }),
    }
}
fn prompt(id: &str) -> Prompt {
    Prompt {
        id: id.into(),
        agent_name: "agent".into(),
        request_reason: "literal <b>reason</b>".into(),
        scope: "user".into(),
        secret_keys: vec!["TEST_KEY".into()],
        expires_at_ms: now() + 60_000,
    }
}
async fn setup() -> (Service, Fixture, tokio::task::JoinHandle<()>) {
    let fixture = Fixture::default();
    let router = Router::new()
        .route(
            "/bot123456:abcdefghijklmnopqrstuvwxyz/{method}",
            post(handler),
        )
        .with_state(fixture.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = Api::for_test(format!("http://{}", listener.local_addr().unwrap()));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let service = Service::new(api);
    // Drive updates deterministically; no network polling or real bot token.
    service.inner.state.lock().await.config = Some(config());
    service.inner.state.lock().await.worker =
        Some(tokio::spawn(std::future::pending::<()>()).abort_handle());
    (service, fixture, server)
}
#[tokio::test]
async fn concurrent_device_configuration_reuses_one_update_worker() {
    let (service, _, server) = setup().await;
    let worker = service
        .inner
        .state
        .lock()
        .await
        .worker
        .as_ref()
        .unwrap()
        .id();
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let clone = service.clone();
        tasks.push(tokio::spawn(async move {
            clone.configure(Some(config())).await.unwrap();
            clone.ensure_worker().await;
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(
        service
            .inner
            .state
            .lock()
            .await
            .worker
            .as_ref()
            .unwrap()
            .id(),
        worker
    );
    service.stop().await;
    server.abort();
}

async fn click(service: &Service, id: &str, action: &str, user: i64, message: i64) {
    service.update(0,json!({"callback_query":{"id":"query","data":format!("{id}:{action}"),"from":{"id":user,"is_bot":false},"message":{"message_id":message,"chat":{"id":user,"type":"private"}}}})).await;
}
#[tokio::test]
async fn rejects_other_users_messages_and_duplicate_decisions() {
    let (service, fixture, server) = setup().await;
    service
        .create("owner", "device", prompt("one"))
        .await
        .unwrap();
    assert!(service.poll("other-owner", "one").await.is_err());
    click(&service, "one", "accept", 77, 1).await;
    click(&service, "one", "accept", 42, 999).await;
    assert!(
        service.inner.state.lock().await.pending["one"]
            .candidate
            .borrow()
            .is_none()
    );
    click(&service, "one", "remember", 42, 1).await;
    service.update(0,json!({"message":{"from":{"id":42,"is_bot":false},"chat":{"id":42,"type":"private"},"reply_to_message":{"message_id":1},"text":"note <script>"}})).await;
    click(&service, "one", "accept", 42, 1).await;
    click(&service, "one", "deny", 42, 1).await;
    let result = service.poll("owner", "one").await.unwrap().unwrap();
    assert_eq!(
        result,
        Decision {
            choice: Choice::Accept,
            remember: true,
            btw: "note <script>".into()
        }
    );
    service
        .finish("owner", "one", Completion::Telegram)
        .await
        .unwrap();
    service.paint("one").await.unwrap();
    let calls = fixture.calls.lock().await;
    let body = &calls
        .iter()
        .rev()
        .find(|(m, _)| m == "editMessageText")
        .unwrap()
        .1;
    assert!(body["text"].as_str().unwrap().ends_with("已接受"));
    assert_eq!(body["reply_markup"]["inline_keyboard"], json!([]));
    assert!(calls.iter().all(|(_, v)| v.get("parse_mode").is_none()));
    service.stop().await;
    server.abort();
}
#[tokio::test]
async fn desktop_completion_is_final_and_retry_removes_buttons() {
    let (service, fixture, server) = setup().await;
    service
        .create("owner", "device", prompt("one"))
        .await
        .unwrap();
    *fixture.fail.lock().await = true;
    service
        .finish("owner", "one", Completion::Other)
        .await
        .unwrap();
    assert!(service.paint("one").await.is_err());
    assert!(service.inner.state.lock().await.pending["one"].dirty);
    click(&service, "one", "accept", 42, 1).await;
    assert!(service.poll("owner", "one").await.is_err());
    assert!(
        service.inner.state.lock().await.pending["one"]
            .candidate
            .borrow()
            .is_none()
    );
    *fixture.fail.lock().await = false;
    service.paint("one").await.unwrap();
    assert!(!service.inner.state.lock().await.pending["one"].dirty);
    service
        .finish("owner", "one", Completion::Expired)
        .await
        .unwrap();
    assert_eq!(
        service.inner.state.lock().await.pending["one"].completed,
        Some(Completion::Other)
    );
    let calls = fixture.calls.lock().await;
    let body = &calls
        .iter()
        .rev()
        .find(|(m, _)| m == "editMessageText")
        .unwrap()
        .1;
    assert!(body["text"].as_str().unwrap().ends_with("已经在其它端操作"));
    assert_eq!(body["reply_markup"]["inline_keyboard"], json!([]));
    service.stop().await;
    server.abort();
}
#[tokio::test]
async fn binding_requires_the_nonce_and_a_private_human_account() {
    let (service, _, server) = setup().await;
    let code = "0123456789abcdef0123456789abcdef";
    service.begin_binding("owner", Some(code)).await.unwrap();
    for (text, chat, bot) in [
        ("/start wrong".into(), 42, false),
        (format!("/start {code}"), 7, false),
        (format!("/start {code}"), 42, true),
    ] {
        service.update(0,json!({"message":{"text":text,"from":{"id":42,"is_bot":bot},"chat":{"id":chat,"type":"private"}}})).await;
        assert!(service.binding("owner", code).await.unwrap().is_none());
    }
    service.update(0,json!({"message":{"text":format!("/start {code}"),"from":{"id":42,"is_bot":false},"chat":{"id":42,"type":"private"}}})).await;
    assert_eq!(
        service.binding("owner", code).await.unwrap(),
        Some(Binding {
            user_id: 42,
            chat_id: 42
        })
    );
    assert!(service.binding("other", code).await.is_err());
    service.stop().await;
    server.abort();
}
#[tokio::test]
async fn ignore_expiry_and_generation_are_fail_closed() {
    let (service, _, server) = setup().await;
    service
        .create("owner", "device", prompt("one"))
        .await
        .unwrap();
    click(&service, "one", "remember", 42, 1).await;
    click(&service, "one", "ignore", 42, 1).await;
    let result = service.poll("owner", "one").await.unwrap().unwrap();
    assert_eq!(result.choice, Choice::Ignore);
    assert!(!result.remember);
    service
        .create("owner", "device", prompt("two"))
        .await
        .unwrap();
    let message = *service.inner.state.lock().await.pending["two"]
        .messages
        .last()
        .unwrap();
    service
        .inner
        .state
        .lock()
        .await
        .pending
        .get_mut("two")
        .unwrap()
        .prompt
        .expires_at_ms = now() - 1;
    click(&service, "two", "accept", 42, message).await;
    assert_eq!(
        service.inner.state.lock().await.pending["two"].completed,
        Some(Completion::Expired)
    );
    assert!(
        service.inner.state.lock().await.pending["two"]
            .candidate
            .borrow()
            .is_none()
    );
    service.stop().await;
    server.abort();
}
#[test]
fn length_bounds_account_for_callback_suffix_and_unicode() {
    let mut p = prompt(&"a".repeat(48));
    assert!(validate_prompt(&p).is_ok());
    assert!(keyboard(&p.id, false).to_string().contains("remember"));
    p.id.push('a');
    assert!(validate_prompt(&p).is_err());
    p = prompt("id");
    p.expires_at_ms = i64::MIN;
    assert!(validate_prompt(&p).is_err());
    let long = "😀你好".repeat(3500);
    let parts = chunks(&long, 1700);
    assert_eq!(parts.concat(), long);
    assert!(parts.iter().all(|p| p.encode_utf16().count() <= 3400));
}
