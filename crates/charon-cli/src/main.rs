//! Charon command-line entrypoint.
//!
//! ```text
//! CHARON_CONFIG=/etc/charon/default.toml charon listen
//! charon --config config/default.toml listen
//! ```

use std::path::PathBuf;

use anyhow::{Context, Result};
use charon_core::Config;
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Charon — multi-chain flash-loan liquidation bot.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Path to the TOML config file.
    ///
    /// No default — the operator must supply the path explicitly via
    /// `--config` or the `CHARON_CONFIG` environment variable. Avoids the
    /// silent cwd-relative `config/default.toml` fallback which breaks inside
    /// the Docker deploy image where WORKDIR may differ from the repo root.
    #[arg(long, short = 'c', env = "CHARON_CONFIG")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Listen to chain events and track positions.
    /// (Scanner wiring arrives in Day 2 — for now this just loads config.)
    Listen,
}

// Explicit multi-thread flavor so the concurrency contract survives any
// future trimming of tokio's `full` feature set.
#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Load `.env` if present. Silent no-op if the file isn't there.
    let _ = dotenvy::dotenv();

    // Structured logs go to stderr so `listen` can eventually emit a JSON
    // data stream on stdout without interleaving. Verbosity via RUST_LOG.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    info!("charon starting up");
    info!(path = %cli.config.display(), "loading config");

    let config = Config::load(&cli.config)
        .with_context(|| format!("failed to load config from {}", cli.config.display()))?;

    // SECURITY: only counts and non-secret scalars here.
    // Never log ws_url, http_url, private keys, wallet addresses, or the
    // full Debug of Config / ChainConfig — RPC URLs embed API keys.
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
            run_listen(&config).await?;
        }
    }

    Ok(())
}

/// Long-running listener entry point. Exits cleanly on SIGINT or SIGTERM so
/// the Docker `stop` → SIGTERM → SIGKILL sequence never tears mid-operation.
async fn run_listen(_config: &Config) -> Result<()> {
    info!("listen: not wired up yet — scanner arrives in Day 2");

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("received SIGINT, shutting down");
        }
        _ = wait_sigterm() => {
            info!("received SIGTERM, shutting down");
        }
    }

    Ok(())
}

#[cfg(unix)]
async fn wait_sigterm() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut s) => {
            let _ = s.recv().await;
        }
        Err(err) => {
            tracing::warn!(error = %err, "failed to install SIGTERM handler");
            std::future::pending::<()>().await
        }
    }
}

#[cfg(not(unix))]
async fn wait_sigterm() {
    std::future::pending::<()>().await
}
