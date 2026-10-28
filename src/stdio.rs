//! Process-scoped MCP adapter. Bearer credentials never cross its stdio boundary.
//! This is not a chat boundary if the host shares one adapter across conversations.
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Mutex, watch},
    task::JoinSet,
};
use zeroize::Zeroizing;

const MAX_MESSAGE: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 64 * 1024 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(75);

pub struct Adapter {
    endpoint: String,
    client: reqwest::Client,
    token: Option<Zeroizing<String>>,
    handle: Mutex<Option<Zeroizing<String>>>,
    // Only lifecycle transitions are serialized. A long run never prevents close/cancel.
    lifecycle: Mutex<()>,
    closed: AtomicBool,
}

impl Adapter {
    pub fn new(endpoint: SocketAddr, token: Option<Zeroizing<String>>) -> Result<Self> {
        ensure!(
            endpoint.ip().is_loopback() && endpoint.port() != 0,
            "stdio requires a literal loopback daemon address"
        );
        ensure!(
            token.as_ref().is_none_or(|value| !value.is_empty()),
            "configured token is empty"
        );
        Ok(Self {
            endpoint: format!("http://{endpoint}"),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .build()?,
            token,
            handle: Mutex::new(None),
            lifecycle: Mutex::new(()),
            closed: AtomicBool::new(false),
        })
    }

    fn definitions(&self) -> Value {
        let mut definitions = crate::mcp_text::tool_definitions();
        let entries = definitions.as_array_mut().unwrap();
        entries.retain(|entry| {
            self.token.is_some()
                || !matches!(
                    entry["name"].as_str(),
                    Some("enter_preauthorized" | "revoke")
                )
        });
        for entry in entries {
            for schema in ["inputSchema", "outputSchema"] {
                remove_field(&mut entry[schema], "handle");
                remove_field(&mut entry[schema], "token");
            }
            match entry["name"].as_str().unwrap() {
                "enter_interactive" => {
                    entry["description"] = json!(
                        "Ask the owner for access through desktop/Telegram. This process holds the resulting session internally. No token or handle is supplied or returned. Names are self-reported. Read status and the owner's btw note. Close the existing session before requesting a new one. Approval has a 60 second deadline; allow more than 60 seconds at the client."
                    )
                }
                "enter_preauthorized" => {
                    entry["description"] = json!(
                        "Enter using the token configured by the owner in the adapter's environment. Never ask the model to copy a token. One token must not be shared by multiple adapter processes. Close the current session first."
                    )
                }
                "run" => {
                    entry["description"] = json!(
                        "Execute in this adapter's authorized session. Explicitly select the execution identity. Variables and working directory persist separately for each identity. No default execution timeout. Cancellation or timeout closes the entire session and its process trees. MCP cancellation works without a handle or transport header. Do not retry commands after an ambiguous transport failure."
                    )
                }
                "destroy" => {
                    entry["description"] = json!(
                        "Close this adapter's current session and terminate its process trees. Idempotent when no session is open."
                    )
                }
                "revoke" => {
                    entry["description"] = json!(
                        "Revoke the owner-configured token, terminating all sessions it owns. No token argument."
                    )
                }
                "secrets_get" => {
                    entry["description"] = json!(
                        "Discouraged unless the user explicitly requests plaintext. Prefer environment injection. Read one value from this adapter's authorized secret_keys snapshot."
                    )
                }
                _ => {}
            }
        }
        definitions
    }

    async fn request(&self, path: &str, args: Option<Value>, unlimited: bool) -> Result<Value> {
        let url = format!("{}{path}", self.endpoint);
        let mut request = if let Some(args) = args {
            self.client.post(url).json(&args)
        } else {
            self.client.get(url)
        };
        if !unlimited {
            request = request.timeout(CONTROL_TIMEOUT);
        }
        let mut response = request
            .send()
            .await
            .context("daemon request failed; outcome may be unknown; do not automatically retry")?;
        let status = response.status();
        // The daemon returns 401 when this handle has already been removed
        // (expiry, revocation, restart or another in-flight cleanup). An explicit
        // close can forget that stale handle, but transport/5xx failures cannot.
        if path == "/v1/sessions/destroy" && status == reqwest::StatusCode::UNAUTHORIZED {
            return Ok(json!({"destroyed":true}));
        }
        // Error bodies are deliberately not relayed: they can contain bearer credentials.
        ensure!(
            status.is_success(),
            "daemon returned HTTP {}; authorization may have expired; close/re-authorize if needed",
            status.as_u16()
        );
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(chunk) = response
            .chunk()
            .await
            .context("daemon response interrupted; outcome may be unknown")?
        {
            ensure!(
                bytes.len().saturating_add(chunk.len()) <= MAX_RESPONSE,
                "daemon response too large"
            );
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).context("invalid daemon response")
    }

    async fn destroy_handle(&self, handle: &str) -> Result<()> {
        self.request(
            "/v1/sessions/destroy",
            Some(json!({"handle":handle})),
            false,
        )
        .await?;
        Ok(())
    }

