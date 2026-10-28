use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "trustd-user", version = localshelld::VERSION, about = "trustd current-user daemon and desktop authorization")]
struct Options {
    #[arg(long)]
    config: Option<PathBuf>,
    /// Disable desktop authorization prompts.
    #[arg(long)]
    headless: bool,
}

fn main() -> Result<()> {
    let options = Options::parse();
    localshelld::daemon::user(options.config, options.headless)
}
