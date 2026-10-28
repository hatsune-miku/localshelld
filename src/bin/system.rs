#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use clap::Parser;
    #[derive(Parser)]
    #[command(name = "trustd-system", version = localshelld::VERSION, about = "trustd Windows SYSTEM service")]
    struct Options {
        #[arg(long)]
        config: Option<std::path::PathBuf>,
        /// Run in the foreground as SYSTEM instead of under the Service Control Manager.
        #[arg(long)]
        console: bool,
    }
    let options = Options::parse();
    localshelld::daemon::system(options.config, options.console)
}

#[cfg(not(windows))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("trustd-system is available only on Windows; use trustd-root on macOS and Linux")
}
