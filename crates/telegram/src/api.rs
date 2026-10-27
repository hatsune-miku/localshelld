use crate::{Config, Result};
use reqwest::Client;
use serde_json::{Value, json};
use std::time::Duration;
use zeroize::Zeroizing;

#[derive(Clone)]
pub struct Api {
    client: Client,
    base: String,
}
impl Default for Api {
    fn default() -> Self {
        Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("TLS HTTP client"),
            base: "https://api.telegram.org".into(),
        }
    }
}
impl Api {
    #[cfg(test)]
    pub(crate) fn for_test(base: String) -> Self {
        Self {
            client: Client::builder().no_proxy().build().unwrap(),
            base,
        }
    }
    pub(crate) async fn call(&self, token: &str, method: &str, body: Value) -> Result<Value> {
        // Never format reqwest errors: their URLs contain the bot token.
        let mut response = self
            .client
            .post(format!("{}/bot{token}/{method}", self.base))
            .json(&body)
            .send()
            .await
            .map_err(|_| "Telegram 连接失败，请检查网络或系统代理。")?;
        let status = response.status();
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(chunk) = response.chunk().await.map_err(|_| "Telegram 响应中断。")? {
            if bytes.len() + chunk.len() > 1024 * 1024 {
                return Err("Telegram response exceeds size limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            // Telegram reports an idempotent edit as HTTP 400. It already has
            // exactly the requested text/buttons, so retrying cannot improve it.
            if method == "editMessageText" && status.as_u16() == 400 {
                let error: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
                if error["description"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("Bad Request: message is not modified"))
                {
                    return Ok(Value::Null);
                }
            }
            return Err(match status.as_u16() {
                401 => "Telegram Bot Token 无效。",
                409 => "此 Bot 已有其它接收端，请停用另一处轮询或 webhook。",
                429 => "Telegram 请求过于频繁，请稍后重试。",
                _ => "Telegram 拒绝请求。",
            });
        }
        let mut value: Value =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid Telegram response")?;
        if value["ok"] != true {
            return Err("Telegram 拒绝请求。");
        }
        Ok(value["result"].take())
    }
    pub async fn verify(&self, token: &str) -> Result<String> {
        let provisional = Config {
            bot_token: token.into(),
            bot_username: "placeholder_bot".into(),
            enabled: true,
            binding: None,
        };
        provisional.validate()?;
        let me = self.call(token, "getMe", json!({})).await?;
        if me["is_bot"] != true {
            return Err("This Telegram account is not a bot");
        }
        let username = me["username"]
            .as_str()
            .ok_or("Telegram bot has no username")?
            .to_owned();
        let webhook = self.call(token, "getWebhookInfo", json!({})).await?;
        if webhook["url"].as_str().is_some_and(|s| !s.is_empty()) {
            return Err("此 Bot 已配置 webhook。请先确认并移除其它接收端。");
        }
        Ok(username)
    }
    pub(crate) async fn edit(
        &self,
        config: &Config,
        message: i64,
        text: &str,
        keyboard: Value,
    ) -> Result<()> {
        let binding = config
            .binding
            .as_ref()
            .ok_or("Telegram account is not bound")?;
        self.call(&config.bot_token, "editMessageText", json!({"chat_id":binding.chat_id,"message_id":message,
            "text":text,"reply_markup":{"inline_keyboard":keyboard},"link_preview_options":{"is_disabled":true}})).await?;
        Ok(())
    }
}
