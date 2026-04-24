//! Charon command-line entrypoint.
//!
//! ```text
//! CHARON_CONFIG=/etc/charon/default.toml charon listen
//! charon --config config/default.toml listen
//! charon --config config/default.toml listen --borrower 0xABC…
//! charon --config config/default.toml test-connection --chain bnb
//! ```
//!
//! ## Pipeline overview
//!
//! `listen` spawns one [`BlockListener`] per configured chain and drains
//! the shared `ChainEvent` channel. For every non-backfill `NewBlock` on
//! the Venus chain the cadence scheduler [`ScanScheduler`] decides which
//! bucket of borrowers to refresh; the Venus adapter fetches their
//! positions pinned to the observed block; the [`HealthScanner`]
//! rebuckets them; each freshly `Liquidatable` position is then walked
//! through the full off-chain pipeline:
//!
//! 1. `get_liquidation_params` — adapter emits vToken + repay.
//! 2. `FlashLoanRouter::route` — cheapest source for (token, repay).
//! 3. `calculate_profit` — wei-native [`NetProfit`] breakdown.
//! 4. Threshold compare — `net_profit_usd_1e6` vs
//!    `config.bot.min_profit_usd_1e6`.
//! 5. `TxBuilder::encode_calldata` + `Simulator::simulate` — hard
//!    safety gate; only opportunities that survive `eth_call` reach
//!    the queue.
//! 6. `OpportunityQueue::push` — wei-ordered heap for the future
//!    broadcast stage.
//!
//! ## Security posture
//!
//! - Signer is held in a [`SecretString`] on the config, only
//!   materialised once via [`ExposeSecret::expose_secret`] at the
//!   single call site that builds the [`PrivateKeySigner`]. The
//!   exposed bytes are never stored back, never logged, never in any
//!   `Debug` format.
//! - No signer → no simulation → no enqueue. The scan-only mode is
//!   observable but refuses to produce any signed artefact.
//! - Backfill blocks are skipped — the next real head supersedes them
//!   and a fresh scan is cheaper than reconciling retroactive bucket
//!   transitions.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{ProviderBuilder, RootProvider, WsConnect};
use alloy::pubsub::PubSubFrontend;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};
use async_trait::async_trait;
use charon_core::{
    Config, FlashLoanQuote, LendingProtocol, LiquidationOpportunity, LiquidationParams,
    OpportunityQueue, Position, Price, ProfitInputs, calculate_profit,
};
use charon_executor::{Simulator, TxBuilder};
use charon_flashloan::{AaveFlashLoan, FlashLoanRouter};
use charon_protocols::VenusAdapter;
use charon_scanner::{
    BlockListener, ChainEvent, ChainProvider, DEFAULT_MAX_AGE, HealthScanner, PositionBucket,
    PriceCache, ScanScheduler,
};
use clap::{Parser, Subcommand};
use secrecy::ExposeSecret;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

/// Size of the fan-in channel from listeners to the scanner pipeline.
/// One slot per ~5 blocks across all chains covers short stalls without
/// back-pressuring the listener task.
const CHAIN_EVENT_CHANNEL: usize = 1024;

/// Slippage budget applied to every profit estimate (basis points).
/// 0.5% — conservative default for PancakeSwap V3 hot-pair swaps.
/// Tracked alongside the future gas oracle (#148); promoted to
/// per-route config once the router produces live quotes.
const DEFAULT_SLIPPAGE_BPS: u16 = 50;

/// Placeholder gas estimate per liquidation tx, in debt-token wei.
/// Replaced by live `eth_estimateGas × effective_gas_price × native /
/// debt_price` once the gas oracle (#148) lands.
const PLACEHOLDER_GAS_COST_DEBT_WEI: u128 = 3_000_000_000_000_000_000;

/// Gas limit supplied to `Simulator::simulate` until a real gas
/// estimate is wired up. Sized to comfortably cover a Venus
/// `liquidateBorrow` + PancakeSwap V3 swap round-trip.
const SIMULATION_GAS_LIMIT: u64 = 2_000_000;

/// Conservative debt-token-wei floor baked into
/// `SwapRoute.min_amount_out` on top of the flash-loan repayment.
/// Combined with the gas floor below it gives the on-chain
/// `CharonLiquidator.executeLiquidation` revert-guard a lower bound
/// independent of the off-chain profit math. Placeholder until the
/// per-token USD → wei conversion (#148) lands.
const STATIC_GAS_FLOOR_DEBT_WEI: u128 = 3_000_000_000_000_000_000;

