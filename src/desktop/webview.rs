//! The user daemon owns the native event loop on its main thread. HTTP, disk
//! writes and authorization waits run on Tokio workers, never on this loop.
use std::{borrow::Cow, collections::HashMap};

use anyhow::{Context, Result};
use tao::{
    dpi::LogicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget},
    window::{Window, WindowBuilder, WindowId},
};
use tokio::sync::oneshot;
use wry::{NewWindowResponse, WebView, WebViewBuilder};

use crate::{
    assets,
    consent::{Choice, ConsentBroker, Decision, DesktopEvent, Prompt},
    server::ApiError,
};

enum GuiEvent {
    Broker(DesktopEvent),
    Decision(String, Decision),
    Stop(i32),
}

pub struct Desktop {
    event_loop: EventLoop<GuiEvent>,
}

struct ConsentWindow {
    // Drop the webview before its owning window.
    _webview: WebView,
    window: Window,
    prompt: Prompt,
    reply: Option<oneshot::Sender<Decision>>,
}

pub fn available() -> bool {
    #[cfg(target_os = "linux")]
    {
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

impl Desktop {
    pub fn new() -> Result<Self> {
        Ok(Self {
            event_loop: EventLoopBuilder::<GuiEvent>::with_user_event().build(),
        })
    }

    pub fn broker(&self) -> ConsentBroker {
        let proxy = self.event_loop.create_proxy();
        ConsentBroker::new(move |event| {
            proxy
                .send_event(GuiEvent::Broker(event))
                .map_err(|_| ApiError::unavailable("desktop event loop stopped"))
        })
    }

    pub fn stop_callback(&self) -> impl Fn(i32) + Send + 'static {
        let proxy = self.event_loop.create_proxy();
        move |code| {
            let _ = proxy.send_event(GuiEvent::Stop(code));
        }
    }

    pub fn run(self) -> ! {
        let mut windows: HashMap<WindowId, ConsentWindow> = HashMap::new();
        let proxy = self.event_loop.create_proxy();
        self.event_loop.run(move |event, target, flow| {
            *flow = ControlFlow::Wait;
            match event {
                Event::UserEvent(GuiEvent::Broker(DesktopEvent::Show(prompt, reply))) => {
                    if prompt.expires_at_ms <= chrono::Utc::now().timestamp_millis()
                        || reply.is_closed()
                    {
                        return;
                    }
                    match create_window(target, proxy.clone(), prompt, reply) {
                        Ok(window) => {
                            windows.insert(window.window.id(), window);
                        }
                        Err(error) => tracing::error!(%error, "unable to show consent window"),
                    }
                }
                Event::UserEvent(GuiEvent::Broker(DesktopEvent::Close(id))) => {
                    windows.retain(|_, window| window.prompt.id != id);
                }
                Event::UserEvent(GuiEvent::Decision(id, decision)) => {
                    if let Some(window_id) = windows
                        .iter()
                        .find(|(_, window)| window.prompt.id == id)
                        .map(|(id, _)| *id)
                        && let Some(mut window) = windows.remove(&window_id)
                        && chrono::Utc::now().timestamp_millis() < window.prompt.expires_at_ms
                        && let Some(reply) = window.reply.take()
                    {
                        let _ = reply.send(decision);
                    }
                }
                Event::WindowEvent {
                    window_id,
                    event: WindowEvent::CloseRequested,
                    ..
                } => {
                    if let Some(mut window) = windows.remove(&window_id)
                        && let Some(reply) = window.reply.take()
                    {
                        let _ = reply.send(Decision {
                            choice: Choice::Ignore,
                            remember: false,
                            btw: String::new(),
                        });
                    }
                }
                Event::UserEvent(GuiEvent::Stop(code)) => {
                    windows.clear();
                    *flow = ControlFlow::ExitWithCode(code);
                }
                Event::UserEvent(GuiEvent::Broker(DesktopEvent::Shutdown)) => {
                    windows.clear();
                    *flow = ControlFlow::Exit;
                }
                _ => {}
            }
        })
    }
}

fn trusted_url(url: &str) -> bool {
    matches!(
        url,
        "localshelld://desktop/consent.html" | "http://localshelld.desktop/consent.html"
    )
}

fn create_window(
    target: &EventLoopWindowTarget<GuiEvent>,
    proxy: EventLoopProxy<GuiEvent>,
    prompt: Prompt,
    reply: oneshot::Sender<Decision>,
) -> Result<ConsentWindow> {
    let window = WindowBuilder::new()
        .with_title("本机命令授权 — localshelld")
        .with_inner_size(LogicalSize::new(
            640.0,
            if prompt.secret_keys.is_empty() {
                660.0
            } else {
                780.0
            },
        ))
        .with_min_inner_size(LogicalSize::new(420.0, 500.0))
        .with_visible(false)
        .build(target)
        .context("native consent window creation failed")?;
    let id = prompt.id.clone();
    let initialization = format!(
        "window.__LOCALSHELLD_PROMPT__ = {};",
        serde_json::to_string(&prompt)?
    );
    let builder = WebViewBuilder::new()
        .with_devtools(false)
        .with_initialization_script(initialization)
        .with_custom_protocol("localshelld".into(), |_, request| {
            let path = request.uri().path();
            let file = if path == "/consent.html" || path.starts_with("/assets/") {
                assets::asset(path)
            } else {
                None
            };
            let (body, mime, status) = file.map(|(body, mime)| (body, mime, 200)).unwrap_or((
                b"Not found",
                "text/plain".into(),
                404,
            ));
            wry::http::Response::builder()
                .status(status)
                .header("Content-Type", mime)
                .header("Content-Security-Policy", assets::CSP)
                .header("X-Content-Type-Options", "nosniff")
                .body(Cow::Borrowed(body))
                .expect("valid embedded asset response")
        })
        .with_navigation_handler(|url| trusted_url(&url))
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
        .with_ipc_handler(move |request| {
            if !trusted_url(&request.uri().to_string()) || request.body().len() > 32_768 {
                return;
            }
            if let Ok(decision) = serde_json::from_str::<Decision>(request.body()) {
                let _ = proxy.send_event(GuiEvent::Decision(id.clone(), decision));
            }
        })
        .with_url("localshelld://desktop/consent.html");
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window)?;
    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder.build_gtk(
            window
                .default_vbox()
                .context("missing native window container")?,
        )?
    };
    window.set_visible(true);
    window.set_focus();
    Ok(ConsentWindow {
        _webview: webview,
        window,
        prompt,
        reply: Some(reply),
    })
}
