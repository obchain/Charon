//! Charon command-line entrypoint.
//!
//! ```text
//! charon --config config/default.toml listen
//! charon --config config/default.toml listen --borrower 0xABC…
//! charon --config config/default.toml test-connection --chain bnb
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use alloy::primitives::{Address, U256};
use alloy::providers::{ProviderBuilder, WsConnect};
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};
use charon_core::{
    Config, LendingProtocol, LiquidationOpportunity, LiquidationParams, OpportunityQueue,
    ProfitInputs, SwapRoute, calculate_profit,
};
use charon_executor::{Simulator, TxBuilder};
use charon_flashloan::{AaveFlashLoan, FlashLoanRouter};
use charon_metrics::{bucket, drop_stage, sim_result};
use charon_protocols::VenusAdapter;
use charon_scanner::{
    BlockListener, ChainEvent, ChainProvider, DEFAULT_MAX_AGE, HealthScanner, PriceCache,
};
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

/// Size of the fan-in channel from listeners to the scanner pipeline.
/// One slot per ~5 blocks across all chains covers short stalls without
/// back-pressuring the listener task.
const CHAIN_EVENT_CHANNEL: usize = 1024;

/// Slippage budget applied to every profit estimate (basis points).
/// 0.5% — conservative default for PancakeSwap V3 hot-pair swaps.
const DEFAULT_SLIPPAGE_BPS: u16 = 50;

/// Placeholder gas estimate per liquidation tx (USD cents). Real
/// `eth_estimateGas × gas_price × native_price` lands once a gas
/// oracle is wired up.
const PLACEHOLDER_GAS_USD_CENTS: u64 = 50;

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
    /// Spawn block listeners + run the full Venus pipeline every new block.
    ///
    /// Borrower discovery from indexed events is a follow-up; pass
    /// `--borrower 0x…` one or more times to seed a test list.
    Listen {
        /// Addresses to scan on every new block. Repeat the flag for
        /// multiple borrowers. Empty list is allowed (the rest of the
        /// pipeline still spins up so the operator can confirm wiring).
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
        min_profit_usd = config.bot.min_profit_usd,
        "config loaded"
    );

    // Prometheus exporter — install the global recorder before any
    // subsystem records a metric. Disabled by the operator via
    // `[metrics] enabled = false` turns the bot into a zero-overhead
    // one-shot, which is handy for `test-connection` smoke runs.
    if config.metrics.enabled {
        charon_metrics::init(config.metrics.bind).await?;
        charon_metrics::set_build_info(
            env!("CARGO_PKG_VERSION"),
            option_env!("CHARON_GIT_SHA").unwrap_or("unknown"),
        );
    } else {
        info!("metrics disabled via config");
    }

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

