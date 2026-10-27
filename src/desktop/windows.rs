use std::time::{Duration, Instant};

use anyhow::Result;
use localshelld_native_dialog::{Choice as NativeChoice, Event, EventLoop, Request};

use crate::{
    consent::{Choice, ConsentBroker, Decision, DesktopEvent},
    server::ApiError,
};

pub fn available() -> bool {
    true
}

pub struct Desktop(EventLoop);

impl Desktop {
    pub fn new() -> Result<Self> {
        Ok(Self(EventLoop::new()?))
    }

    pub fn broker(&self) -> ConsentBroker {
        let sender = self.0.sender();
        ConsentBroker::new(move |event| {
            let event = match event {
                DesktopEvent::Show(prompt, reply) => {
                    if reply.is_closed() {
                        return Ok(());
                    }
                    let remaining = prompt
                        .expires_at_ms
                        .saturating_sub(chrono::Utc::now().timestamp_millis())
                        .clamp(0, 60_000);
                    Event::Show(Request {
                        id: prompt.id,
                        agent: prompt.agent_name,
                        reason: prompt.request_reason,
                        scope: prompt.scope,
                        secret_keys: prompt.secret_keys,
                        deadline: Instant::now() + Duration::from_millis(remaining as u64),
                        complete: Box::new(move |outcome| {
                            let choice = match outcome.choice {
                                NativeChoice::Accept => Choice::Accept,
                                NativeChoice::Deny => Choice::Deny,
                                NativeChoice::Ignore => Choice::Ignore,
                            };
                            let _ = reply.send(Decision {
                                choice,
                                remember: outcome.remember,
                                btw: outcome.note,
                            });
                        }),
                    })
                }
                DesktopEvent::Close(id) => Event::Close(id),
                DesktopEvent::Shutdown => Event::Stop(0),
            };
            sender
                .send(event)
                .map_err(|_| ApiError::unavailable("native desktop event loop unavailable"))
        })
    }

    pub fn stop_callback(&self) -> impl Fn(i32) + Send + 'static {
        let sender = self.0.sender();
        move |code| {
            let _ = sender.send(Event::Stop(code));
        }
    }

    pub fn run(self) -> ! {
        // EventLoop drops every HWND and GDI resource before returning. Tokio
        // has already finished server shutdown before invoking stop_callback.
        std::process::exit(self.0.run());
    }
}
