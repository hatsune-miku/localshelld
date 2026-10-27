//! The only live-consent entry point. No HTTP route can submit a decision.
//! Desktop implementations receive an in-process, one-shot reply capability.
use std::{sync::Arc, time::Duration};

use serde::{Deserialize, Serialize};
use tokio::{sync::oneshot, time::Instant};

use crate::server::ApiError;

pub const CONSENT_TIMEOUT: Duration = Duration::from_millis(60_000);
pub const MAX_PENDING: usize = 8;

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    pub choice: Choice,
    pub remember: bool,
    #[serde(default)]
    pub btw: String,
}

pub enum DesktopEvent {
    Show(Prompt, oneshot::Sender<Decision>),
    Close(String),
    Shutdown,
}

type EventSink = dyn Fn(DesktopEvent) -> Result<(), ApiError> + Send + Sync;

#[derive(Clone)]
pub struct ConsentBroker {
    sink: Arc<EventSink>,
    permits: Arc<tokio::sync::Semaphore>,
}

impl ConsentBroker {
    pub fn new(
        sink: impl Fn(DesktopEvent) -> Result<(), ApiError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            sink: Arc::new(sink),
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_PENDING)),
        }
    }

    pub async fn request(&self, prompt: Prompt) -> Result<Decision, ApiError> {
        let _permit = self
            .permits
            .try_acquire()
            .map_err(|_| ApiError::unavailable("too many pending authorization requests"))?;
        let remaining = prompt
            .expires_at_ms
            .saturating_sub(chrono::Utc::now().timestamp_millis());
        let deadline = Instant::now() + Duration::from_millis(remaining.clamp(0, 60_000) as u64);
        let id = prompt.id.clone();
        let (reply, receive) = oneshot::channel();
        (self.sink)(DesktopEvent::Show(prompt, reply))?;
        // Drop cleanup also runs when an MCP client cancels its HTTP request.
        let _close = CloseOnDrop {
            sink: self.sink.clone(),
            id,
        };
        let mut btw = String::new();
        if let Ok(Ok(decision)) = tokio::time::timeout_at(deadline, receive).await {
            if decision.btw.len() > 16_384 {
                return Err(ApiError::bad_request("authorization note is too long"));
            }
            // Deliver Ignore (including its note) to the authority immediately.
            // The authority, not a potentially lossy IPC round trip, owns the
            // remaining wait until the original 60-second deadline.
            if Instant::now() < deadline {
                return Ok(decision);
            }
            btw = decision.btw;
        }
        // Ignore, closing the window and losing the desktop do not authorize or
        // deny a request. Only the original fixed deadline times it out.
        tokio::time::sleep_until(deadline).await;
        Ok(Decision {
            choice: Choice::Ignore,
            remember: false,
            btw,
        })
    }
}

struct CloseOnDrop {
    sink: Arc<EventSink>,
    id: String,
}
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        let _ = (self.sink)(DesktopEvent::Close(self.id.clone()));
    }
}

pub fn validate_request(agent_name: &str, request_reason: &str) -> Result<(), ApiError> {
    if agent_name.trim().is_empty()
        || agent_name.chars().count() > 128
        || agent_name.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(
            "agent_name must contain 1–128 characters without control characters",
        ));
    }
    if request_reason.trim().is_empty()
        || request_reason.len() > 16_384
        || request_reason.contains('\0')
    {
        return Err(ApiError::bad_request(
            "request_reason must be nonempty and at most 16384 bytes",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn prompt() -> Prompt {
        Prompt {
            id: "request".into(),
            agent_name: "agent".into(),
            request_reason: "reason <script> unchanged".into(),
            scope: "user".into(),
            secret_keys: vec![],
            expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ignore_preserves_note_for_the_authoritys_deadline_wait() {
        let broker = ConsentBroker::new(|event| {
            if let DesktopEvent::Show(_, reply) = event {
                let _ = reply.send(Decision {
                    choice: Choice::Ignore,
                    remember: true,
                    btw: "later".into(),
                });
            }
            Ok(())
        });
        let started = Instant::now();
        let result = broker.request(prompt()).await.unwrap();
        assert!(started.elapsed() < CONSENT_TIMEOUT);
        assert_eq!(result.choice, Choice::Ignore);
        assert!(result.remember);
        assert_eq!(result.btw, "later");
    }

    #[tokio::test]
    async fn accepts_only_a_desktop_reply() {
        let broker = ConsentBroker::new(|event| {
            if let DesktopEvent::Show(prompt, reply) = event {
                assert_eq!(prompt.request_reason, "reason <script> unchanged");
                let _ = reply.send(Decision {
                    choice: Choice::Accept,
                    remember: true,
                    btw: "yes".into(),
                });
            }
            Ok(())
        });
        assert_eq!(
            broker.request(prompt()).await.unwrap().choice,
            Choice::Accept
        );
        assert!(validate_request("", "reason").is_err());
        assert!(validate_request("agent", " ").is_err());
    }
}
