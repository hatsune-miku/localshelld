use anyhow::{Context, Result};
use clap::Parser;
use std::net::SocketAddr;
use zeroize::Zeroizing;

#[derive(Parser)]
#[command(name = "trustd-stdio", version = localshelld::VERSION, about = "MCP stdio adapter; keeps authorization handles out of model messages")]
struct Options {
    /// Literal loopback address of the daemon. Never uses an HTTP proxy or follows redirects.
    #[arg(long, default_value = "127.0.0.1:32120")]
    endpoint: SocketAddr,
    /// Read a preauthorization token from this environment variable, not a tool argument.
    #[arg(long)]
    token_env: Option<String>,
}

fn main() -> Result<()> {
    let options = Options::parse();
    let token = options
        .token_env
        .map(|name| {
            std::env::var(name)
                .map(Zeroizing::new)
                .context("configured token environment variable is unavailable")
        })
        .transpose()?;
    let adapter = localshelld::stdio::Adapter::new(options.endpoint, token)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let result = runtime.block_on(adapter.serve(tokio::io::stdin(), tokio::io::stdout()));
    // Tokio stdin uses a blocking reader. A signal must not keep the process
    // alive indefinitely merely because the host still has its pipe open.
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    result
}
