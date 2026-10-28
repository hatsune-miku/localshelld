use axum::{
    Json,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, watch};

use crate::{
    api::{self, Operation},
    mcp_text::{
        MISSING_TOOL_NAME, server_instructions, tool_definitions, unknown_method, unknown_tool,
    },
    server::{ApiError, AppState},
};

#[derive(Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Default)]
pub(crate) struct TransportSessions(Mutex<HashMap<String, TransportSession>>);
struct TransportSession {
    touched: Instant,
    running: HashMap<String, RunningRequest>,
}
struct RunningRequest {
    handle: String,
    cancel: watch::Sender<bool>,
}
const SESSION_HEADER: &str = "mcp-session-id";

impl TransportSessions {
    async fn create(&self) -> Result<String, ApiError> {
        let mut sessions = self.0.lock().await;
        sessions.retain(|_, session| {
            !session.running.is_empty() || session.touched.elapsed() < Duration::from_secs(3600)
        });
        if sessions.len() >= 1024 {
            return Err(ApiError::unavailable("too many MCP transport sessions"));
        }
        let id = crate::server::strong_handle();
        sessions.insert(
            id.clone(),
            TransportSession {
                touched: Instant::now(),
                running: HashMap::new(),
            },
        );
        Ok(id)
    }
    async fn register(
        &self,
        session: &str,
        id: &Value,
        handle: &str,
    ) -> Result<watch::Receiver<bool>, ApiError> {
        let mut sessions = self.0.lock().await;
        let session = sessions.get_mut(session).ok_or_else(|| {
            ApiError::bad_request("unknown MCP transport session; initialize again")
        })?;
        session.touched = Instant::now();
        let id = id.to_string();
        if session.running.contains_key(&id) {
            return Err(ApiError::bad_request("duplicate in-flight MCP request ID"));
        }
        let (cancel, receiver) = watch::channel(false);
        session.running.insert(
            id,
            RunningRequest {
                handle: handle.into(),
                cancel,
            },
        );
        Ok(receiver)
    }
    async fn take(&self, session: &str, id: &Value) -> Option<RunningRequest> {
        let mut sessions = self.0.lock().await;
        let session = sessions.get_mut(session)?;
        session.touched = Instant::now();
        session.running.remove(&id.to_string())
    }
}

pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<JsonRpcRequest>,
) -> Response {
    let transport = headers
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok());
    let Some(id) = request.id.clone() else {
        // A request ID is not an authorization boundary. Only the unpredictable
        // transport capability issued to this client can address its requests.
        if request.method == "notifications/cancelled"
            && let (Some(session), Some(id)) = (transport, request.params.get("requestId"))
            && let Some(running) = state.mcp_sessions.take(session, id).await
        {
            let _ = running.cancel.send(true);
            let _ = state.destroy_session(&running.handle).await;
        }
        return StatusCode::NO_CONTENT.into_response();
    };
    let mut new_session = None;
    if request.method == "initialize" {
        match state.mcp_sessions.create().await {
            Ok(session) => new_session = Some(session),
            Err(error) => return rpc_response(id, Err(error)),
        }
    }
    let mut cancellation = None;
    if request.method == "tools/call"
        && matches!(
            request.params["name"].as_str(),
            Some("run" | "localshelld_run")
        )
        && let (Some(session), Some(handle)) =
            (transport, request.params["arguments"]["handle"].as_str())
    {
        match state.mcp_sessions.register(session, &id, handle).await {
            Ok(receiver) => cancellation = Some(receiver),
            Err(error) => return rpc_response(id, Err(error)),
        }
    }
    let result = if let Some(mut cancelled) = cancellation {
        let worker_state = state.clone();
        let worker_session = transport.unwrap().to_owned();
        let worker_id = id.clone();
        let worker = tokio::spawn(async move {
            let result = dispatch(&worker_state, &request.method, request.params).await;
            // Finish the transport bookkeeping even if the HTTP caller left.
            worker_state
                .mcp_sessions
                .take(&worker_session, &worker_id)
                .await;
            result
        });
        let result = tokio::select! {
            biased;
            _ = async {
                if cancelled.wait_for(|cancel| *cancel).await.is_err() {
                    // Normal completion drops the sender without cancelling.
                    std::future::pending::<()>().await;
                }
            } => return StatusCode::NO_CONTENT.into_response(),
            result = worker => result.unwrap_or_else(|_| Err(ApiError::internal("MCP execution worker failed"))),
        };
        result
    } else {
        dispatch(&state, &request.method, request.params).await
    };
    let mut response = rpc_response(id, result);
    if let Some(session) = new_session {
        response
            .headers_mut()
            .insert(SESSION_HEADER, HeaderValue::from_str(&session).unwrap());
    }
    response
}

fn rpc_response(id: Value, result: Result<Value, ApiError>) -> Response {
    let response = match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32602, "message": error.to_string() }
        }),
    };
    Json(response).into_response()
}

async fn dispatch(state: &AppState, method: &str, params: Value) -> Result<Value, ApiError> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "trustd", "version": crate::VERSION },
            "instructions": format!("{} Connected daemon: {:?}.", server_instructions(), state.mode)
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(state, params).await,
        _ => Err(ApiError::bad_request(unknown_method(method))),
    }
}

