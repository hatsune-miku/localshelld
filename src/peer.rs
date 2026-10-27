//! User-to-privileged daemon calls. Never forward a password or follow redirects.
use std::{net::SocketAddr, time::Duration};

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    execution::{PrivilegeLevel, RunOptions, execution_argument},
    server::ApiError,
    shell::ExecutionResult,
};

#[derive(Clone)]
pub(crate) struct Peer {
    client: reqwest::Client,
    base: String,
    response_limit: usize,
}

#[derive(Deserialize)]
pub(crate) struct Entered {
    pub handle: String,
    pub expires_at: Option<i64>,
    pub daemon: String,
    #[serde(default)]
    pub identities: std::collections::BTreeMap<String, Value>,
    #[serde(default)]
    pub secret_keys: Vec<String>,
}

#[derive(Deserialize)]
pub(crate) struct Inspection {
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub identities: std::collections::BTreeMap<String, Value>,
    #[serde(default)]
    pub secret_keys: Vec<String>,
}

impl Peer {
    pub fn new(address: SocketAddr, max_output: usize) -> Result<Self, ApiError> {
        if !address.ip().is_loopback() {
            return Err(ApiError::bad_request(
                "privileged daemon must be on loopback",
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .build()
            .map_err(|_| ApiError::internal("unable to initialize daemon client"))?;
        Ok(Self {
            client,
            base: format!("http://{address}"),
            response_limit: max_output.saturating_mul(32).saturating_add(65_536),
        })
    }

    async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Value,
        timeout: Option<Duration>,
    ) -> Result<T, ApiError> {
        let mut request = self.client.post(format!("{}{path}", self.base)).json(&body);
        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }
        let mut response = request.send().await.map_err(|_| {
            ApiError::unavailable("privileged daemon unavailable; command was not retried")
        })?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| {
            ApiError::unavailable("privileged daemon response interrupted; command was not retried")
        })? {
            if bytes.len().saturating_add(chunk.len()) > self.response_limit {
                return Err(ApiError::unavailable(
                    "privileged daemon response exceeds size limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let message = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| value["error"].as_str().map(str::to_owned));
            return Err(message.map_or_else(
                || ApiError::peer(status),
                |message| ApiError::with_status(status.as_u16(), message),
            ));
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| ApiError::unavailable("invalid privileged daemon response"))
    }

    pub async fn enter(&self, token: &str, secret_keys: &[String]) -> Result<Entered, ApiError> {
        let entered: Entered = self
            .post(
                "/v1/sessions/enter",
                json!({"token": token, "secret_keys":secret_keys}),
                Some(Duration::from_secs(5)),
            )
            .await?;
        if entered.daemon != "privileged" {
            return Err(ApiError::bad_request(
                "upstream must be a privileged daemon",
            ));
        }
        if !secret_keys.is_empty() && entered.secret_keys != secret_keys {
            return Err(ApiError::unavailable(
                "privileged daemon did not authorize the requested secret keys; upgrade both daemons to the same version",
            ));
        }
        Ok(entered)
    }

    pub async fn run(
        &self,
        handle: &str,
        command: &str,
        level: PrivilegeLevel,
        options: &RunOptions,
    ) -> Result<ExecutionResult, ApiError> {
        let mut body = execution_argument(level);
        body.as_object_mut().unwrap().extend(json!({"handle":handle,"command":command,"stdin":options.stdin,"timeout_seconds":options.timeout_seconds}).as_object().unwrap().clone());
        self.post("/v1/commands/run", body, None).await
    }

    pub async fn validate(&self, handle: &str, watch: bool) -> Result<(), ApiError> {
        let path = if watch {
            "/v1/sessions/watch"
        } else {
            "/v1/sessions/validate"
        };
        let _: Value = self
            .post(
                path,
                json!({"handle": handle}),
                Some(Duration::from_secs(20)),
            )
            .await?;
        Ok(())
    }

    pub async fn inspect(&self, handle: &str) -> Result<Inspection, ApiError> {
        let result: Inspection = self
            .post(
                "/v1/sessions/validate",
                json!({"handle":handle}),
                Some(Duration::from_secs(5)),
            )
            .await?;
        Ok(result)
    }

    pub async fn destroy(&self, handle: &str) -> Result<(), ApiError> {
        let _: Value = self
            .post(
                "/v1/sessions/destroy",
                json!({"handle": handle}),
                Some(Duration::from_secs(5)),
            )
            .await?;
        Ok(())
    }

    pub async fn revoke(&self, token: &str) -> Result<(), ApiError> {
        let _: Value = self
            .post(
                "/v1/tokens/revoke",
                json!({"token": token}),
                Some(Duration::from_secs(5)),
            )
            .await?;
        Ok(())
    }
}