/// Minimum-profit floor in debt-token smallest units, also baked into
/// `SwapRoute.min_amount_out`. Forces the DEX leg to return strictly
/// more than repay + fee + gas floor so a zero-net liquidation cannot
/// slip past the on-chain backstop. Replaced by a configured value
/// once USD → token pricing lands (#148).
const MIN_PROFIT_FLOOR_DEBT_WEI: u128 = 1_000_000_000_000_000_000;

/// Placeholder debt-token price (Chainlink 1e8 — 1 USD per token,
/// appropriate for stablecoin debt). Overridden by the PriceCache
/// feed per symbol once the price-cache → profit-calc bridge lands.
const PLACEHOLDER_DEBT_PRICE_USD_1E8: u64 = 100_000_000;

/// Placeholder debt-token decimals. Venus stablecoin debt on BSC is
/// 18 (USDT/BUSD) so this is a safe fallback for v0.1. A real
/// per-token decimals lookup lands alongside the price bridge.
const PLACEHOLDER_DEBT_DECIMALS: u8 = 18;

/// Wall-clock deadline for one per-block pipeline pass. If the
/// adapter, router, or simulator stalls beyond this we abandon the
/// tick so the event drain can pick up on the next block instead of
/// blocking across multiple heads.
const PER_BLOCK_TIMEOUT: Duration = Duration::from_millis(2_500);

/// Charon — multi-chain flash-loan liquidation bot.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Path to the TOML config file.
    ///
    /// No default — the operator must supply the path explicitly via
    /// `--config` or the `CHARON_CONFIG` environment variable. Avoids
    /// the silent cwd-relative `config/default.toml` fallback which
    /// breaks inside the Docker deploy image where WORKDIR may differ
    /// from the repo root.
    #[arg(long, short = 'c', env = "CHARON_CONFIG")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Spawn one block listener per configured chain and run the full
    /// Venus → router → profit → builder → simulator pipeline every
    /// real (non-backfill) block.
    ///
    /// Borrower discovery from indexed events is a follow-up; pass
    /// `--borrower 0x…` one or more times to seed a test list. An
    /// empty list is allowed — the adapter still connects so the
    /// operator can confirm the WS pipeline.
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

/// Shared Venus-side pipeline state assembled once at startup. Wrapped
/// in `Option` so the listener still drains events when
/// `[protocol.venus]` is absent (useful for operators running the
/// block pipeline against a chain before its adapter is wired).
struct VenusPipeline {
    chain_name: String,
    adapter: Arc<VenusAdapter>,
    scanner: Arc<HealthScanner>,
    scheduler: ScanScheduler,
    prices: Arc<PriceCache>,
    router: Arc<FlashLoanRouter>,
    liquidator: Address,
    provider: Arc<RootProvider<PubSubFrontend>>,
    /// Queue for opportunities that pass the simulation gate. The
    /// broadcast stage lands on top of this in a follow-up PR.
    queue: Arc<OpportunityQueue>,
    /// Built lazily on first actionable opportunity so scan-only
    /// runs (no signer configured) never touch the secret.
    tx_builder: tokio::sync::OnceCell<Option<Arc<TxBuilder>>>,
    simulator: tokio::sync::OnceCell<Option<Simulator>>,
    min_profit_usd_1e6: u64,
    chain_id: u64,
}

