//! Charon command-line entrypoint.
//!
//! ```text
//! CHARON_CONFIG=/etc/charon/default.toml charon listen
//! charon --config config/default.toml listen
//! charon --config config/default.toml listen --borrower 0xABC…
//! charon --config config/default.toml test-connection --chain bnb
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::Address;
use alloy::providers::{ProviderBuilder, WsConnect};
use anyhow::{Context, Result};
use charon_core::{Config, LendingProtocol};
use charon_protocols::VenusAdapter;
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
    /// Spawn one block listener per configured chain, drain chain events,
    /// and run the Venus adapter every new block for the supplied borrower
    /// list.
    ///
    /// Borrower discovery from indexed events is a follow-up; pass
    /// `--borrower 0x…` one or more times to seed a test list. An empty
    /// list is allowed — the adapter still connects so the operator can
    /// confirm the WS pipeline.
    Listen {
        /// Addresses to scan on every new block. Repeat the flag for
        /// multiple borrowers.
        #[arg(long = "borrower")]
        borrowers: Vec<Address>,
    },

    /// Connect to a configured chain and print its latest block number.
    TestConnection {
        /// Chain key (must match a `[chain.<name>]` section in the config).
        #[arg(long, default_value = "bnb")]
        chain: String,
    },
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
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
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
        min_profit_usd_1e6 = config.bot.min_profit_usd_1e6,
        "config loaded"
    );

    match cli.command {
        Command::Listen { borrowers } => {
            run_listen(&config, borrowers).await?;
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

/// Long-running listener entry point. Spawns one `BlockListener` per
/// configured chain, drains the shared `ChainEvent` channel, and exits
/// cleanly on SIGINT or SIGTERM so the Docker `stop` → SIGTERM → SIGKILL
/// sequence never tears mid-operation.
///
/// For every `NewBlock` event on a chain with a `[protocol.venus]` entry,
/// the Venus adapter scans the supplied borrower list anchored at the
/// observed block. Chains without a Venus protocol config still flow
/// through the drain loop but trigger no protocol scans (v0.1 scope).
async fn run_listen(config: &Config, borrowers: Vec<Address>) -> Result<()> {
    if config.chain.is_empty() {
        anyhow::bail!("no chains configured — nothing to listen to");
    }

    // Venus adapter is currently single-chain (BNB) per config scope.
    // Build it only if `[protocol.venus]` exists and its target chain is
    // configured; otherwise run the listener pipeline without a scanner.
    let venus_adapter: Option<(String, Arc<VenusAdapter>)> = match config.protocol.get("venus") {
        Some(venus_cfg) => {
            let chain_name = &venus_cfg.chain;
            let chain_cfg = config.chain.get(chain_name).with_context(|| {
                format!(
                    "protocol 'venus' references chain '{chain_name}' which is not in [chain.*]"
                )
            })?;
            let adapter_ws = ProviderBuilder::new()
                .on_ws(WsConnect::new(&chain_cfg.ws_url))
                .await
                .context("venus adapter: failed to connect over ws")?;
            let adapter =
                Arc::new(VenusAdapter::connect(Arc::new(adapter_ws), venus_cfg.comptroller).await?);
            info!(
                chain = %chain_name,
                borrower_count = borrowers.len(),
                market_count = adapter.markets().await.len(),
                "venus adapter ready"
            );
            Some((chain_name.clone(), adapter))
        }
        None => {
            info!("no [protocol.venus] configured — listener will drain events without scanning");
            None
        }
    };

    let (tx, mut rx) = mpsc::channel::<ChainEvent>(CHAIN_EVENT_CHANNEL);
    let mut listeners: tokio::task::JoinSet<(String, Result<()>)> =
        tokio::task::JoinSet::new();

    // `ChainConfig: Clone` — we only borrow `config`, so each listener task
    // gets its own owned copy.
    for (name, chain_cfg) in &config.chain {
        let name = name.clone();
        let chain_cfg = chain_cfg.clone();
        let listener = BlockListener::new(name.clone(), chain_cfg, tx.clone());
        listeners.spawn(async move { (name, listener.run().await) });
    }
    // Drop our sender so the channel closes when every listener exits.
    drop(tx);

    info!("listen: draining chain events (Ctrl-C or SIGTERM to stop)");

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
                        // Route to Venus scan only when this event is for
                        // the chain the Venus adapter was configured on.
                        if let Some((venus_chain, adapter)) = venus_adapter.as_ref() {
                            if venus_chain == &chain {
                                let start = std::time::Instant::now();
                                let block_tag = BlockNumberOrTag::Number(number);
                                match adapter.fetch_positions(&borrowers, block_tag).await {
                                    Ok(positions) => {
                                        info!(
                                            chain = %chain,
                                            block = number,
                                            timestamp,
                                            backfill,
                                            tracked = borrowers.len(),
                                            returned = positions.len(),
                                            scan_ms = start.elapsed().as_millis() as u64,
                                            "venus scan"
                                        );
                                    }
                                    Err(err) => warn!(
                                        chain = %chain,
                                        block = number,
                                        error = ?err,
                                        "venus scan failed"
                                    ),
                                }
                            }
                        }
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
            info!("received SIGINT, shutting down");
            listeners.shutdown().await;
        }
        _ = wait_sigterm() => {
            info!("received SIGTERM, shutting down");
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
