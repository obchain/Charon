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
use charon_scanner::{
    BlockListener, ChainEvent, ChainProvider, HealthScanner, PositionBucket, ScanScheduler,
};
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
/// For every `NewBlock` event on a chain with a `[protocol.venus]` entry
/// the Venus adapter fetches positions anchored at the observed block,
/// pushes them through the bucketed [`HealthScanner`], and limits fetches
/// to buckets whose cadence fires this block via [`ScanScheduler`].
/// Chains without a Venus protocol config still flow through the drain
/// loop but trigger no protocol scans (v0.1 scope).
///
/// Backfill blocks (synthesised during WebSocket reconnect) are logged
/// but not scanned — the state they would produce is superseded by the
/// next real head and a fresh scan is cheaper than retroactive bucket
/// transitions.
async fn run_listen(config: &Config, borrowers: Vec<Address>) -> Result<()> {
    if config.chain.is_empty() {
        anyhow::bail!("no chains configured — nothing to listen to");
    }

    // Venus adapter + bucketed scanner + cadence scheduler are currently
    // single-chain (BNB) per config scope. Build them only if
    // `[protocol.venus]` exists and its target chain is configured;
    // otherwise run the listener pipeline without a scanner.
    let venus_adapter: Option<(String, Arc<VenusAdapter>, Arc<HealthScanner>, ScanScheduler)> =
        match config.protocol.get("venus") {
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
                let adapter = Arc::new(
                    VenusAdapter::connect(Arc::new(adapter_ws), venus_cfg.comptroller).await?,
                );
                let scanner = Arc::new(HealthScanner::new(
                    config.bot.liquidatable_threshold_bps,
                    config.bot.near_liq_threshold_bps,
                )?);
                let sched = ScanScheduler::new(
                    config.bot.hot_scan_blocks,
                    config.bot.warm_scan_blocks,
                    config.bot.cold_scan_blocks,
                );
                info!(
                    chain = %chain_name,
                    borrower_count = borrowers.len(),
                    market_count = adapter.markets().await.len(),
                    liquidatable_bps = config.bot.liquidatable_threshold_bps,
                    near_liq_bps = config.bot.near_liq_threshold_bps,
                    hot_blocks = sched.hot,
                    warm_blocks = sched.warm,
                    cold_blocks = sched.cold,
                    "venus adapter + scanner ready"
                );
                Some((chain_name.clone(), adapter, scanner, sched))
            }
            None => {
                info!(
                    "no [protocol.venus] configured — listener will drain events without scanning"
                );
                None
            }
        };

    let (tx, mut rx) = mpsc::channel::<ChainEvent>(CHAIN_EVENT_CHANNEL);
    let mut listeners: tokio::task::JoinSet<(String, Result<()>)> = tokio::task::JoinSet::new();

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

    // The first real (non-backfill) block on the Venus chain seeds the
    // scanner with the operator-supplied borrower list. Subsequent scans
    // pull from the scheduler-selected bucket membership so we don't
    // burn RPC re-fetching COLD positions every block.
    let mut seeded = false;
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
                        if backfill {
                            // Skip backfill — the next real head will
                            // snapshot the final state of the missed range.
                            continue;
                        }
                        if let Some((venus_chain, adapter, scanner, sched)) =
                            venus_adapter.as_ref()
                        {
                            if venus_chain != &chain {
                                continue;
                            }
                            let start = std::time::Instant::now();
                            let scan_set: Vec<Address> = if !seeded {
                                seeded = true;
                                borrowers.clone()
                            } else {
                                let mut v = Vec::new();
                                for b in [
                                    PositionBucket::Liquidatable,
                                    PositionBucket::NearLiquidation,
                                    PositionBucket::Healthy,
                                ] {
                                    if sched.should_scan(b, number) {
                                        v.extend(scanner.borrowers_in_bucket(b));
                                    }
                                }
                                v
                            };
                            if scan_set.is_empty() {
                                continue;
                            }
                            let block_tag = BlockNumberOrTag::Number(number);
                            match adapter.fetch_positions(&scan_set, block_tag).await {
                                Ok(positions) => {
                                    let returned = positions.len();
                                    scanner.upsert(positions.clone());
                                    scanner.prune(&positions);
                                    let counts = scanner.bucket_counts();
                                    metrics::histogram!(
                                        "charon_scanner_scan_duration_seconds"
                                    )
                                    .record(start.elapsed().as_secs_f64());
                                    info!(
                                        chain = %chain,
                                        block = number,
                                        timestamp,
                                        tracked = scan_set.len(),
                                        returned,
                                        healthy = counts.healthy,
                                        near_liq = counts.near_liquidation,
                                        liquidatable = counts.liquidatable,
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
async fn supervise(listeners: &mut tokio::task::JoinSet<(String, Result<()>)>) {
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