/// Wire the full Venus → scanner → profit → router → builder → sim
/// pipeline into the block-event drain loop.
///
/// **Read-only end-to-end:** the simulator's verdict is logged but no
/// transaction is broadcast. Wiring the broadcast step lands with the
/// MEV / private-RPC submission tasks (#18).
async fn run_listen(config: Config, borrowers: Vec<Address>) -> Result<()> {
    // ── Adapters + scanner + price cache (existing #8/#9/#10 wiring) ──
    let bnb = config
        .chain
        .get("bnb")
        .context("chain 'bnb' not configured — required for v0.1")?;
    let venus_cfg = config
        .protocol
        .get("venus")
        .context("protocol 'venus' not configured — required for v0.1")?;
    let aave_cfg = config
        .flashloan
        .get("aave_v3_bsc")
        .context("flashloan 'aave_v3_bsc' not configured — required for v0.1")?;
    let liquidator_cfg = config
        .liquidator
        .get("bnb")
        .context("liquidator 'bnb' not configured — required for v0.1")?;

    // Single shared pub-sub provider — adapter, price cache, flash-loan
    // adapter, and tx builder all hang off it. Cuts WS connection
    // count from 4 to 1.
    let provider = Arc::new(
        ProviderBuilder::new()
            .on_ws(WsConnect::new(&bnb.ws_url))
            .await
            .context("listen: failed to open shared ws provider")?,
    );

    let adapter = Arc::new(VenusAdapter::connect(provider.clone(), venus_cfg.comptroller).await?);

    let scanner = Arc::new(HealthScanner::new(
        config.bot.liquidatable_threshold,
        config.bot.near_liq_threshold,
    )?);

    let price_feeds = config.chainlink.get("bnb").cloned().unwrap_or_default();
    let prices = Arc::new(PriceCache::new(
        provider.clone(),
        price_feeds,
        DEFAULT_MAX_AGE,
    ));
    prices.refresh_all().await;
    for sym in prices.symbols() {
        if let Some(p) = prices.get(sym) {
            info!(symbol = %sym, price = %p.price, decimals = p.decimals, "chainlink feed");
        }
    }

    // ── Flash-loan router (#13) ──
    // Liquidator address may be the placeholder zero — adapter still
    // builds, but `executeOperation` on a zero-address receiver would
    // never be reached because no broadcast happens here.
    let aave = Arc::new(
        AaveFlashLoan::connect(
            provider.clone(),
            aave_cfg.pool,
            liquidator_cfg.contract_address,
        )
        .await?,
    );
    let router = Arc::new(FlashLoanRouter::new(vec![aave.clone()]));

    // ── Tx builder + simulator (#14) ──
    // Both gracefully degrade if `BOT_SIGNER_KEY` is unset — encoding
    // and simulation can still run, but signing is skipped.
    let tx_builder: Option<Arc<TxBuilder>> = match std::env::var("BOT_SIGNER_KEY") {
        Ok(key) => match key.parse::<PrivateKeySigner>() {
            Ok(signer) => {
                let chain_id = adapter.chain_id;
                info!(
                    signer = %signer.address(),
                    liquidator = %liquidator_cfg.contract_address,
                    chain_id,
                    "tx builder ready"
                );
                Some(Arc::new(TxBuilder::new(
                    signer,
                    chain_id,
                    liquidator_cfg.contract_address,
                )))
            }
            Err(err) => {
                warn!(error = ?err, "BOT_SIGNER_KEY set but unparseable — tx builder disabled");
                None
            }
        },
        Err(_) => {
            info!("BOT_SIGNER_KEY not set — pipeline runs read-only (no tx signing/sim)");
            None
        }
    };

    let simulator = tx_builder.as_ref().map(|b| {
        Arc::new(Simulator::new(
            b.signer_address(),
            liquidator_cfg.contract_address,
        ))
    });

    // ── Profit-ordered queue ──
    let queue = Arc::new(tokio::sync::Mutex::new(OpportunityQueue::with_default_ttl()));

    info!(
        borrower_count = borrowers.len(),
        market_count = adapter.markets.len(),
        liquidatable_threshold = config.bot.liquidatable_threshold,
        near_liq_threshold = config.bot.near_liq_threshold,
        flash_sources = router.providers().len(),
        signer_present = tx_builder.is_some(),
        "pipeline ready (scan-only, no broadcast)"
    );

    // ── Block-event drain ──
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

    tokio::select! {
        _ = async {
            while let Some(event) = rx.recv().await {
                match event {
                    ChainEvent::NewBlock { chain, number, timestamp } => {
                        process_block(
                            chain,
                            number,
                            timestamp,
                            &borrowers,
                            adapter.clone(),
                            scanner.clone(),
                            router.clone(),
                            tx_builder.clone(),
                            simulator.clone(),
                            queue.clone(),
                            provider.clone(),
                            config.bot.min_profit_usd,
                        )
                        .await;
                    }
                }
            }
        } => info!("all listeners exited"),
        _ = tokio::signal::ctrl_c() => info!("ctrl-c received, shutting down"),
    }

    Ok(())
}

