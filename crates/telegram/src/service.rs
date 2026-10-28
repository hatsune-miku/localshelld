use crate::{Api, Binding, Choice, Completion, Config, Decision, Prompt, Result, validate_prompt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::sync::{Mutex, watch};

#[derive(Clone)]
pub struct Service {
    inner: Arc<Inner>,
}
struct Inner {
    state: Mutex<State>,
    api: Api,
}
#[derive(Default)]
struct State {
    config: Option<Config>,
    generation: u64,
    offset: i64,
    pending: HashMap<String, Pending>,
    bindings: HashMap<String, PendingBinding>,
    worker: Option<tokio::task::AbortHandle>,
    error: Option<&'static str>,
}
struct Pending {
    owner: String,
    config: Config,
    prompt: Prompt,
    messages: Vec<i64>,
    last_text: String,
    remember: bool,
    note: String,
    candidate: watch::Sender<Option<Decision>>,
    completed: Option<Completion>,
    paint_lock: Arc<Mutex<()>>,
    dirty: bool,
}
struct PendingBinding {
    owner: String,
    expires: i64,
    candidate: Option<Binding>,
}

impl Default for Service {
    fn default() -> Self {
        Self::new(Api::default())
    }
}
impl Service {
    pub fn new(api: Api) -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(State::default()),
                api,
            }),
        }
    }
    pub async fn configure(&self, config: Option<Config>) -> Result<()> {
        if let Some(config) = &config {
            config.validate()?;
        }
        let mut state = self.inner.state.lock().await;
        if state.config == config {
            return Ok(());
        }
        if let Some(worker) = state.worker.take() {
            worker.abort();
        }
        let ids: Vec<_> = state
            .pending
            .iter_mut()
            .filter_map(|(id, p)| {
                if p.completed.is_none() {
                    p.completed = Some(Completion::Cancelled);
                    p.dirty = true;
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();
        state.config = config;
        state.generation += 1;
        state.offset = 0;
        state.bindings.clear();
        state.error = None;
        drop(state);
        for id in ids {
            self.schedule_paint(id);
        }
        self.ensure_worker().await;
        Ok(())
    }
    async fn ensure_worker(&self) {
        let mut state = self.inner.state.lock().await;
        if state.worker.is_some() || state.config.is_none() {
            return;
        }
        let weak = Arc::downgrade(&self.inner);
        state.worker = Some(tokio::spawn(worker(weak)).abort_handle());
    }
    pub async fn stop(&self) {
        let mut state = self.inner.state.lock().await;
        if let Some(worker) = state.worker.take() {
            worker.abort();
        }
    }
    pub async fn test_message(&self) -> Result<()> {
        let config = self
            .inner
            .state
            .lock()
            .await
            .config
            .clone()
            .ok_or("请先保存 Bot Token。")?;
        let binding = config.binding.as_ref().ok_or("请先绑定 Telegram 账号。")?;
        self.inner.api.call(&config.bot_token,"sendMessage",json!({"chat_id":binding.chat_id,"text":"trustd Telegram 授权已就绪。此消息不授予任何执行权限。"})).await?;
        Ok(())
    }
    pub async fn status(&self) -> Value {
        let state = self.inner.state.lock().await;
        json!({"configured":state.config.is_some(),"enabled":state.config.as_ref().is_some_and(|c|c.enabled),
            "bot_username":state.config.as_ref().map(|c|&c.bot_username),
            "binding":state.config.as_ref().and_then(|c|c.binding.as_ref()),"error":state.error})
    }
    pub async fn begin_binding(&self, owner: &str, code: Option<&str>) -> Result<Value> {
        let code = code
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
        if code.len() != 32 || !code.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid binding code");
        }
        let mut state = self.inner.state.lock().await;
        let username = state
            .config
            .as_ref()
            .ok_or("请先保存 Bot Token。")?
            .bot_username
            .clone();
        state.bindings.retain(|_, b| b.expires > now());
        if state.bindings.len() >= 16 || state.bindings.contains_key(&code) {
            return Err("binding already exists or limit reached");
        }
        state.bindings.insert(
            code.clone(),
            PendingBinding {
                owner: owner.into(),
                expires: now() + 900_000,
                candidate: None,
            },
        );
        drop(state);
        self.ensure_worker().await;
        Ok(
            json!({"code":code,"url":format!("https://t.me/{username}?start={code}"),"expires_in":900}),
        )
    }
    pub async fn binding(&self, owner: &str, code: &str) -> Result<Option<Binding>> {
        let state = self.inner.state.lock().await;
        let binding = state
            .bindings
            .get(code)
            .filter(|b| b.owner == owner && b.expires > now())
            .ok_or("binding expired or does not belong to this session")?;
        Ok(binding.candidate.clone())
    }
    pub async fn create(&self, owner: &str, device: &str, prompt: Prompt) -> Result<()> {
        validate_prompt(&prompt)?;
        if device.is_empty() || device.len() > 256 || device.chars().any(char::is_control) {
            return Err("invalid device name");
        }
        let id = prompt.id.clone();
        let config = {
            let mut state = self.inner.state.lock().await;
            let config = state
                .config
                .clone()
                .filter(|c| c.enabled && c.binding.is_some())
                .ok_or("Telegram 尚未启用或绑定。")?;
            state
                .pending
                .retain(|_, p| p.prompt.expires_at_ms + 120_000 > now());
            if let Some(existing) = state.pending.get(&id) {
                return if existing.owner == owner {
                    Ok(())
                } else {
                    Err("request id already exists")
                };
            }
            if state.pending.len() >= 128
                || state
                    .pending
                    .values()
                    .filter(|p| p.owner == owner && p.completed.is_none())
                    .count()
                    >= 8
            {
                return Err("too many Telegram requests");
            }
            state.pending.insert(
                id.clone(),
                Pending {
                    owner: owner.into(),
                    config: config.clone(),
                    prompt: prompt.clone(),
                    messages: vec![],
                    last_text: String::new(),
                    remember: false,
                    note: String::new(),
                    candidate: watch::channel(None).0,
                    completed: None,
                    paint_lock: Arc::new(Mutex::new(())),
                    dirty: false,
                },
            );
            config
        };
        self.ensure_worker().await;
        let keys = if prompt.secret_keys.is_empty() {
            "无".to_owned()
        } else {
            prompt.secret_keys.join(", ")
        };
        let content = format!(
            "trustd 授权\n设备：{device}\n代理：{}\n权限：{}\n凭据名称：{keys}\n\n{}\n\n60 秒内有效。回复本消息可添加备注。",
            prompt.agent_name, prompt.scope, prompt.request_reason
        );
        let chunks = chunks(&content, 3400);
        for (index, chunk) in chunks.iter().enumerate() {
            let last = index + 1 == chunks.len();
            let markup = if last {
                keyboard(&id, false)
            } else {
                json!([])
            };
            let value = self.inner.api.call(&config.bot_token,"sendMessage",json!({"chat_id":config.binding.as_ref().unwrap().chat_id,"text":chunk,"reply_markup":{"inline_keyboard":markup},"link_preview_options":{"is_disabled":true}})).await;
            let value = match value {
                Ok(v) => v,
                Err(error) => {
                    self.finish(owner, &id, Completion::Cancelled).await?;
                    return Err(error);
                }
            };
            let message = value["message_id"]
                .as_i64()
                .ok_or("invalid Telegram message")?;
            let mut state = self.inner.state.lock().await;
            if let Some(p) = state.pending.get_mut(&id) {
                p.messages.push(message);
                p.last_text = chunk.clone();
                let completed = p.completed.is_some();
                drop(state);
                if completed {
                    let _ = self.paint(&id).await;
                    break;
                }
            }
        }
        Ok(())
    }
    pub async fn poll(&self, owner: &str, id: &str) -> Result<Option<Decision>> {
        let (mut receiver, expiry) = {
            let state = self.inner.state.lock().await;
            let p = state
                .pending
                .get(id)
                .filter(|p| p.owner == owner)
                .ok_or("authorization request not found")?;
            if p.completed.is_some() || p.prompt.expires_at_ms <= now() {
                return Err("authorization request is closed");
            }
            (p.candidate.subscribe(), p.prompt.expires_at_ms)
        };
        let current = receiver.borrow_and_update().clone();
        if current.is_some() {
            return Ok(current);
        }
        let wait = Duration::from_millis((expiry - now()).clamp(1, 20_000) as u64);
        let _ = tokio::time::timeout(wait, receiver.changed()).await;
        Ok(receiver.borrow().clone())
    }
    pub async fn finish(&self, owner: &str, id: &str, completion: Completion) -> Result<()> {
        let mut state = self.inner.state.lock().await;
        let Some(p) = state.pending.get_mut(id).filter(|p| p.owner == owner) else {
            return Ok(());
        };
        if p.completed.is_none() {
            if completion == Completion::Telegram && p.candidate.borrow().is_none() {
                return Err("no Telegram decision to acknowledge");
            }
            p.completed = Some(completion);
            p.dirty = true;
        }
        drop(state);
        // State is closed before network I/O; obsolete buttons cannot authorize.
        self.schedule_paint(id.to_owned());
        Ok(())
    }
    fn schedule_paint(&self, id: String) {
        let service = self.clone();
        tokio::spawn(async move {
            // Also retry when configuration removal stopped the polling worker.
            for attempt in 0..3 {
                if service.paint(&id).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
            }
        });
    }
    async fn paint(&self, id: &str) -> Result<()> {
        let lock = self
            .inner
            .state
            .lock()
            .await
            .pending
            .get(id)
            .map(|p| p.paint_lock.clone())
            .ok_or("request not found")?;
        let _serial = lock.lock().await;
        let (config, message, text, keyboard) = {
            let mut state = self.inner.state.lock().await;
            let p = state.pending.get_mut(id).ok_or("request not found")?;
            let Some(message) = p.messages.last() else {
                return Ok(());
            };
            p.dirty = false;
            let candidate = p.candidate.borrow();
            let (suffix, keys) = if let Some(completed) = p.completed {
                (completed.label(candidate.as_ref()).to_owned(), json!([]))
            } else if candidate.is_some() {
                ("已提交，等待设备确认".into(), json!([]))
            } else {
                (
                    format!(
                        "记住选择：{}\n备注：{}",
                        if p.remember { "是" } else { "否" },
                        if p.note.is_empty() {
                            "无"
                        } else {
                            "已收到"
                        }
                    ),
                    keyboard(id, p.remember),
                )
            };
            (
                p.config.clone(),
                *message,
                format!("{}\n\n{suffix}", p.last_text),
                keys,
            )
        };
        let result = self.inner.api.edit(&config, message, &text, keyboard).await;
        if result.is_err()
            && let Some(p) = self.inner.state.lock().await.pending.get_mut(id)
        {
            p.dirty = true;
        }
        result
    }
    async fn update(&self, generation: u64, update: Value) {
        if update.get("callback_query").is_some() {
            let query = &update["callback_query"];
            let data = query["data"].as_str().unwrap_or("");
            let Some((id, action)) = data.split_once(':') else {
                return;
            };
            let (config, message) = {
                let mut state = self.inner.state.lock().await;
                if state.generation != generation {
                    return;
                }
                let Some(config) = state.config.clone() else {
                    return;
                };
                let valid = config.binding.as_ref().is_some_and(|b| {
                    query["from"]["id"].as_i64() == Some(b.user_id)
                        && query["from"]["is_bot"] == false
                        && query["message"]["chat"]["id"].as_i64() == Some(b.chat_id)
                        && query["message"]["chat"]["type"] == "private"
                });
                if !valid {
                    (config, "此账号无权处理授权")
                } else if let Some(p) = state.pending.get_mut(id) {
                    p.dirty = true;
                    if !p
                        .messages
                        .iter()
                        .any(|m| Some(*m) == query["message"]["message_id"].as_i64())
                    {
                        (config, "授权消息不匹配")
                    } else if let Some(done) = p.completed {
                        (config, done.label(p.candidate.borrow().as_ref()))
                    } else if p.prompt.expires_at_ms <= now() {
                        p.completed = Some(Completion::Expired);
                        (config, "授权请求已超时")
                    } else if p.candidate.borrow().is_some() {
                        (config, "已提交，等待设备确认")
                    } else if action == "remember" {
                        p.remember = !p.remember;
                        (config, "已更新记住选择")
                    } else {
                        let choice = match action {
                            "accept" => Some(Choice::Accept),
                            "deny" => Some(Choice::Deny),
                            "ignore" => Some(Choice::Ignore),
                            _ => None,
                        };
                        if let Some(choice) = choice {
                            p.candidate.send_replace(Some(Decision {
                                choice,
                                remember: p.remember && choice != Choice::Ignore,
                                btw: p.note.clone(),
                            }));
                        }
                        (
                            config,
                            if choice.is_some() {
                                "已提交，等待设备确认"
                            } else {
                                "无效操作"
                            },
                        )
                    }
                } else {
                    (config, "授权请求已失效")
                }
            };
            let _ = self
                .inner
                .api
                .call(
                    &config.bot_token,
                    "answerCallbackQuery",
                    json!({"callback_query_id":query["id"],"text":message}),
                )
                .await;
            let _ = self.paint(id).await;
            return;
        }
        let message = &update["message"];
        let Some(text) = message["text"].as_str() else {
            return;
        };
        let Some(user) = message["from"]["id"].as_i64().filter(|id| *id > 0) else {
            return;
        };
        if message["chat"]["id"].as_i64() != Some(user)
            || message["chat"]["type"] != "private"
            || message["from"]["is_bot"] != false
        {
            return;
        }
        let mut state = self.inner.state.lock().await;
        if state.generation != generation {
            return;
        }
        if let Some(code) = text.strip_prefix("/start ") {
            if let Some(binding) = state
                .bindings
                .get_mut(code.trim())
                .filter(|b| b.expires > now() && b.candidate.is_none())
            {
                binding.candidate = Some(Binding {
                    user_id: user,
                    chat_id: user,
                });
            }
            return;
        }
        if state
            .config
            .as_ref()
            .and_then(|c| c.binding.as_ref())
            .is_none_or(|b| b.user_id != user)
        {
            return;
        }
        let reply = message["reply_to_message"]["message_id"].as_i64();
        if text.len() > 16384 || text.contains('\0') {
            return;
        }
        let mut target = None;
        for (id, p) in &mut state.pending {
            if p.completed.is_none()
                && p.prompt.expires_at_ms > now()
                && p.candidate.borrow().is_none()
                && p.messages.iter().any(|m| Some(*m) == reply)
            {
                p.note = text.into();
                p.dirty = true;
                target = Some(id.clone());
                break;
            }
        }
        drop(state);
        if let Some(id) = target {
            let _ = self.paint(&id).await;
        }
    }
}

async fn worker(weak: Weak<Inner>) {
    loop {
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let (config, offset, generation, active, expired) = {
            let mut state = inner.state.lock().await;
            state.bindings.retain(|_, b| b.expires > now());
            state
                .pending
                .retain(|_, p| p.prompt.expires_at_ms + 120_000 > now());
            let expired: Vec<_> = state
                .pending
                .iter_mut()
                .filter_map(|(id, p)| {
                    if p.completed.is_none() && p.prompt.expires_at_ms <= now() {
                        p.completed = Some(Completion::Expired);
                        p.dirty = true;
                    }
                    p.dirty.then(|| id.clone())
                })
                .collect();
            let Some(config) = state.config.clone() else {
                return;
            };
            let active = (config.enabled && config.binding.is_some()) || !state.bindings.is_empty();
            (config, state.offset, state.generation, active, expired)
        };
        let api = inner.api.clone();
        let service = Service { inner };
        for id in expired {
            let _ = service.paint(&id).await;
        }
        drop(service);
        if !active {
            tokio::time::sleep(Duration::from_millis(300)).await;
            continue;
        }
        // A short long-poll interval also bounds expiry cleanup latency.
        let result=api.call(&config.bot_token,"getUpdates",json!({"offset":offset,"timeout":5,"limit":50,"allowed_updates":["message","callback_query"]})).await;
        let Some(inner) = weak.upgrade() else {
            return;
        };
        let service = Service { inner };
        let mut state = service.inner.state.lock().await;
        if state.generation != generation {
            return;
        }
        state.error = result.as_ref().err().copied();
        drop(state);
        match result {
            Ok(value) => {
                for update in value.as_array().into_iter().flatten() {
                    if let Some(id) = update["update_id"].as_i64() {
                        service.update(generation, update.clone()).await;
                        let mut state = service.inner.state.lock().await;
                        if state.generation != generation {
                            return;
                        }
                        state.offset = state.offset.max(id + 1);
                    }
                }
            }
            Err(_) => {
                drop(service);
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}
fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn keyboard(id: &str, remember: bool) -> Value {
    json!([
        [{"text":"接受","callback_data":format!("{id}:accept")},{"text":"拒绝","callback_data":format!("{id}:deny")},{"text":"忽略","callback_data":format!("{id}:ignore")}],
        [{"text":if remember{"☑ 记住选择"}else{"☐ 记住选择"},"callback_data":format!("{id}:remember")}]
    ])
}
fn chunks(text: &str, size: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut units = 0;
    for character in text.chars() {
        if units + character.len_utf16() > size && !part.is_empty() {
            parts.push(std::mem::take(&mut part));
            units = 0;
        }
        units += character.len_utf16();
        part.push(character);
    }
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

#[cfg(test)]
mod tests;
