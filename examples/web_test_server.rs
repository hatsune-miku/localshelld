//! Disposable browser-test fixture. Never uses installed service configurations.
use anyhow::Result;
use localshelld::{
    auth::{AuthManager, hash_password},
    config::Config,
    server::{AppState, router},
};

#[tokio::main]
async fn main() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = Config {
        bind: "127.0.0.1:31819".parse()?,
        password_hash: hash_password(b"browser test")?,
        ..Config::default()
    };
    let path = directory.path().join("config.toml");
    config.save(&path)?;
    let auth = AuthManager::new(config.password_hash.clone(), None);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    let state = AppState::new(config, auth).with_storage(path)?;
    state.start_sync_worker().await;
    let second_config = Config {
        bind: "127.0.0.1:31822".parse()?,
        password_hash: hash_password(b"browser test")?,
        ..Config::default()
    };
    let second_path = directory.path().join("second/config.toml");
    second_config.save(&second_path)?;
    let second_auth = AuthManager::new(second_config.password_hash.clone(), None);
    let second_listener = tokio::net::TcpListener::bind(second_config.bind).await?;
    let second = AppState::new(second_config, second_auth).with_storage(second_path)?;
    second.start_sync_worker().await;
    let second_app = router(second.clone());
    let second_server = tokio::spawn(async move { axum::serve(second_listener, second_app).await });
    println!("Browser test fixture: http://localhost:31819");
    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            state.shutdown().await;
            second.shutdown().await;
            second_server.abort();
        })
        .await?;
    Ok(())
}
