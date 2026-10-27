//! Telegram is a presentation channel, never an execution endpoint. Decisions
//! are candidates until the requesting device's single arbiter acknowledges one.
#![forbid(unsafe_code)]
mod api;
mod service;

pub use api::Api;
pub use localshelld_sync_protocol::{TelegramBinding as Binding, TelegramConfig as Config};
use serde::{Deserialize, Serialize};
pub use service::Service;

pub type Result<T> = std::result::Result<T, &'static str>;

#[derive(Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Status,
    Configure {
        token: Option<String>,
        enabled: bool,
    },
    Remove,
    BindStart {
        #[serde(default)]
        code: Option<String>,
    },
    BindPoll {
        code: String,
    },
    Test,
}
impl Drop for Action {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        if let Self::Configure {
            token: Some(token), ..
        } = self
        {
            token.zeroize();
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Prompt {
    pub id: String,
    pub agent_name: String,
    pub request_reason: String,
    pub scope: String,
    #[serde(default)]
    pub secret_keys: Vec<String>,
    pub expires_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    Accept,
    Deny,
    Ignore,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Decision {
    pub choice: Choice,
    pub remember: bool,
    #[serde(default)]
    pub btw: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    Telegram,
    Other,
    Expired,
    Cancelled,
}

impl Completion {
    pub fn label(self, decision: Option<&Decision>) -> &'static str {
        match self {
            Self::Other => "已经在其它端操作",
            Self::Expired => "授权请求已超时",
            Self::Cancelled => "授权请求已取消",
            Self::Telegram => match decision.map(|d| d.choice) {
                Some(Choice::Accept) => "已接受",
                Some(Choice::Deny) => "已拒绝",
                _ => "已忽略",
            },
        }
    }
}

pub fn validate_prompt(prompt: &Prompt) -> Result<()> {
    if prompt.id.is_empty()
        || prompt.id.len() > 48
        || !prompt
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        || prompt.agent_name.trim().is_empty()
        || prompt.agent_name.chars().count() > 128
        || prompt.agent_name.chars().any(char::is_control)
        || prompt.scope.len() > 1024
        || prompt.request_reason.trim().is_empty()
        || prompt.request_reason.len() > 16384
        || prompt.request_reason.contains('\0')
        || prompt.secret_keys.len() > 64
        || prompt
            .secret_keys
            .iter()
            .any(|k| localshelld_sync_protocol::validate_injection_key(k).is_err())
    {
        return Err("invalid Telegram authorization request");
    }
    let remaining = prompt
        .expires_at_ms
        .saturating_sub(chrono::Utc::now().timestamp_millis());
    if !(1..=61_000).contains(&remaining) {
        return Err("authorization request expired or has an invalid deadline");
    }
    Ok(())
}
