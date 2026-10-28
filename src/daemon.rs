//! Runtime entry points. No installation, update or maintenance commands live here.
#[cfg(windows)]
use anyhow::Context;
use anyhow::{Result, ensure};
use std::path::PathBuf;

use crate::{
    auth::AuthManager,
    config::{
        Config, default_config_path, load_or_create_seal_key, seal_key_path, unseal,
        user_config_path,
    },
    server::{AppState, router},
};

pub fn logging() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "localshelld=info,trustd=info".into()),
        )
        .init();
}

pub fn is_elevated() -> Result<bool> {
    #[cfg(unix)]
    {
        Ok(rustix::process::geteuid().is_root())
    }
    #[cfg(windows)]
    {
        Ok(localshelld_native_process::current_identity()?.elevated)
    }
}

pub fn require_user() -> Result<()> {
    ensure!(
        !is_elevated()?,
        "user daemon requires a non-elevated user; refusing root/Administrator identity"
    );
    Ok(())
}

pub fn user(config: Option<PathBuf>, headless: bool) -> Result<()> {
    require_user()?;
    #[cfg(windows)]
    if !headless {
        localshelld_native_dialog::detach_owned_console()
            .context("failed to detach the user daemon's standalone console")?;
    }
    logging();
    let path = config.map_or_else(user_config_path, Ok)?;
    let runtime = tokio::runtime::Runtime::new()?;
    if !headless && crate::desktop::available() {
        let desktop = crate::desktop::Desktop::new()?;
        let broker = desktop.broker();
        let stop = desktop.stop_callback();
        runtime.spawn(async move {
            let result = serve(path, true, Some(broker), shutdown_signal()).await;
            if let Err(error) = &result {
                tracing::error!(%error, "user daemon stopped");
            }
            stop(if result.is_ok() { 0 } else { 1 });
        });
        desktop.run();
    }
    runtime.block_on(serve(path, true, None, shutdown_signal()))
}

#[cfg(unix)]
pub fn root(config: Option<PathBuf>) -> Result<()> {
    ensure!(is_elevated()?, "trustd-root must run as root");
    logging();
    tokio::runtime::Runtime::new()?.block_on(serve(
        config.map_or_else(default_config_path, Ok)?,
        false,
        None,
        shutdown_signal(),
    ))
}

#[cfg(windows)]
pub fn system(config: Option<PathBuf>, console: bool) -> Result<()> {
    ensure!(
        localshelld_native_process::current_identity()?.is_system,
        "trustd-system must run as NT AUTHORITY\\SYSTEM through the installed service"
    );
    logging();
    let path = config.map_or_else(default_config_path, Ok)?;
    if console {
        tokio::runtime::Runtime::new()?.block_on(serve(path, false, None, shutdown_signal()))
    } else {
        crate::windows_service_host::dispatch(path)
    }
}

pub(crate) async fn serve<F>(
    path: PathBuf,
    user: bool,
    consent: Option<crate::consent::ConsentBroker>,
    shutdown: F,
) -> Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let config = Config::load(&path)?;
    let totp = config
        .totp_secret
        .as_ref()
        .map(|secret| {
            let key = load_or_create_seal_key(&seal_key_path(&path))?;
            unseal(secret, &key)
        })
        .transpose()?;
    let auth = AuthManager::new(config.password_hash.clone(), totp);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    tracing::info!(bind = %config.bind, "trustd management endpoint ready");
    let mut state = if user {
        AppState::user(config, auth)?
    } else {
        AppState::new(config, auth)
    }
    .with_storage(path)?;
    if let Some(consent) = consent {
        state = state.with_consent(consent);
    }
    state.start_sync_worker().await;
    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(async move {
            shutdown.await;
            state.shutdown().await;
        })
        .await?;
    Ok(())
}

pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}