    async fn close(&self) -> Result<Value> {
        let _transition = self.lifecycle.lock().await;
        let handle = self.handle.lock().await.clone();
        if let Some(handle) = handle {
            // Retain on failure so the owner can retry cleanup. Never substitute another session.
            self.destroy_handle(&handle).await?;
            self.handle.lock().await.take();
        }
        Ok(json!({"destroyed":true}))
    }

    async fn call(&self, params: Value, mut cancelled: watch::Receiver<bool>) -> Result<Value> {
        let name = params["name"].as_str().context("missing tool name")?;
        let definition = self
            .definitions()
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == name)
            .cloned()
            .context("unknown tool")?;
        let mut args = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let object = args.as_object().context("arguments must be an object")?;
        let properties = definition["inputSchema"]["properties"].as_object().unwrap();
        ensure!(
            object.keys().all(|key| properties.contains_key(key)),
            "unknown argument; use this adapter's tool schema"
        );
        for required in definition["inputSchema"]["required"]
            .as_array()
            .into_iter()
            .flatten()
        {
            ensure!(
                object.contains_key(required.as_str().unwrap()),
                "missing required argument"
            );
        }
        ensure!(
            !self.closed.load(Ordering::SeqCst) && !*cancelled.borrow(),
            "request cancelled"
        );
        match name {
            "secrets_list" => self.request("/v1/secrets/keys", None, false).await,
            "enter_interactive" | "enter_preauthorized" => {
                let _transition = self.lifecycle.lock().await;
                ensure!(
                    !self.closed.load(Ordering::SeqCst) && !*cancelled.borrow(),
                    "request cancelled"
                );
                ensure!(
                    self.handle.lock().await.is_none(),
                    "a session is already open; destroy it before requesting another authorization"
                );
                let path = if name == "enter_interactive" {
                    "/v1/sessions/enter/interactive"
                } else {
                    args["token"] = json!(
                        self.token
                            .as_deref()
                            .context("no owner-configured token")?
                            .as_str()
                    );
                    "/v1/sessions/enter/preauthorized"
                };
                // Do not drop an in-flight authorization on cancellation/EOF: a late approval
                // can create a session. Receive it, then explicitly destroy it.
                let mut result = self.request(path, Some(args), false).await?;
                let handle = result
                    .as_object_mut()
                    .context("invalid authorization response")?
                    .remove("handle");
                if let Some(Value::String(handle)) = handle {
                    let handle = Zeroizing::new(handle);
                    ensure!(
                        result["reused"] != true,
                        "configured token already has a live session; use a separate token per adapter"
                    );
                    *self.handle.lock().await = Some(handle.clone());
                    if self.closed.load(Ordering::SeqCst) || *cancelled.borrow() {
                        self.destroy_handle(&handle).await?;
                        self.handle.lock().await.take();
                        bail!("request cancelled");
                    }
                } else {
                    ensure!(
                        matches!(result["status"].as_str(), Some("denied" | "timed_out")),
                        "daemon did not return an authorization session"
                    );
                }
                Ok(result)
            }
            "destroy" => self.close().await,
            "revoke" => {
                let _transition = self.lifecycle.lock().await;
                let result = self.request("/v1/tokens/revoke", Some(json!({"token":self.token.as_deref().context("no owner-configured token")?.as_str()})), false).await?;
                self.handle.lock().await.take();
                Ok(result)
            }
            "run" | "secrets_get" => {
                let handle = {
                    let _transition = self.lifecycle.lock().await;
                    ensure!(
                        !self.closed.load(Ordering::SeqCst) && !*cancelled.borrow(),
                        "request cancelled"
                    );
                    self.handle
                        .lock()
                        .await
                        .clone()
                        .context("no session; request authorization first")?
                };
                args["handle"] = json!(handle.as_str());
                let request = self.request(
                    if name == "run" {
                        "/v1/commands/run"
                    } else {
                        "/v1/secrets/get"
                    },
                    Some(args),
                    name == "run",
                );
                let result = tokio::select! {
                    result = request => result,
                    _ = cancelled.changed() => {
                        self.destroy_handle(&handle).await?;
                        self.forget(&handle).await;
                        bail!("request cancelled");
                    }
                };
                if result
                    .as_ref()
                    .is_ok_and(|value| value["session_ended"] == true)
                {
                    self.forget(&handle).await;
                }
                result
            }
            _ => bail!("unknown tool"),
        }
    }

    async fn forget(&self, handle: &str) {
        let mut current = self.handle.lock().await;
        if current
            .as_ref()
            .is_some_and(|value| value.as_str() == handle)
        {
            current.take();
        }
    }

    pub async fn serve<R, W>(self, input: R, mut output: W) -> Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let adapter = Arc::new(self);
        let mut reader = BufReader::new(input);
        let mut line_buffer = Vec::new();
        let mut tasks = JoinSet::new();
        let mut pending = HashMap::<String, watch::Sender<bool>>::new();
        let mut initialized = false;
        let outcome = loop {
            tokio::select! {
                _ = crate::daemon::shutdown_signal() => break Ok(()),
                line = read_message(&mut reader, &mut line_buffer) => {
                    let bytes = match line { Ok(Some(line)) => line, Ok(None) => break Ok(()), Err(error) => break Err(error) };
                    let request: Value = match serde_json::from_slice(&bytes) {
                        Ok(value) => value,
                        Err(_) => {
                            if let Err(error) = write_message(&mut output, &rpc_error(Value::Null, -32700, "invalid JSON")).await { break Err(error); }
                            continue;
                        }
                    };
                    let id = request.get("id").cloned();
                    let method = request["method"].as_str();
                    if request["jsonrpc"] != "2.0" || method.is_none() || id.as_ref().is_some_and(|id| !id.is_string() && !id.is_number()) {
                        if let Err(error) = write_message(&mut output, &rpc_error(Value::Null, -32600, "invalid JSON-RPC request")).await { break Err(error); }
                        continue;
                    }
                    let method = method.unwrap();
                    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
                    let Some(id) = id else {
                        if method == "notifications/cancelled"
                            && let Some(cancel) = pending.get(&params["requestId"].to_string()) { let _ = cancel.send(true); }
                        continue;
                    };
                    let key = id.to_string();
                    if pending.contains_key(&key) {
                        if let Err(error) = write_message(&mut output, &rpc_error(id, -32600, "duplicate in-flight request ID")).await { break Err(error); }
                        continue;
                    }
                    let response = match method {
                        "initialize" if !initialized => {
                            initialized = true;
                            json!({"jsonrpc":"2.0","id":id,"result":{
                                "protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},
                                "serverInfo":{"name":"trustd-stdio","version":crate::VERSION},
                                "instructions":"trustd holds one authorization session inside this adapter process. Request only the identities and credential keys needed for the user's task. Handles and configured tokens never appear in tools. One process is one isolation boundary, not necessarily one chat. Closing stdin or cancelling a run attempts to destroy the entire session; forced process termination cannot guarantee cleanup. Explicit credential reads remain available when the user requests plaintext."
                            }})
                        }
                        "ping" => json!({"jsonrpc":"2.0","id":id,"result":{}}),
                        _ if !initialized => rpc_error(id, -32000, "initialize first"),
                        "tools/list" => json!({"jsonrpc":"2.0","id":id,"result":{"tools":adapter.definitions()}}),
                        "tools/call" => {
                            if pending.len() >= 64 {
                                rpc_error(id, -32000, "too many pending requests")
                            } else {
                                let (tx, rx) = watch::channel(false);
                                pending.insert(key.clone(), tx);
                                let adapter = adapter.clone();
                                tasks.spawn(async move { (key, id, adapter.call(params, rx).await) });
                                continue;
                            }
                        }
                        _ => rpc_error(id, -32601, "unsupported method"),
                    };
                    if let Err(error) = write_message(&mut output, &response).await { break Err(error); }
                }
                completed = tasks.join_next(), if !tasks.is_empty() => {
                    let (key, id, result) = match completed.unwrap() { Ok(value) => value, Err(_) => break Err(anyhow::anyhow!("stdio request worker failed")) };
                    let cancelled = pending.remove(&key).is_some_and(|tx| *tx.borrow());
                    if !cancelled {
                        let response = match result {
                            Ok(value) => json!({"content":[{"type":"text","text":serde_json::to_string(&value)?}],"structuredContent":value,"isError":false}),
                            Err(error) => json!({"content":[{"type":"text","text":error.to_string()}],"isError":true}),
                        };
                        if let Err(error) = write_message(&mut output, &json!({"jsonrpc":"2.0","id":id,"result":response})).await { break Err(error); }
                    }
                }
            }
        };
        adapter.closed.store(true, Ordering::SeqCst);
        for cancel in pending.values() {
            let _ = cancel.send(true);
        }
        // Await late authorization replies and cancellation cleanup, but never forever.
        let cleanup = tokio::time::timeout(Duration::from_secs(85), async {
            while tasks.join_next().await.is_some() {}
            adapter.close().await
        })
        .await;
        tasks.abort_all();
        outcome?;
        cleanup.context("session cleanup timed out; inspect the daemon before reauthorizing")??;
        Ok(())
    }
}

fn remove_field(schema: &mut Value, key: &str) {
    if let Some(properties) = schema["properties"].as_object_mut() {
        properties.remove(key);
    }
    if let Some(required) = schema["required"].as_array_mut() {
        required.retain(|value| value != key);
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

async fn write_message(output: &mut (impl AsyncWrite + Unpin), value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    output.write_all(&bytes).await?;
    output.flush().await?;
    Ok(())
}

async fn read_message(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    line: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>> {
    // The caller retains partial bytes if select! cancels this read for a response.
    let count = reader
        .take((MAX_MESSAGE + 1 - line.len()) as u64)
        .read_until(b'\n', line)
        .await?;
    ensure!(line.len() <= MAX_MESSAGE, "MCP message exceeds 1 MiB");
    if count == 0 && line.is_empty() {
        return Ok(None);
    }
    ensure!(line.last() == Some(&b'\n'), "unterminated MCP message");
    Ok(Some(std::mem::take(line)))
}

#[cfg(test)]
mod tests;