async fn call_tool(state: &AppState, params: Value) -> Result<Value, ApiError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request(MISSING_TOOL_NAME))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let operation =
        Operation::from_name(name).ok_or_else(|| ApiError::bad_request(unknown_tool(name)))?;
    tool_json(api::execute(state, operation, arguments).await?)
}

fn tool_json(value: Value) -> Result<Value, ApiError> {
    Ok(json!({
        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default() }],
        "structuredContent": value,
        "isError": false
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::{privilege_arg, secret_keys_arg, timeout_arg},
        execution::PrivilegeLevel,
    };

    #[test]
    fn exposes_all_required_tools_and_privacy_guidance() {
        let definitions = tool_definitions();
        let text = definitions.to_string();
        for tool in [
            "secrets_list",
            "secrets_get",
            "enter_interactive",
            "enter_preauthorized",
            "run",
            "destroy",
            "revoke",
        ] {
            assert!(text.contains(tool));
        }
        assert!(server_instructions().contains("user-authorized shell access"));
        assert!(server_instructions().contains(crate::shell::ShellKind::native().name()));
        assert!(text.contains("Respect the user's personal privacy."));
        assert!(text.contains("reused"));
        let plaintext = definitions
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "secrets_get")
            .unwrap();
        assert!(plaintext["description"].as_str().unwrap().contains(
            "Discouraged unless the user explicitly requests plaintext. Prefer environment injection."
        ));
        assert_eq!(
            plaintext["inputSchema"]["required"],
            json!(["handle", "key"])
        );
    }

    #[test]
    fn validates_secret_key_arguments_without_coercion() {
        assert!(secret_keys_arg(&json!({})).unwrap().is_empty());
        assert_eq!(
            secret_keys_arg(&json!({"secret_keys":["B","A","A"]})).unwrap(),
            ["A", "B"]
        );
        for value in [
            json!(null),
            json!("KEY"),
            json!([123]),
            json!(["lowercase"]),
            json!(["LOCALSHELLD_SESSION"]),
        ] {
            assert!(secret_keys_arg(&json!({"secret_keys":value})).is_err());
        }
    }

    #[tokio::test]
    async fn secret_listing_and_enter_never_return_values_implicitly() {
        let mut auth = crate::auth::AuthManager::new("unused".into(), None);
        let token = auth.issue_token(None).unwrap().0;
        let state = AppState::new(crate::config::Config::default(), auth);
        state
            .put_secret("JENKINS_CI_TOKEN", "fixture-secret-123")
            .await
            .unwrap();
        let listed = call_tool(&state, json!({"name":"secrets_list"}))
            .await
            .unwrap();
        assert_eq!(
            listed["structuredContent"],
            json!({"keys":["JENKINS_CI_TOKEN"]})
        );
        assert!(!listed.to_string().contains("fixture-secret-123"));
        for name in ["secrets_get", "secret_get", "secrets_reveal"] {
            assert!(
                call_tool(
                    &state,
                    json!({"name":name,"arguments":{"key":"JENKINS_CI_TOKEN","token":token}})
                )
                .await
                .is_err()
            );
        }
        let entered = call_tool(&state, json!({"name":"enter_preauthorized","arguments":{"token":token,"confirm_text":"OK","secret_keys":["JENKINS_CI_TOKEN"]}})).await.unwrap();
        assert_eq!(
            entered["structuredContent"]["secret_keys"],
            json!(["JENKINS_CI_TOKEN"])
        );
        assert!(!entered.to_string().contains("fixture-secret-123"));
        let revealed = call_tool(
            &state,
            json!({"name":"secrets_get","arguments":{
                "handle":entered["structuredContent"]["handle"],"key":"JENKINS_CI_TOKEN"
            }}),
        )
        .await
        .unwrap();
        assert_eq!(
            revealed["structuredContent"],
            json!({"key":"JENKINS_CI_TOKEN","value":"fixture-secret-123"})
        );
        assert_eq!(
            serde_json::from_str::<Value>(revealed["content"][0]["text"].as_str().unwrap())
                .unwrap(),
            revealed["structuredContent"]
        );
        state.shutdown().await;
    }

    #[test]
    fn validates_optional_unbounded_timeout() {
        for value in [json!({}), json!({"timeout_seconds": null})] {
            assert_eq!(timeout_arg(&value).unwrap(), None);
        }
        assert_eq!(
            timeout_arg(&json!({"timeout_seconds": 3600})).unwrap(),
            Some(3600)
        );
        for value in [json!(0), json!(-1), json!(1.5), json!("10"), json!(true)] {
            assert!(timeout_arg(&json!({"timeout_seconds": value})).is_err());
        }
    }

    #[test]
    fn schemas_and_identity_validation_are_platform_specific() {
        let definitions = tool_definitions();
        for tool in definitions.as_array().unwrap() {
            assert!(tool["name"].as_str().unwrap().len() + "mcp__localshelld__".len() <= 64);
            assert!(tool["outputSchema"].is_object());
        }
        let run = definitions
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "run")
            .unwrap();
        if cfg!(windows) {
            assert_eq!(
                run["inputSchema"]["properties"]["privilege_level"]["enum"],
                json!(["nt-authority/system", "as-administrator", "current-user"])
            );
            assert!(run["inputSchema"]["properties"].get("sudo").is_none());
            for invalid in [
                json!({"sudo":true}),
                json!({"privilege_level":"root"}),
                json!({"privilege_level":true}),
                json!({"privilege_level":"current-user","sudo":false}),
            ] {
                assert!(privilege_arg(&invalid).is_err());
            }
        } else {
            assert_eq!(run["inputSchema"]["properties"]["sudo"]["type"], "boolean");
            assert!(privilege_arg(&json!({"privilege_level":"current-user"})).is_err());
            assert_eq!(
                privilege_arg(&json!({"sudo":true})).unwrap(),
                PrivilegeLevel::Root
            );
        }
    }

    #[tokio::test]
    async fn cancellation_requires_the_originating_transport_capability() {
        use axum::{body::Body, http::Request};
        use tower::ServiceExt;
        let mut auth = crate::auth::AuthManager::new("unused".into(), None);
        let token = auth.issue_token(None).unwrap().0;
        let state = AppState::user(
            crate::config::Config {
                bind: "127.0.0.1:32120".parse().unwrap(),
                ..Default::default()
            },
            auth,
        )
        .unwrap();
        let entered = state.enter(&token).await.unwrap();
        state
            .run(&entered.handle, "echo ready", false, Some(10))
            .await
            .unwrap();
        let app = crate::server::router(state.clone());
        let initialize = || {
            Request::post("/mcp")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"jsonrpc":"2.0","id":1,"method":"initialize"}).to_string(),
                ))
                .unwrap()
        };
        let first = app.clone().oneshot(initialize()).await.unwrap();
        let first = first.headers()[SESSION_HEADER].to_str().unwrap().to_owned();
        let second = app.clone().oneshot(initialize()).await.unwrap();
        let second = second.headers()[SESSION_HEADER]
            .to_str()
            .unwrap()
            .to_owned();
        assert_ne!(first, second);
        // Dropping the cancellation sender after normal completion must not be
        // confused with a real cancel notification (which has no response).
        let mut normal = crate::execution::execution_argument(PrivilegeLevel::CurrentUser);
        normal["handle"] = json!(entered.handle);
        normal["command"] = json!("echo completed");
        let response = app.clone().oneshot(Request::post("/mcp")
            .header("content-type", "application/json").header(SESSION_HEADER, &first)
            .body(Body::from(json!({"jsonrpc":"2.0","id":16,"method":"tools/call","params":{"name":"run","arguments":normal}}).to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            state
                .mcp_sessions
                .0
                .lock()
                .await
                .get(&first)
                .unwrap()
                .running
                .is_empty()
        );

        // Disconnecting a caller does not leave a permanent request-ID entry.
        normal["command"] = json!(if cfg!(windows) {
            "Start-Sleep -Milliseconds 500"
        } else {
            "sleep 0.5"
        });
        let disconnected = tokio::spawn(app.clone().oneshot(Request::post("/mcp")
            .header("content-type", "application/json").header(SESSION_HEADER, &first)
            .body(Body::from(json!({"jsonrpc":"2.0","id":18,"method":"tools/call","params":{"name":"run","arguments":normal}}).to_string())).unwrap()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        disconnected.abort();
        tokio::time::timeout(Duration::from_secs(5), async {
            while state
                .mcp_sessions
                .0
                .lock()
                .await
                .get(&first)
                .unwrap()
                .running
                .contains_key("18")
            {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("disconnected request bookkeeping leaked");
        let mut arguments = crate::execution::execution_argument(PrivilegeLevel::CurrentUser);
        arguments["handle"] = json!(entered.handle);
        arguments["command"] = json!(if cfg!(windows) {
            "Start-Sleep -Seconds 120"
        } else {
            "sleep 120"
        });
        let request = Request::post("/mcp").header("content-type","application/json").header(SESSION_HEADER,&first)
            .body(Body::from(json!({"jsonrpc":"2.0","id":17,"method":"tools/call","params":{"name":"run","arguments":arguments}}).to_string())).unwrap();
        let running = tokio::spawn(app.clone().oneshot(request));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if state
                    .mcp_sessions
                    .0
                    .lock()
                    .await
                    .get(&first)
                    .unwrap()
                    .running
                    .contains_key("17")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let cancel = |session: &str| {
            Request::post("/mcp").header("content-type","application/json").header(SESSION_HEADER,session)
            .body(Body::from(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":17}}).to_string())).unwrap()
        };
        assert_eq!(
            app.clone().oneshot(cancel(&second)).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
        assert!(!running.is_finished());
        assert_eq!(
            app.clone().oneshot(cancel(&first)).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), running)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .status(),
            StatusCode::NO_CONTENT
        );
        assert!(
            state
                .run(&entered.handle, "echo never", false, None)
                .await
                .is_err()
        );
        state.shutdown().await;
    }
}
