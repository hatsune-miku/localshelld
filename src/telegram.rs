//! Configuration and a device-local arbiter for desktop/Telegram consent.
use crate::{
    bridge, config,
    consent::{Choice, Decision, Prompt},
    server::{ApiError, AppState, DaemonMode},
    settings::Settings,
};
pub use localshelld_telegram::Action;
use localshelld_telegram::{Api, Completion, Config, Service};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::oneshot;
use zeroize::Zeroizing;

pub(crate) fn read_config(settings: &Settings) -> Result<Option<Config>, ApiError> {
    settings
        .saved
        .telegram
        .as_ref()
        .map(|sealed| {
            let bytes = config::unseal(sealed, &settings.seal_key)
                .map_err(|_| ApiError::internal("Telegram configuration decryption failed"))?;
            let config: Config = serde_json::from_slice(&bytes)
                .map_err(|_| ApiError::internal("invalid Telegram configuration"))?;
            config.validate().map_err(ApiError::bad_request)?;
            Ok(config)
        })
        .transpose()
}
pub(crate) fn encrypt_config(
    settings: &Settings,
    value: Option<&Config>,
) -> Result<Option<config::SealedSecret>, ApiError> {
    value
        .map(|value| {
            value.validate().map_err(ApiError::bad_request)?;
            let bytes = Zeroizing::new(
                serde_json::to_vec(value)
                    .map_err(|_| ApiError::internal("Telegram encoding failed"))?,
            );
            config::seal(&bytes, &settings.seal_key)
                .map_err(|_| ApiError::internal("Telegram encryption failed"))
        })
        .transpose()
}

