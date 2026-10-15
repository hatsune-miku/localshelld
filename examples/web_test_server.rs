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
    println!("Browser test fixture: http://localhost:31819");
    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            state.shutdown().await;
        })
        .await?;
    Ok(())
}