// Explicit multi-thread flavor so the concurrency contract survives any
// future trimming of tokio's `full` feature set.
#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Load `.env` if present. Silent no-op if the file isn't there.
    let _ = dotenvy::dotenv();

    // Structured logs go to stderr so `listen` can eventually emit a
    // JSON data stream on stdout without interleaving. Verbosity via
    // RUST_LOG.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    info!("charon starting up");
    info!(path = %cli.config.display(), "loading config");

    let config = Config::load(&cli.config)
        .with_context(|| format!("failed to load config from {}", cli.config.display()))?;
    config
        .validate()
        .context("config validation failed — refusing to start")?;

    // SECURITY: only counts and non-secret scalars here. Never log
    // ws_url, http_url, signer_key, or the full Debug of Config —
    // RPC URLs embed API keys, and the signer key is a SecretString.
    info!(
        chains = config.chain.len(),
        protocols = config.protocol.len(),
        flashloan_sources = config.flashloan.len(),
        liquidators = config.liquidator.len(),
        min_profit_usd_1e6 = config.bot.min_profit_usd_1e6,
        signer_configured = config.bot.signer_key.is_some(),
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
/// cleanly on SIGINT or SIGTERM so the Docker `stop` → SIGTERM →
/// SIGKILL sequence never tears mid-operation.
///
/// For every `NewBlock` event on a chain with a `[protocol.venus]`
/// entry the Venus adapter fetches positions anchored at the observed
/// block, pushes them through the bucketed [`HealthScanner`], and
/// limits fetches to buckets whose cadence fires this block via
/// [`ScanScheduler`]. A per-chain [`PriceCache`] is refreshed on each
/// scan tick so profit-ranking reads sub-heartbeat Chainlink feeds.
///
/// Liquidatable positions flow into the full off-chain pipeline:
/// router-picked flash loan → wei-native profit calc → tx encode →
/// `eth_call` simulation gate → profit-ordered queue.
///
/// Chains without a Venus protocol config still flow through the drain
/// loop but trigger no protocol scans (v0.1 scope).
///
/// Backfill blocks (synthesised during WebSocket reconnect) are logged
/// but not scanned — the state they would produce is superseded by
/// the next real head and a fresh scan is cheaper than retroactive
/// bucket transitions.
async fn run_listen(config: &Config, borrowers: Vec<Address>) -> Result<()> {
    if config.chain.is_empty() {
        anyhow::bail!("no chains configured — nothing to listen to");
    }

    // Venus pipeline state is currently single-chain (BNB) per config
    // scope. Build it only if `[protocol.venus]` exists and its target
    // chain plus flashloan+liquidator entries are all configured;
    // otherwise run the listener pipeline without a scanner.
    let venus: Option<Arc<VenusPipeline>> = match config.protocol.get("venus") {
        Some(venus_cfg) => {
            let chain_name = &venus_cfg.chain;
            let chain_cfg = config.chain.get(chain_name).with_context(|| {
                format!(
                    "protocol 'venus' references chain '{chain_name}' which is not in [chain.*]"
                )
            })?;

            // Single shared pub-sub provider for the Venus adapter,
            // price cache, flash-loan adapter, and simulator. Cuts
            // WebSocket count from 4 to 1.
            let provider = Arc::new(
                ProviderBuilder::new()
                    .on_ws(WsConnect::new(&chain_cfg.ws_url))
                    .await
                    .context("venus adapter: failed to open shared ws provider")?,
            );

            let adapter =
                Arc::new(VenusAdapter::connect(provider.clone(), venus_cfg.comptroller).await?);

            let scanner = Arc::new(HealthScanner::new(
                config.bot.liquidatable_threshold_bps,
                config.bot.near_liq_threshold_bps,
            )?);
            let scheduler = ScanScheduler::new(
                config.bot.hot_scan_blocks,
                config.bot.warm_scan_blocks,
                config.bot.cold_scan_blocks,
            );

            // Chainlink price cache. Empty map = no feeds configured,
            // cache stays idle and downstream stages fall back to the
            // protocol oracle. Reuses the Venus adapter's WS provider.
            let price_feeds = config.chainlink.get(chain_name).cloned().unwrap_or_default();
            let prices = Arc::new(PriceCache::new(
                provider.clone(),
                price_feeds,
                DEFAULT_MAX_AGE,
            ));
            prices.refresh_all().await;
            let fresh_feeds: Vec<String> = prices.symbols().map(str::to_string).collect();
            for sym in &fresh_feeds {
                if let Some(p) = prices.get(sym) {
                    info!(
                        symbol = %sym,
                        price = %p.price,
                        decimals = p.decimals,
                        "chainlink feed"
                    );
                }
            }

            // Flash-loan router — Aave V3 on BSC for v0.1. Requires a
            // liquidator address (receiver) from [liquidator.<chain>]
            // so `executeOperation` can be dispatched back to our
            // deployed contract. Absence of either stops pipeline
            // construction — the listener still runs event-drain-only.
            let router = match (
                config.flashloan.get("aave_v3_bsc"),
                config.liquidator.get(chain_name.as_str()),
            ) {
                (Some(fl_cfg), Some(liq_cfg)) => {
                    let data_provider = fl_cfg.data_provider.with_context(|| {
                        format!(
                            "flashloan 'aave_v3_bsc': missing data_provider for chain '{chain_name}'"
                        )
                    })?;
                    let aave = Arc::new(
                        AaveFlashLoan::connect(
                            provider.clone(),
                            fl_cfg.pool,
                            data_provider,
                            liq_cfg.contract_address,
                        )
                        .await
                        .context("aave v3: failed to connect flash-loan adapter")?,
                    );
                    Some((Arc::new(FlashLoanRouter::new(vec![aave])), liq_cfg.contract_address))
                }
                _ => {
                    info!(
                        "no [flashloan.aave_v3_bsc] + [liquidator.<chain>] — pipeline will scan \
                         but not build / simulate / enqueue"
                    );
                    None
                }
            };

            let chain_id = chain_cfg.chain_id;

            info!(
                chain = %chain_name,
                borrower_count = borrowers.len(),
                market_count = adapter.markets().await.len(),
                feed_count = fresh_feeds.len(),
                liquidatable_bps = config.bot.liquidatable_threshold_bps,
                near_liq_bps = config.bot.near_liq_threshold_bps,
                hot_blocks = scheduler.hot,
                warm_blocks = scheduler.warm,
                cold_blocks = scheduler.cold,
                flash_sources = router.as_ref().map(|(r, _)| r.providers().len()).unwrap_or(0),
                min_profit_usd_1e6 = config.bot.min_profit_usd_1e6,
                signer_configured = config.bot.signer_key.is_some(),
                "venus pipeline ready"
            );

            match router {
                Some((router, liquidator)) => Some(Arc::new(VenusPipeline {
                    chain_name: chain_name.clone(),
                    adapter,
                    scanner,
                    scheduler,
                    prices,
                    router,
                    liquidator,
                    provider,
                    queue: Arc::new(OpportunityQueue::with_default_ttl()),
                    tx_builder: tokio::sync::OnceCell::new(),
                    simulator: tokio::sync::OnceCell::new(),
                    min_profit_usd_1e6: config.bot.min_profit_usd_1e6,
                    chain_id,
                })),
                None => None,
            }
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

    // `ChainConfig: Clone` — we only borrow `config`, so each listener
    // task gets its own owned copy.
    for (name, chain_cfg) in &config.chain {
        let name = name.clone();
        let chain_cfg = chain_cfg.clone();
        let listener = BlockListener::new(name.clone(), chain_cfg, tx.clone());
        listeners.spawn(async move { (name, listener.run().await) });
    }
    // Drop our sender so the channel closes when every listener exits.
    drop(tx);

    info!("listen: draining chain events (Ctrl-C or SIGTERM to stop)");

    // The signer is loaded lazily on first actionable opportunity so
    // pure scanning works without a signer configured. The
    // `signer_key` field is `Option<SecretString>` — we
    // `expose_secret()` at exactly one call site, pass it straight to
    // `PrivateKeySigner::from_str`, and never retain the exposed
    // bytes.
    let signer_key = config.bot.signer_key.clone();

    // The first real (non-backfill) block on the Venus chain seeds
    // the scanner with the operator-supplied borrower list.
    // Subsequent scans pull from the scheduler-selected bucket
    // membership so we don't burn RPC re-fetching COLD positions
    // every block.
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
                            // snapshot the final state of the missed
                            // range.
                            continue;
                        }
                        let Some(pipeline) = venus.as_ref() else {
                            continue;
                        };
                        if pipeline.chain_name != chain {
                            continue;
                        }
                        // Per-block deadline: a stalled adapter /
                        // router / simulator must not block the event
                        // drain across multiple heads.
                        let pass = run_block_pipeline(
                            pipeline.clone(),
                            number,
                            timestamp,
                            &borrowers,
                            &mut seeded,
                            signer_key.as_ref(),
                        );
                        if let Err(_elapsed) =
                            tokio::time::timeout(PER_BLOCK_TIMEOUT, pass).await
                        {
                            warn!(
                                chain = %chain,
                                block = number,
                                timeout_ms = PER_BLOCK_TIMEOUT.as_millis() as u64,
                                "per-block pipeline pass timed out; moving on"
                            );
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

/// One full pipeline pass for one non-backfill block on the Venus
/// chain. Errors are logged, never propagated — the bot keeps draining
/// events even if a single block's scan has issues.
async fn run_block_pipeline(
    pipeline: Arc<VenusPipeline>,
    block: u64,
    timestamp: u64,
    borrowers: &[Address],
    seeded: &mut bool,
    signer_key: Option<&secrecy::SecretString>,
) {
    let start = std::time::Instant::now();

    // Which borrowers to scan this tick. First real block uses the
    // operator's seed list; thereafter the scheduler picks buckets
    // whose cadence fires.
    let scan_set: Vec<Address> = if !*seeded {
        *seeded = true;
        borrowers.to_vec()
    } else {
        let mut v = Vec::new();
        for b in [
            PositionBucket::Liquidatable,
            PositionBucket::NearLiquidation,
            PositionBucket::Healthy,
        ] {
            if pipeline.scheduler.should_scan(b, block) {
                v.extend(pipeline.scanner.borrowers_in_bucket(b));
            }
        }
        v
    };
    if scan_set.is_empty() {
        return;
    }

    // Refresh Chainlink prices so downstream profit-ranking reads
    // sub-heartbeat feeds. Individual feed failures are logged inside
    // `refresh_all` and do not abort the scan.
    pipeline.prices.refresh_all().await;

    let block_tag = BlockNumberOrTag::Number(block);
    let positions = match pipeline.adapter.fetch_positions(&scan_set, block_tag).await {
        Ok(p) => p,
        Err(err) => {
            warn!(
                chain = %pipeline.chain_name,
                block,
                error = ?err,
                "venus fetch_positions failed"
            );
            return;
        }
    };

    let returned = positions.len();
    pipeline.scanner.upsert(positions.clone());
    pipeline.scanner.prune(&positions);
    let counts = pipeline.scanner.bucket_counts();
    metrics::histogram!("charon_scanner_scan_duration_seconds")
        .record(start.elapsed().as_secs_f64());

    // Walk each liquidatable position through the e2e pipeline. Only
    // opportunities that pass the simulation gate reach the queue.
    let liquidatable = pipeline.scanner.liquidatable();
    let mut queued = 0usize;
    for pos in liquidatable {
        match process_opportunity(pipeline.clone(), &pos, block, signer_key).await {
            Ok(true) => queued += 1,
            Ok(false) => {}
            Err(err) => debug!(
                borrower = %pos.borrower,
                error = ?err,
                "opportunity dropped"
            ),
        }
    }

    let queue_len = pipeline.queue.len().await;
    info!(
        chain = %pipeline.chain_name,
        block,
        timestamp,
        tracked = scan_set.len(),
        returned,
        healthy = counts.healthy,
        near_liq = counts.near_liquidation,
        liquidatable = counts.liquidatable,
        queued,
        queue_len,
        scan_ms = start.elapsed().as_millis() as u64,
        "venus scan"
    );
}

/// Narrow trait objects let `process_opportunity` run against either
/// the production Simulator-over-provider path or a hand-rolled mock
/// in tests. Keeps the surface tiny — no simulation framework needed.
#[async_trait]
trait SimGate: Send + Sync {
    async fn encode_and_simulate(
        &self,
        opp: &LiquidationOpportunity,
        params: &LiquidationParams,
    ) -> Result<()>;
}

/// Production simulation gate: encode via `TxBuilder`, run `eth_call`
/// via `Simulator`.
struct ProductionSimGate<'a> {
    builder: &'a TxBuilder,
    sim: &'a Simulator,
    provider: &'a RootProvider<PubSubFrontend>,
}

#[async_trait]
impl<'a> SimGate for ProductionSimGate<'a> {
    async fn encode_and_simulate(
        &self,
        opp: &LiquidationOpportunity,
        params: &LiquidationParams,
    ) -> Result<()> {
        let calldata: Bytes = self.builder.encode_calldata(opp, params)?;
        self.sim
            .simulate(self.provider, calldata, SIMULATION_GAS_LIMIT)
            .await?;
        Ok(())
    }
}

/// Lazy-materialise the `(TxBuilder, Simulator)` pair the first time
/// an actionable opportunity reaches this pipeline. Scan-only runs
/// (no signer configured) never touch `signer_key` at all.
///
/// The signer bytes are exposed for a single synchronous call to
/// `PrivateKeySigner::from_str` and then dropped — `TxBuilder` owns
/// the signer handle, never the raw hex.
async fn ensure_executor<'a>(
    pipeline: &'a VenusPipeline,
    signer_key: Option<&secrecy::SecretString>,
) -> Option<(&'a TxBuilder, &'a Simulator)> {
    let builder_slot = pipeline
        .tx_builder
        .get_or_init(|| async {
            let key = signer_key?;
            // `expose_secret()` is the only place the raw hex is
            // materialised. `PrivateKeySigner::from_str` parses it
            // into an internal `k256::SecretKey`; the returned
            // `String` reference is dropped at end of this block.
            let raw = key.expose_secret();
            match raw.parse::<PrivateKeySigner>() {
                Ok(signer) => {
                    info!(
                        signer = %signer.address(),
                        liquidator = %pipeline.liquidator,
                        chain_id = pipeline.chain_id,
                        "tx builder ready"
                    );
                    Some(Arc::new(TxBuilder::new(
                        signer,
                        pipeline.chain_id,
                        pipeline.liquidator,
                    )))
                }
                Err(err) => {
                    // Log the error display *only* — alloy's
                    // PrivateKeySigner parse error does not echo the
                    // input hex, but we still stick to `{err}` (not
                    // `{err:?}`) to avoid any accidental leak.
                    warn!(
                        error = %err,
                        "CHARON_SIGNER_KEY unparseable — tx builder disabled, scan-only mode"
                    );
                    None
                }
            }
        })
        .await;
    let builder = builder_slot.as_ref()?;

    let sim_slot = pipeline
        .simulator
        .get_or_init(|| async { Some(Simulator::from_builder(builder, pipeline.liquidator)) })
        .await;
    let sim = sim_slot.as_ref()?;

    Some((builder.as_ref(), sim))
}

/// Run one liquidatable position through the rest of the pipeline.
///
/// Return value semantics:
/// * `Ok(true)`  — opportunity cleared every gate and landed in the queue.
/// * `Ok(false)` — dropped at a configured gate (no signer, no route,
///   below profit threshold, or simulation reverted). Not an error.
/// * `Err(..)`   — unexpected failure (profit-calc error, encoder
///   error, RPC error); caller logs.
///
/// Hard invariant (CLAUDE.md, #170): **an opportunity is never
/// enqueued unless it passed the simulation gate**. If no signer is
/// configured, this function returns `Ok(false)` before touching the
/// queue — scan-only mode observes, it never queues.
async fn process_opportunity(
    pipeline: Arc<VenusPipeline>,
    pos: &Position,
    block: u64,
    signer_key: Option<&secrecy::SecretString>,
) -> Result<bool> {
    // a. Adapter: build protocol-specific liquidation params (vTokens
    //    + repay).
    let params = pipeline
        .adapter
        .get_liquidation_params(pos)
        .context("venus: get_liquidation_params failed")?;

    // Exhaustive match so a new `LiquidationParams` variant forces
    // this call site to be audited. `LiquidationParams` is
    // `#[non_exhaustive]`, hence the trailing wildcard.
    let repay = match &params {
        LiquidationParams::Venus { repay_amount, .. } => *repay_amount,
        other => {
            debug!(
                borrower = %pos.borrower,
                variant = ?other,
                "unsupported liquidation protocol — skipping"
            );
            return Ok(false);
        }
    };

    // b. Router: pick cheapest flash-loan source for (debt token,
    //    repay amount).
    let Some(quote) = pipeline.router.route(pos.debt_token, repay).await else {
        return Ok(false);
    };

    // c. Profit calc — wei-native NetProfit breakdown. Until the
    //    per-token USD pricing layer lands (#148), the debt price is
    //    a stablecoin placeholder; the CLI is configured BSC-Venus
    //    v0.1 with stablecoin debt so the figure is accurate for the
    //    current deployment target.
    let debt_price =
        Price::new(PLACEHOLDER_DEBT_PRICE_USD_1E8).context("profit: invalid placeholder price")?;
    let opp_preview = preview_opportunity(pos, &quote, repay);
    let inputs = match ProfitInputs::from_opportunity(
        &opp_preview,
        opp_preview.expected_collateral_out,
        quote.fee,
        U256::from(PLACEHOLDER_GAS_COST_DEBT_WEI),
        DEFAULT_SLIPPAGE_BPS,
        debt_price,
        PLACEHOLDER_DEBT_DECIMALS,
    ) {
        Ok(i) => i,
        Err(err) => {
            debug!(borrower = %pos.borrower, error = ?err, "profit inputs rejected");
            return Ok(false);
        }
    };
    let net = match calculate_profit(&inputs, pipeline.min_profit_usd_1e6) {
        Ok(n) => n,
        Err(err) => {
            debug!(borrower = %pos.borrower, error = ?err, "profit gate dropped");
            return Ok(false);
        }
    };

    // d. Build the executor's view of the opportunity.
    //
    //    `swap_route.min_amount_out` is the on-chain backstop. It
    //    must strictly exceed what we owe (quote.amount + quote.fee)
    //    by at least gas floor + profit floor — otherwise the flash
    //    loan could close successfully while the bot posts a zero-
    //    or negative-net result on-chain. Today both floors are
    //    constants in debt-token smallest units; live gas-oracle +
    //    USD → token conversion (#148) replaces them.
    let gas_floor = U256::from(STATIC_GAS_FLOOR_DEBT_WEI);
    let profit_floor = U256::from(MIN_PROFIT_FLOOR_DEBT_WEI);
    let min_amount_out = quote
        .amount
        .saturating_add(quote.fee)
        .saturating_add(gas_floor)
        .saturating_add(profit_floor);

    let opp = LiquidationOpportunity::with_profit(
        opp_preview.position.clone(),
        repay,
        opp_preview.expected_collateral_out,
        quote.source,
        charon_core::SwapRoute {
            token_in: pos.collateral_token,
            token_out: pos.debt_token,
            amount_in: pos.collateral_amount,
            min_amount_out,
            // PancakeSwap V3 hot-pair default. `None` is for
            // fee-less routes (Balancer V2, Curve stable pool);
            // PancakeSwap V3 uses 0.3% for BSC stablecoin pairs.
            pool_fee: Some(3_000),
        },
        net,
    );

    // e. Simulation gate — the hard safety invariant: no signer → no
    //    simulation → no enqueue. We refuse to push opportunities
    //    that have not passed `eth_call`, because the downstream
    //    broadcast stage assumes every queued entry is known-good
    //    against the latest state.
    let Some((builder, sim)) = ensure_executor(pipeline.as_ref(), signer_key).await else {
        debug!(
            borrower = %pos.borrower,
            "simulation skipped — no signer configured; opportunity not enqueued"
        );
        return Ok(false);
    };

    let gate = ProductionSimGate {
        builder,
        sim,
        provider: pipeline.provider.as_ref(),
    };
    if let Err(err) = gate.encode_and_simulate(&opp, &params).await {
        debug!(borrower = %pos.borrower, error = ?err, "simulation gate dropped");
        return Ok(false);
    }

    // f. Push to the profit-ordered queue.
    pipeline.queue.push(opp, block).await;
    Ok(true)
}

/// Build the preview [`LiquidationOpportunity`] used as input to
/// [`ProfitInputs::from_opportunity`]. The final opportunity stored
/// in the queue comes out of [`LiquidationOpportunity::with_profit`]
/// with a real `net_profit_wei`; the preview just carries the
/// position + swap-amount context the profit calculator needs and
/// holds a placeholder `net_profit_wei = 0`.
fn preview_opportunity(
    pos: &Position,
    quote: &FlashLoanQuote,
    repay: U256,
) -> LiquidationOpportunity {
    LiquidationOpportunity {
        position: pos.clone(),
        debt_to_repay: repay,
        // Expected collateral out is the seized collateral after the
        // liquidation bonus. Until the adapter surfaces a precise
        // expected-seize figure, we forward `pos.collateral_amount`
        // as an upper bound; the profit calc uses this purely as the
        // slippage denominator, so an overestimate here is safe
        // (slippage is charged *against* it).
        expected_collateral_out: pos.collateral_amount,
        flash_source: quote.source,
        swap_route: charon_core::SwapRoute {
            token_in: pos.collateral_token,
            token_out: pos.debt_token,
            amount_in: pos.collateral_amount,
            min_amount_out: U256::ZERO,
            pool_fee: Some(3_000),
        },
        net_profit_wei: U256::ZERO,
    }
}

/// Drain a `JoinSet` of listener tasks and surface panics / errors
/// per chain. Returns when every listener has exited so the caller
/// can shut down.
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
