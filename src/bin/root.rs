#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    use clap::Parser;
    #[derive(Parser)]
    #[command(name = "trustd-root", version = localshelld::VERSION, about = "trustd root daemon for macOS and Linux")]
    struct Options {
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    }
    let options = Options::parse();
    localshelld::daemon::root(options.config)
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("trustd-root is available only on macOS and Linux; use trustd-system on Windows")
}
