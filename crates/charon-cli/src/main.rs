//! Charon command-line entrypoint.
//!
//! ```text
//! charon --config config/default.toml listen
//! charon --config config/default.toml test-connection --chain bnb
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result};
use charon_core::Config;
use charon_scanner::ChainProvider;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Charon — multi-chain flash-loan liquidation bot.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(long, short = 'c', default_value = "config/default.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Listen to chain events and track positions.
    /// (Scanner wiring lands across multiple M1 issues — currently a stub.)
    Listen,

    /// Connect to a configured chain and print its latest block number.
    TestConnection {
        /// Chain key (must match a `[chain.<name>]` section in the config).
        #[arg(long, default_value = "bnb")]
        chain: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load `.env` if present. Silent no-op if the file isn't there.
    let _ = dotenvy::dotenv();

    // Structured logging. Override verbosity with RUST_LOG=debug etc.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();

    info!("charon starting up");
    info!(path = %cli.config.display(), "loading config");

    let config = Config::load(&cli.config)
        .with_context(|| format!("failed to load config from {}", cli.config.display()))?;

    info!(
        chains = config.chain.len(),
        protocols = config.protocol.len(),
        flashloan_sources = config.flashloan.len(),
        liquidators = config.liquidator.len(),
        min_profit_usd = config.bot.min_profit_usd,
        "config loaded"
    );

    match cli.command {
        Command::Listen => {
            info!("listen: not wired up yet — scanner arrives across M1 issues");
        }
        Command::TestConnection { chain } => {
            let chain_cfg = config
                .chain
                .get(&chain)
                .with_context(|| format!("chain '{chain}' not found in config"))?;
            let provider = ChainProvider::connect(&chain, chain_cfg).await?;
            let block = provider.test_connection().await?;
            info!(chain = %chain, block = block, "connected — latest block");
        }
    }

    Ok(())
}
