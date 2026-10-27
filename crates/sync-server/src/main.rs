use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(about = "Self-hosted localshelld credential synchronization")]
struct Args {
    #[arg(long, env = "LOCALSHELLD_SYNC_ORIGIN")]
    origin: String,
    #[arg(long, env = "LOCALSHELLD_SYNC_DATA", default_value = "./sync-data")]
    data: PathBuf,
    #[arg(long, env = "LOCALSHELLD_SYNC_BIND", default_value = "127.0.0.1:32121")]
    bind: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let app = localshelld_sync_server::App::open(&args.data, &args.origin)?;
    app.start_telegram().await?;
    if args.data.join("bootstrap-code").exists() {
        eprintln!(
            "Initialization code file: {}",
            args.data.join("bootstrap-code").display()
        );
    }
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    eprintln!("localshelld sync listening on {}", listener.local_addr()?);
    axum::serve(listener, localshelld_sync_server::router(app))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
