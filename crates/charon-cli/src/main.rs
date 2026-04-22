//! Charon command-line entrypoint.
//!
//! ```text
//! charon --config config/default.toml listen
//! charon --config config/default.toml test-connection --chain bnb
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result};
use charon_core::Config;
use charon_scanner::{BlockListener, ChainEvent, ChainProvider};
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// Size of the fan-in channel from listeners to the scanner pipeline.
/// One slot per ~5 blocks across all chains covers short stalls without
/// back-pressuring the listener task.
const CHAIN_EVENT_CHANNEL: usize = 1024;

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
    /// Spawn one block listener per configured chain and print new blocks.
    ///
    /// Downstream pipeline (scanner → profit calc → executor) consumes
    /// the same channel once those layers land.
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
        Command::Listen => run_listen(config).await?,
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

/// Spawn one `BlockListener` per configured chain, drain the shared
/// `ChainEvent` channel, and exit on Ctrl-C.
async fn run_listen(config: Config) -> Result<()> {
    if config.chain.is_empty() {
        anyhow::bail!("no chains configured — nothing to listen to");
    }

    let (tx, mut rx) = mpsc::channel::<ChainEvent>(CHAIN_EVENT_CHANNEL);
    let mut listeners: tokio::task::JoinSet<(String, Result<()>)> =
        tokio::task::JoinSet::new();

    for (name, chain_cfg) in config.chain {
        let listener = BlockListener::new(name.clone(), chain_cfg, tx.clone());
        listeners.spawn(async move { (name, listener.run().await) });
    }
    // Drop our sender so the channel closes when every listener exits.
    drop(tx);

    info!("listen: draining chain events (Ctrl-C to stop)");

    tokio::select! {
        _ = async {
            while let Some(event) = rx.recv().await {
                match event {
                    ChainEvent::NewBlock { chain, number, timestamp, backfill } => {
                        tracing::debug!(
                            chain = %chain,
                            block = number,
                            timestamp = timestamp,
                            backfill,
                            "cli drained event"
                        );
                    }
                    _ => {}
                }
            }
        } => {
            info!("all listeners exited");
        }
        _ = supervise(&mut listeners) => {
            info!("all listener tasks terminated");
        }
        _ = tokio::signal::ctrl_c() => {
            info!("ctrl-c received, shutting down");
            listeners.shutdown().await;
        }
    }

    Ok(())
}

/// Drain a `JoinSet` of listener tasks and surface panics / errors per chain.
/// Returns when every listener has exited so the caller can shut down.
async fn supervise(
    listeners: &mut tokio::task::JoinSet<(String, Result<()>)>,
) {
    while let Some(joined) = listeners.join_next().await {
        match joined {
            Ok((name, Ok(()))) => {
                info!(chain = %name, "listener exited cleanly");
            }
            Ok((name, Err(err))) => {
                warn!(chain = %name, error = ?err, "listener terminated with error");
            }
            Err(err) if err.is_panic() => {
                warn!(error = ?err, "listener panicked");
            }
            Err(err) => {
                warn!(error = ?err, "listener join error");
            }
        }
    }
}
