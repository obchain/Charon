//! Charon command-line entrypoint.
//!
//! ```text
//! charon --config config/default.toml listen
//! charon --config config/default.toml listen --borrower 0xABC…
//! charon --config config/default.toml test-connection --chain bnb
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use alloy::primitives::Address;
use alloy::providers::{ProviderBuilder, WsConnect};
use anyhow::{Context, Result};
use charon_core::{Config, LendingProtocol};
use charon_protocols::VenusAdapter;
use charon_scanner::{
    BlockListener, ChainEvent, ChainProvider, DEFAULT_MAX_AGE, HealthScanner, PriceCache,
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
    #[arg(long, short = 'c', default_value = "config/default.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Spawn block listeners + run the Venus adapter every new block.
    ///
    /// Borrower discovery from indexed events is a follow-up; pass
    /// `--borrower 0x…` one or more times to seed a test list.
    Listen {
        /// Addresses to scan on every new block. Repeat the flag for
        /// multiple borrowers. Empty list is allowed (adapter still
        /// connects so the operator can confirm the WS pipeline).
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
        min_profit_usd_1e6 = config.bot.min_profit_usd_1e6,
        "config loaded"
    );

    match cli.command {
        Command::Listen { borrowers } => run_listen(config, borrowers).await?,
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

/// Spawn block listeners, wire up the Venus adapter, and for every new
/// block scan the supplied borrower list. For v0.1 the protocol is
/// hard-wired to Venus on BNB Chain — matching the config scope.
async fn run_listen(config: Config, borrowers: Vec<Address>) -> Result<()> {
    // 1. Venus adapter — connects to BNB over WebSocket and snapshots
    //    Comptroller config (markets, oracle, close factor).
    let bnb = config
        .chain
        .get("bnb")
        .context("chain 'bnb' not configured — required for v0.1")?;
    let venus_cfg = config
        .protocol
        .get("venus")
        .context("protocol 'venus' not configured — required for v0.1")?;

    let adapter_ws = ProviderBuilder::new()
        .on_ws(WsConnect::new(&bnb.ws_url))
        .await
        .context("venus adapter: failed to connect over ws")?;
    let adapter =
        Arc::new(VenusAdapter::connect(Arc::new(adapter_ws), venus_cfg.comptroller).await?);

    let scanner = Arc::new(HealthScanner::new(
        config.bot.liquidatable_threshold,
        config.bot.near_liq_threshold,
    )?);

    // Chainlink price cache — feeds are configured per chain under
    // `[chainlink.<chain>]`. Empty map = no feeds configured, cache
    // stays idle and downstream stages fall back to protocol oracle.
    let price_feeds = config.chainlink.get("bnb").cloned().unwrap_or_default();
    let price_cache_ws = ProviderBuilder::new()
        .on_ws(WsConnect::new(&bnb.ws_url))
        .await
        .context("price cache: failed to connect over ws")?;
    let prices = Arc::new(PriceCache::new(
        Arc::new(price_cache_ws),
        price_feeds,
        DEFAULT_MAX_AGE,
    ));
    prices.refresh_all().await;
    let fresh_feeds: Vec<String> = prices.symbols().map(str::to_string).collect();
    for sym in &fresh_feeds {
        if let Some(p) = prices.get(sym) {
            info!(symbol = %sym, price = %p.price, decimals = p.decimals, "chainlink feed");
        }
    }

    info!(
        borrower_count = borrowers.len(),
        market_count = adapter.markets.len(),
        feed_count = fresh_feeds.len(),
        liquidatable_threshold = config.bot.liquidatable_threshold,
        near_liq_threshold = config.bot.near_liq_threshold,
        "venus adapter + scanner + price cache ready"
    );

    // 2. Block listeners — one per configured chain, fan-in to a shared
    //    mpsc. Each listener owns its own reconnect loop.
    let (tx, mut rx) = mpsc::channel::<ChainEvent>(CHAIN_EVENT_CHANNEL);
    for (name, chain_cfg) in config.chain {
        let listener = BlockListener::new(name.clone(), chain_cfg, tx.clone());
        tokio::spawn(async move {
            if let Err(err) = listener.run().await {
                warn!(chain = %name, error = ?err, "listener terminated");
            }
        });
    }
    drop(tx);

    info!("listen: draining chain events (Ctrl-C to stop)");

    // 3. Drain loop: on every new block, run one Venus scan.
    tokio::select! {
        _ = async {
            while let Some(event) = rx.recv().await {
                match event {
                    ChainEvent::NewBlock { chain, number, timestamp } => {
                        let start = std::time::Instant::now();
                        match adapter.fetch_positions(&borrowers).await {
                            Ok(positions) => {
                                let returned = positions.len();
                                scanner.upsert(positions);
                                let counts = scanner.bucket_counts();
                                info!(
                                    chain = %chain,
                                    block = number,
                                    timestamp = timestamp,
                                    tracked = borrowers.len(),
                                    returned,
                                    healthy = counts.healthy,
                                    near_liq = counts.near_liquidation,
                                    liquidatable = counts.liquidatable,
                                    scan_ms = start.elapsed().as_millis() as u64,
                                    "venus scan"
                                );
                            }
                            Err(err) => warn!(
                                chain = %chain, block = number, error = ?err,
                                "venus scan failed"
                            ),
                        }
                    }
                }
            }
        } => info!("all listeners exited"),
        _ = tokio::signal::ctrl_c() => info!("ctrl-c received, shutting down"),
    }

    Ok(())
}
