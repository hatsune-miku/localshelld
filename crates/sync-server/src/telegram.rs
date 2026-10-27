use super::*;
use localshelld_telegram::{Action, Api, Completion, Config, Prompt};

impl App {
    pub async fn start_telegram(&self) -> anyhow::Result<()> {
        let config = self.store.lock().await.saved.vault.telegram.clone();
        self.telegram
            .configure(config)
            .await
            .map_err(anyhow::Error::msg)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Command {
    expected_revision: u64,
    command: Action,
}

async fn owner(app: &App, headers: &HeaderMap) -> Result<String> {
    let store = app.store.lock().await;
    Ok(app.authenticate(&store, headers)?.sid)
}
async fn save(app: &App, headers: &HeaderMap, revision: u64, config: Option<Config>) -> Result<()> {
    let mut store = app.store.lock().await;
    app.authenticate(&store, headers)?;
    if store.saved.vault.revision != revision {
        return Err(Error(
            StatusCode::CONFLICT,
            "Telegram settings changed; refresh and retry",
        ));
    }
    let mut next = store.saved.clone();
    next.vault.telegram = config.clone();
    next.vault.revision = next.vault.revision.checked_add(1).ok_or_else(internal)?;
    store.commit(next).map_err(|_| internal())?;
    app.revision.send_replace(store.saved.vault.revision);
    drop(store);
    app.telegram.configure(config).await.map_err(bad)
}
pub(super) async fn control(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Command>,
) -> Result<Json<Value>> {
    let sid = owner(&app, &headers).await?;
    let _serial = app.telegram_config_lock.lock().await;
    app.start_telegram().await.map_err(|_| internal())?;
    let (config, revision) = {
        let store = app.store.lock().await;
        (
            store.saved.vault.telegram.clone(),
            store.saved.vault.revision,
        )
    };
    let value = match &body.command {
        Action::Status => app.telegram.status().await,
        Action::Configure { token, enabled } => {
            if revision != body.expected_revision {
                return Err(Error(StatusCode::CONFLICT, "settings revision conflict"));
            }
            let mut config = if let Some(token) = token {
                let username = Api::default().verify(token).await.map_err(bad)?;
                let binding = config
                    .as_ref()
                    .filter(|old| old.bot_token == *token)
                    .and_then(|old| old.binding.clone());
                Config {
                    bot_token: token.clone(),
                    bot_username: username,
                    enabled: *enabled,
                    binding,
                }
            } else {
                config.ok_or_else(|| bad("save a bot token first"))?
            };
            config.enabled = *enabled;
            save(&app, &headers, revision, Some(config)).await?;
            json!({"saved":true})
        }
        Action::Remove => {
            save(&app, &headers, body.expected_revision, None).await?;
            json!({"removed":true})
        }
        Action::BindStart { code } => app
            .telegram
            .begin_binding(&sid, code.as_deref())
            .await
            .map_err(bad)?,
        Action::BindPoll { code } => {
            if let Some(binding) = app.telegram.binding(&sid, code).await.map_err(bad)? {
                let mut config = config.ok_or_else(|| bad("bot was removed"))?;
                config.binding = Some(binding);
                save(&app, &headers, revision, Some(config)).await?;
                json!({"bound":true})
            } else {
                json!({"pending":true})
            }
        }
        Action::Test => {
            app.telegram.test_message().await.map_err(bad)?;
            json!({"sent":true})
        }
    };
    Ok(Json(value))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Create {
    prompt: Prompt,
    device: String,
}
pub(super) async fn create(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Create>,
) -> Result<Json<Value>> {
    let sid = owner(&app, &headers).await?;
    app.telegram
        .create(&sid, &body.device, body.prompt)
        .await
        .map_err(bad)?;
    Ok(Json(json!({"created":true})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Id {
    id: String,
}
pub(super) async fn poll(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Id>,
) -> Result<Json<Value>> {
    let sid = owner(&app, &headers).await?;
    let decision = app.telegram.poll(&sid, &body.id).await.map_err(bad)?;
    // Do not deliver approval after the device's cloud session was revoked.
    owner(&app, &headers).await?;
    Ok(Json(json!({"decision":decision})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Finish {
    id: String,
    completion: Completion,
}
pub(super) async fn finish(
    State(app): State<App>,
    headers: HeaderMap,
    Json(body): Json<Finish>,
) -> Result<Json<Value>> {
    let sid = owner(&app, &headers).await?;
    app.telegram
        .finish(&sid, &body.id, body.completion)
        .await
        .map_err(bad)?;
    Ok(Json(json!({"closed":true})))
}