/// One full pipeline pass for one block. Errors are logged, never
/// propagated — the bot keeps draining events even if a single block's
/// scan has issues.
#[allow(clippy::too_many_arguments)]
async fn process_block(
    chain: String,
    block: u64,
    timestamp: u64,
    borrowers: &[Address],
    adapter: Arc<VenusAdapter>,
    scanner: Arc<HealthScanner>,
    router: Arc<FlashLoanRouter>,
    tx_builder: Option<Arc<TxBuilder>>,
    simulator: Option<Arc<Simulator>>,
    queue: Arc<tokio::sync::Mutex<OpportunityQueue>>,
    provider: Arc<alloy::providers::RootProvider<alloy::pubsub::PubSubFrontend>>,
    min_profit_usd: f64,
) {
    let start = std::time::Instant::now();

    charon_metrics::record_block_scanned(&chain);

    // 1. Adapter — fetch raw positions for the tracked borrower list.
    let positions = match adapter.fetch_positions(borrowers).await {
        Ok(p) => p,
        Err(err) => {
            warn!(chain = %chain, block, error = ?err, "venus fetch_positions failed");
            return;
        }
    };

    // 2. Scanner — classify into healthy / near-liq / liquidatable buckets.
    scanner.upsert(positions);
    let counts = scanner.bucket_counts();

    charon_metrics::set_position_bucket(&chain, bucket::HEALTHY, counts.healthy as u64);
    charon_metrics::set_position_bucket(&chain, bucket::NEAR_LIQ, counts.near_liquidation as u64);
    charon_metrics::set_position_bucket(&chain, bucket::LIQUIDATABLE, counts.liquidatable as u64);

    // 3. Per-liquidatable: route flash loan, calc profit, build, simulate, queue.
    let liquidatable = scanner.liquidatable();
    let mut queued = 0usize;
    for pos in liquidatable {
        match process_opportunity(
            &chain,
            &pos,
            adapter.as_ref(),
            router.as_ref(),
            tx_builder.as_deref(),
            simulator.as_deref(),
            provider.as_ref(),
            min_profit_usd,
            block,
            queue.clone(),
        )
        .await
        {
            Ok(true) => queued += 1,
            Ok(false) => {}
            Err(err) => {
                charon_metrics::record_opportunity_dropped(&chain, drop_stage::BUILD);
                debug!(borrower = %pos.borrower, error = ?err, "opportunity dropped");
            }
        }
    }

    // 4. Drain queue stats.
    let q = queue.lock().await;
    let queue_len = q.len();
    drop(q);

    charon_metrics::set_queue_depth(queue_len as u64);
    charon_metrics::observe_block_duration(&chain, start.elapsed().as_secs_f64());

    info!(
        chain = %chain,
        block,
        timestamp,
        tracked = borrowers.len(),
        healthy = counts.healthy,
        near_liq = counts.near_liquidation,
        liquidatable = counts.liquidatable,
        queued,
        queue_len,
        block_ms = start.elapsed().as_millis() as u64,
        "pipeline tick"
    );
}