impl AppState {
    pub async fn telegram_action(&self, owner: &str, action: Action) -> Result<Value, ApiError> {
        if let Some(pairing) = self.vault_peer().await {
            return bridge::call(
                &pairing,
                DaemonMode::User,
                bridge::Request::Telegram {
                    owner: owner.into(),
                    action,
                },
                Some(Duration::from_secs(60)),
                65536,
            )
            .await;
        }
        let cloud = self.settings.lock().await.saved.sync.is_some();
        if cloud {
            self.telegram
                .configure(None)
                .await
                .map_err(ApiError::bad_request)?;
            let revision = self.read_vault().await?.revision;
            let result = self
                .telegram_cloud_request(
                    "telegram",
                    json!({"expected_revision":revision,"command":action}),
                    Duration::from_secs(30),
                )
                .await?;
            if result["saved"] == true || result["bound"] == true || result["removed"] == true {
                self.read_vault().await?;
            }
            let mut result = result;
            if matches!(action, Action::Status) {
                result["mode"] = json!("cloud");
                result["revision"] = json!(revision);
            }
            return Ok(result);
        }
        // The lock order is sync -> Telegram configuration -> settings.
        let _sync = self.sync.lock().await;
        let _serial = self.telegram_config_lock.lock().await;
        let config = {
            let settings = self.settings.lock().await;
            if settings.saved.sync.is_some() {
                return Err(ApiError::conflict("storage mode changed; retry"));
            }
            read_config(&settings)?
        };
        self.telegram
            .configure(config.clone())
            .await
            .map_err(ApiError::bad_request)?;
        match &action {
            Action::Status => {
                let mut status = self.telegram.status().await;
                status["mode"] = json!("offline");
                Ok(status)
            }
            Action::Configure { token, enabled } => {
                let mut next = if let Some(token) = token {
                    let username = Api::default()
                        .verify(token)
                        .await
                        .map_err(ApiError::bad_request)?;
                    Config {
                        bot_token: token.clone(),
                        bot_username: username,
                        enabled: *enabled,
                        binding: config
                            .as_ref()
                            .filter(|c| c.bot_token == *token)
                            .and_then(|c| c.binding.clone()),
                    }
                } else {
                    config.ok_or_else(|| ApiError::bad_request("请先保存 Bot Token。"))?
                };
                next.enabled = *enabled;
                self.save_local_telegram(Some(next)).await?;
                Ok(json!({"saved":true}))
            }
            Action::Remove => {
                self.save_local_telegram(None).await?;
                Ok(json!({"removed":true}))
            }
            Action::BindStart { code } => self
                .telegram
                .begin_binding(owner, code.as_deref())
                .await
                .map_err(ApiError::bad_request),
            Action::BindPoll { code } => {
                if let Some(binding) = self
                    .telegram
                    .binding(owner, code)
                    .await
                    .map_err(ApiError::bad_request)?
                {
                    let mut config =
                        config.ok_or_else(|| ApiError::bad_request("Bot was removed"))?;
                    config.binding = Some(binding);
                    self.save_local_telegram(Some(config)).await?;
                    Ok(json!({"bound":true}))
                } else {
                    Ok(json!({"pending":true}))
                }
            }
            Action::Test => {
                self.telegram
                    .test_message()
                    .await
                    .map_err(ApiError::bad_request)?;
                Ok(json!({"sent":true}))
            }
        }
    }
    async fn save_local_telegram(&self, config: Option<Config>) -> Result<(), ApiError> {
        let mut settings = self.settings.lock().await;
        let mut next = settings.saved.clone();
        next.telegram = encrypt_config(&settings, config.as_ref())?;
        settings.save(next).await?;
        drop(settings);
        self.telegram
            .configure(config)
            .await
            .map_err(ApiError::bad_request)
    }
    async fn telegram_backend(&self) -> Result<Backend, ApiError> {
        let _serial = self.telegram_config_lock.lock().await;
        let (cloud, config) = {
            let settings = self.settings.lock().await;
            (settings.saved.sync.is_some(), read_config(&settings)?)
        };
        if cloud {
            self.telegram
                .configure(None)
                .await
                .map_err(ApiError::bad_request)?;
            return Ok(Backend::Cloud(self.clone()));
        }
        let config = config
            .filter(|c| c.enabled && c.binding.is_some())
            .ok_or_else(|| ApiError::unavailable("Telegram is not configured"))?;
        self.telegram
            .configure(Some(config))
            .await
            .map_err(ApiError::bad_request)?;
        Ok(Backend::Local(self.telegram.clone()))
    }
    pub(crate) async fn request_consent(&self, prompt: Prompt) -> Result<Decision, ApiError> {
        crate::consent::validate_request(&prompt.agent_name, &prompt.request_reason)?;
        let remaining = prompt
            .expires_at_ms
            .saturating_sub(chrono::Utc::now().timestamp_millis())
            .clamp(0, 60_000);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(remaining as u64);
        let mut telegram = Channel::start(self.clone(), prompt.clone(), deadline);
        let desktop = async {
            match &self.consent {
                Some(broker) => broker.request(prompt).await,
                None => Err(ApiError::unavailable("desktop channel is unavailable")),
            }
        };
        tokio::pin!(desktop);
        let mut desktop_done = false;
        let mut telegram_done = false;
        let (decision, completion) = loop {
            tokio::select! {
                _=tokio::time::sleep_until(deadline)=>break (Decision {choice:Choice::Ignore,remember:false,btw:String::new()},Completion::Expired),
                result=&mut desktop,if !desktop_done=>match result {
                    Ok(decision)=>break (decision,Completion::Other),Err(_)=>desktop_done=true,
                },
                result=&mut telegram.reply,if !telegram_done=>match result {
                    Ok(Ok(decision))=>break (decision,Completion::Telegram),_=>telegram_done=true,
                },
            }
            if desktop_done && telegram_done {
                return Err(ApiError::unavailable("桌面和 Telegram 授权通道均不可用。"));
            }
        };
        if tokio::time::Instant::now() >= deadline {
            telegram.complete(Completion::Expired);
            return Ok(Decision {
                choice: Choice::Ignore,
                remember: false,
                btw: decision.btw,
            });
        }
        if decision.btw.len() > 16384 {
            return Err(ApiError::bad_request("authorization note is too long"));
        }
        telegram.complete(completion);
        // Dropping the losing desktop future invokes its CloseOnDrop guard.
        Ok(decision)
    }
}

