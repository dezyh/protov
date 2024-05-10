mod utils;

use clap::Parser;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use utils::parse;

#[derive(Parser)]
#[command(name = "protov")]
#[command(bin_name = "protov")]
enum Command {
    Sync(SyncArgs),
    Clean(CleanArgs),
}

#[derive(clap::Args)]
#[command(about = "Synchronize local state with the current state of the sources")]
struct SyncArgs {}

#[derive(clap::Args)]
#[command(about = "Clean the outputs of all build targets")]
struct CleanArgs {}

fn main() -> anyhow::Result<()> {
    // Read from env with a default fallback
    let filter = EnvFilter::from_default_env().add_directive(LevelFilter::DEBUG.into());
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let config = parse("protov.config.toml")?;

    match Command::parse() {
        Command::Sync(args) => utils::sync(&args, &config),
        Command::Clean(args) => utils::clean(&args, &config),
    };

    Ok(())
}
