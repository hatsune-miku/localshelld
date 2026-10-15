//! Manual native-WebView smoke test. This does not start a daemon, create a
//! shell or grant access; it only prints the choice made in a test window.
use localshelld::{consent::Prompt, desktop::Desktop};

fn main() {
    let desktop = Desktop::new().expect("test desktop");
    let broker = desktop.broker();
    let stop = desktop.stop_callback();
    let runtime = tokio::runtime::Runtime::new().expect("test runtime");
    runtime.spawn(async move {
        let result = broker
            .request(Prompt {
                id: "native-smoke-test".into(),
                agent_name: "localshelld 界面测试".into(),
                request_reason:
                    "仅用于界面测试，不授予任何命令权限。\n检查原生窗口、完整缘由和附加说明回传。"
                        .into(),
                scope: "无（测试）".into(),
                expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
            })
            .await;
        match result {
            Ok(decision) => println!("{}", serde_json::to_string(&decision).unwrap()),
            Err(error) => eprintln!("{error}"),
        }
        stop(0);
    });
    desktop.run();
}