enum Backend {
    Local(Service),
    Cloud(AppState),
}
impl Backend {
    async fn create(&self, prompt: Prompt) -> Result<(), ApiError> {
        let device = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "localshelld device".into());
        match self {
            Self::Local(service) => service
                .create("local-consent", &device, prompt)
                .await
                .map_err(ApiError::unavailable),
            Self::Cloud(state) => state
                .telegram_cloud_request(
                    "telegram/request",
                    json!({"prompt":prompt,"device":device}),
                    Duration::from_secs(25),
                )
                .await
                .map(|_| ()),
        }
    }
    async fn poll(&self, id: &str) -> Result<Option<Decision>, ApiError> {
        match self {
            Self::Local(service) => service
                .poll("local-consent", id)
                .await
                .map_err(ApiError::unavailable),
            Self::Cloud(state) => {
                let result = state
                    .telegram_cloud_request(
                        "telegram/poll",
                        json!({"id":id}),
                        Duration::from_secs(25),
                    )
                    .await?;
                serde_json::from_value(result["decision"].clone())
                    .map_err(|_| ApiError::unavailable("invalid Telegram relay response"))
            }
        }
    }
    async fn finish(&self, id: &str, completion: Completion) {
        match self {
            Self::Local(service) => {
                let _ = service.finish("local-consent", id, completion).await;
            }
            Self::Cloud(state) => {
                for attempt in 0..3 {
                    if state
                        .telegram_cloud_request(
                            "telegram/finish",
                            json!({"id":id,"completion":completion}),
                            Duration::from_secs(10),
                        )
                        .await
                        .is_ok()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
                }
            }
        }
    }
}
struct Channel {
    reply: oneshot::Receiver<Result<Decision, ApiError>>,
    finish: Option<oneshot::Sender<Completion>>,
}
impl Channel {
    fn start(state: AppState, prompt: Prompt, deadline: tokio::time::Instant) -> Self {
        let (reply, receive) = oneshot::channel();
        let (finish, mut closed) = oneshot::channel();
        tokio::spawn(async move {
            let backend = match state.telegram_backend().await {
                Ok(b) => b,
                Err(e) => {
                    let _ = reply.send(Err(e));
                    return;
                }
            };
            let id = prompt.id.clone();
            // Delivery is not abandoned when the other channel wins. Even a
            // late send is followed by an explicit completion update.
            if let Err(error) = backend.create(prompt).await {
                let _ = reply.send(Err(error));
                backend
                    .finish(&id, closed.try_recv().unwrap_or(Completion::Cancelled))
                    .await;
                return;
            }
            let mut reply = Some(reply);
            let completion = loop {
                tokio::select! {
                    completion=&mut closed=>break completion.unwrap_or(Completion::Cancelled),
                    _=tokio::time::sleep_until(deadline)=>break Completion::Expired,
                    result=backend.poll(&id),if reply.is_some()=>match result {
                        Ok(Some(decision))=>{let _=reply.take().unwrap().send(Ok(decision));},
                        Ok(None)=>{},
                        Err(error)=>{let _=reply.take().unwrap().send(Err(error));}
                    }
                }
            };
            backend.finish(&id, completion).await;
        });
        Self {
            reply: receive,
            finish: Some(finish),
        }
    }
    fn complete(&mut self, completion: Completion) {
        if let Some(send) = self.finish.take() {
            let _ = send.send(completion);
        }
    }
}
impl Drop for Channel {
    fn drop(&mut self) {
        self.complete(Completion::Cancelled);
    }
}

#[cfg(test)]
mod tests;