/// Run one liquidatable position through the rest of the pipeline.
/// Returns `Ok(true)` if it landed in the queue, `Ok(false)` if it was
/// dropped at a profit / simulation gate, `Err` for unexpected
/// failures.
#[allow(clippy::too_many_arguments)]
async fn process_opportunity(
    chain: &str,
    pos: &charon_core::Position,
    adapter: &VenusAdapter,
    router: &FlashLoanRouter,
    tx_builder: Option<&TxBuilder>,
    simulator: Option<&Simulator>,
    provider: &alloy::providers::RootProvider<alloy::pubsub::PubSubFrontend>,
    min_profit_usd: f64,
    queued_at_block: u64,
    queue: Arc<tokio::sync::Mutex<OpportunityQueue>>,
) -> Result<bool> {
    // a. Adapter: build protocol-specific liquidation params (vTokens + repay).
    let params = adapter.get_liquidation_params(pos)?;
    let LiquidationParams::Venus { repay_amount, .. } = &params;
    let repay = *repay_amount;

    // b. Router: pick cheapest flash-loan source.
    let Some(quote) = router.route(pos.debt_token, repay).await else {
        charon_metrics::record_opportunity_dropped(chain, drop_stage::ROUTER);
        return Ok(false);
    };

    // c. Profit calc — placeholder USD math until precise per-token
    //    pricing lands. Treat repay_amount as 1:1 USD cents-equivalent
    //    after stripping decimals (works for stablecoin debt; underprices
    //    BNB/BTC/ETH debt — flagged as a follow-up).
    let repay_usd_cents = repay_to_usd_cents_placeholder(repay);
    let flash_fee_usd_cents = repay_to_usd_cents_placeholder(quote.fee);
    let profit_inputs = ProfitInputs {
        repay_amount_usd_cents: repay_usd_cents,
        liquidation_bonus_bps: pos.liquidation_bonus_bps,
        flash_fee_usd_cents,
        gas_cost_usd_cents: PLACEHOLDER_GAS_USD_CENTS,
        slippage_bps: DEFAULT_SLIPPAGE_BPS,
    };
    let net = match calculate_profit(&profit_inputs, min_profit_usd) {
        Ok(n) => n,
        Err(err) => {
            charon_metrics::record_opportunity_dropped(chain, drop_stage::PROFIT);
            debug!(borrower = %pos.borrower, error = ?err, "profit gate dropped");
            return Ok(false);
        }
    };

    // d. Build the executor's view of the opportunity. swap_route is
    //    a placeholder until the DEX optimizer lands; min_amount_out
    //    is set to `quote.amount + quote.fee` so the on-chain backstop
    //    catches an under-fill.
    let opp = LiquidationOpportunity {
        position: pos.clone(),
        debt_to_repay: repay,
        expected_collateral_out: pos.collateral_amount,
        flash_source: quote.source,
        swap_route: SwapRoute {
            token_in: pos.collateral_token,
            token_out: pos.debt_token,
            amount_in: pos.collateral_amount,
            min_amount_out: quote.amount + quote.fee,
            pool_fee: 3_000,
        },
        net_profit_usd_cents: net.net_usd_cents,
    };

    // e. Tx builder + simulator — only if the operator supplied
    //    BOT_SIGNER_KEY. Without it, push to the queue based on profit
    //    alone so dry-runs still surface ranked candidates.
    if let (Some(builder), Some(sim)) = (tx_builder, simulator) {
        let calldata = builder.encode_calldata(&opp, &params)?;
        match sim.simulate(provider, calldata).await {
            Ok(()) => charon_metrics::record_simulation(chain, sim_result::OK),
            Err(err) => {
                charon_metrics::record_simulation(chain, sim_result::REVERT);
                charon_metrics::record_opportunity_dropped(chain, drop_stage::SIMULATION);
                debug!(borrower = %pos.borrower, error = ?err, "simulation gate dropped");
                return Ok(false);
            }
        }
    }

    // f. Push to the profit-ordered queue.
    let profit_cents = opp.net_profit_usd_cents;
    let mut q = queue.lock().await;
    q.push(opp, queued_at_block);
    charon_metrics::record_opportunity_queued(chain, profit_cents);
    Ok(true)
}

/// Strip 18 decimals and convert to USD cents (×100), saturating to
/// `u64`. Treats every token as 1 USD per unit — fine for stablecoin
/// debt, wildly off for BNB/BTC/ETH. Real per-token pricing replaces
/// this once a token-decimals + symbol-resolution layer lands.
fn repay_to_usd_cents_placeholder(amount: U256) -> u64 {
    // 1 token (18 decimals) ≈ $1 → 100 cents. Divide by 1e16.
    let scale = U256::from(10u64).pow(U256::from(16u64));
    let cents = amount / scale;
    u64::try_from(cents).unwrap_or(u64::MAX)
}
