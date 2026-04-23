//! Charon command-line entrypoint.
//!
//! ```text
//! charon --config config/default.toml listen
//! charon --config config/default.toml listen --borrower 0xABC…
//! charon --config config/default.toml test-connection --chain bnb
//! ```

use std::path::PathBuf;
use std::sync::Arc;

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, U256};
use alloy::providers::{ProviderBuilder, WsConnect};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result, bail};
use charon_core::{
    Config, LendingProtocol, LiquidationOpportunity, LiquidationParams, OpportunityQueue,
    ProfitInputs, SwapRoute, calculate_profit,
};
use charon_executor::{
    DEFAULT_SUBMIT_TIMEOUT, GasOracle, GasParams, NonceManager, Simulator, SubmitError, Submitter,
    TxBuilder, gas_cost_usd_cents,
};
use charon_flashloan::{AaveFlashLoan, FlashLoanRouter};
use charon_protocols::VenusAdapter;
use charon_scanner::{
    BlockListener, ChainEvent, ChainProvider, DEFAULT_MAX_AGE, HealthScanner, PriceCache,
    TokenMetaCache,
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

/// Pre-broadcast gas-units estimate used by the profit gate. Venus
/// liquidation path through the Aave flashloan callback empirically
/// lands in ~1.1-1.6M gas; we use 1.5M to avoid gating out profitable
/// txs that would comfortably fit under the real `eth_estimateGas`
/// result fetched inside [`broadcast`]. The actual gas limit sent on
/// the wire is still `estimate_gas × 1.3` at broadcast time.
const PROFIT_GATE_ROUGH_GAS_UNITS: u64 = 1_500_000;

/// Native-asset Chainlink feed symbol on BSC. Used to price the gas
/// cost estimate in USD cents. If this feed is missing from the
/// config's `[chainlink.bnb]` table the bot refuses to start — a
/// missing BNB feed means we cannot compute gas cost at all.
const NATIVE_FEED_SYMBOL: &str = "BNB";

/// Multiplier applied to `eth_estimateGas` before broadcast. 30 %
/// headroom covers state drift between estimate and inclusion (vToken
/// index update, oracle writes, swap-pool reserve change, Venus
/// reentrancy into the callback) without overpaying on a happy-path
/// liquidation. BSC gas is cheap enough that the extra buffer is worth
/// the reduction in out-of-gas reverts.
const GAS_LIMIT_BUFFER_NUM: u64 = 13;
const GAS_LIMIT_BUFFER_DEN: u64 = 10;

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

        /// Broadcast signed liquidation txs when the simulator passes.
        /// Off by default — the pipeline runs scan + simulate only.
        /// Requires: `BOT_SIGNER_KEY` set, a non-zero
        /// `liquidator.contract_address`, and a `private_rpc_url` (or
        /// `allow_public_mempool = true`, dev-only).
        #[arg(long = "execute", default_value_t = false)]
        execute: bool,
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
    config
        .validate()
        .context("config validation failed — refusing to start")?;

    info!(
        chains = config.chain.len(),
        protocols = config.protocol.len(),
        flashloan_sources = config.flashloan.len(),
        liquidators = config.liquidator.len(),
        min_profit_usd = config.bot.min_profit_usd,
        "config loaded"
    );

    match cli.command {
        Command::Listen { borrowers, execute } => run_listen(config, borrowers, execute).await?,
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

/// Bundle of executor components needed to broadcast a simulated
/// opportunity. Present only when the operator ran `listen --execute`
/// and every safety gate passed; `None` means the pipeline is in
/// scan-only or scan-plus-simulate mode. Holds its own `GasOracle`
/// clone — the oracle is also used by the profit gate outside
/// `--execute` mode, so it lives at run-listen scope and the clone
/// here is just convenience.
struct ExecHarness {
    gas_oracle: GasOracle,
    nonce_manager: Arc<NonceManager>,
    submitter: Arc<Submitter>,
    signer_address: Address,
}

/// Wire the full Venus → scanner → profit → router → builder → sim
/// pipeline into the block-event drain loop.
///
/// With `--execute` the pipeline also signs and broadcasts any
/// opportunity whose simulator gate passes. Without it the pipeline is
/// read-only: simulation results are logged but nothing is signed or
/// sent.
async fn run_listen(config: Config, borrowers: Vec<Address>, execute: bool) -> Result<()> {
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
    if prices.get(NATIVE_FEED_SYMBOL).is_none() {
        bail!(
            "chainlink feed for '{NATIVE_FEED_SYMBOL}' missing or stale — gas cost cannot be priced"
        );
    }

    // Token metadata (symbol + decimals) for every Venus underlying.
    // Queried once at startup; the profit gate needs both fields to
    // convert a raw repay amount into USD cents via the price cache.
    let token_meta = Arc::new(
        TokenMetaCache::build(
            provider.as_ref(),
            adapter.underlying_to_vtoken.keys().copied(),
        )
        .await,
    );
    info!(
        tokens_cached = token_meta.len(),
        "token metadata cache built"
    );
    if token_meta.is_empty() {
        bail!(
            "token metadata cache is empty — no Venus underlying resolved its symbol/decimals; \
             profit gate would drop every opportunity. Check RPC and adapter wiring."
        );
    }

    // Gas oracle is needed by both the profit gate (every block) and
    // the broadcast path (under --execute). Build once, share by
    // value — `GasOracle` is `Copy`.
    let gas_oracle = GasOracle::new(config.bot.max_gas_gwei, bnb.priority_fee_gwei);

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

    // ── Execution harness (gated on --execute) ──
    // Built only when every safety gate passes: signer present,
    // non-zero liquidator, private-RPC URL present on the chain. Any
    // failure here aborts startup rather than silently degrading to
    // scan-only — `--execute` is an explicit operator intent.
    let exec_harness: Option<Arc<ExecHarness>> = if execute {
        let builder = tx_builder
            .as_ref()
            .context("--execute requires BOT_SIGNER_KEY to be set and parseable")?;
        if liquidator_cfg.contract_address == Address::ZERO {
            bail!("--execute refuses to run with zero-address liquidator");
        }
        let url = bnb
            .private_rpc_url
            .as_ref()
            .context("--execute requires a private_rpc_url on chain 'bnb' (https:// or wss://)")?;
        let submitter = Submitter::connect(url, bnb.private_rpc_auth.as_ref(), DEFAULT_SUBMIT_TIMEOUT)
            .await
            .context("--execute: failed to connect private-RPC submitter")?;
        let signer_address = builder.signer_address();
        let nonce_manager = NonceManager::init(provider.as_ref(), signer_address)
            .await
            .context("--execute: failed to initialise nonce manager")?;
        warn!(
            signer = %signer_address,
            liquidator = %liquidator_cfg.contract_address,
            max_gas_gwei = config.bot.max_gas_gwei,
            "execute mode ON — bot will sign and broadcast liquidations"
        );
        Some(Arc::new(ExecHarness {
            gas_oracle,
            nonce_manager: Arc::new(nonce_manager),
            submitter: Arc::new(submitter),
            signer_address,
        }))
    } else {
        None
    };

    // ── Profit-ordered queue ──
    let queue = Arc::new(tokio::sync::Mutex::new(OpportunityQueue::with_default_ttl()));

    info!(
        borrower_count = borrowers.len(),
        market_count = adapter.markets.len(),
        liquidatable_threshold = config.bot.liquidatable_threshold,
        near_liq_threshold = config.bot.near_liq_threshold,
        flash_sources = router.providers().len(),
        signer_present = tx_builder.is_some(),
        execute = exec_harness.is_some(),
        "pipeline ready"
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
                            exec_harness.clone(),
                            prices.clone(),
                            token_meta.clone(),
                            gas_oracle,
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
    exec_harness: Option<Arc<ExecHarness>>,
    prices: Arc<PriceCache>,
    token_meta: Arc<TokenMetaCache>,
    gas_oracle: GasOracle,
    queue: Arc<tokio::sync::Mutex<OpportunityQueue>>,
    provider: Arc<alloy::providers::RootProvider<alloy::pubsub::PubSubFrontend>>,
    min_profit_usd: f64,
) {
    let start = std::time::Instant::now();

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

    // 3a. One-shot gas snapshot for this block. Shared across every
    //    opportunity in this tick so we make a single `get_block`
    //    call no matter how many liquidatable positions fan out.
    //    `None` means the gas oracle refused to emit (ceiling tripped
    //    or RPC error); the profit gate treats that as "too expensive
    //    this block" and drops every candidate.
    let gas_snapshot = match gas_oracle.fetch_params(provider.as_ref()).await {
        Ok(params) => params,
        Err(err) => {
            warn!(chain = %chain, block, error = ?err, "gas oracle tick failed");
            None
        }
    };

    // 3b. Per-liquidatable: route flash loan, calc profit, build, simulate, queue.
    let liquidatable = scanner.liquidatable();
    let mut queued = 0usize;
    let mut broadcast = 0usize;
    for pos in liquidatable {
        match process_opportunity(
            &pos,
            adapter.as_ref(),
            router.as_ref(),
            tx_builder.as_deref(),
            simulator.as_deref(),
            exec_harness.as_deref(),
            prices.as_ref(),
            token_meta.as_ref(),
            gas_snapshot,
            provider.as_ref(),
            min_profit_usd,
            block,
            queue.clone(),
        )
        .await
        {
            Ok(ProcessOutcome::Queued) => queued += 1,
            Ok(ProcessOutcome::Broadcast) => {
                queued += 1;
                broadcast += 1;
            }
            Ok(ProcessOutcome::Dropped) => {}
            Err(err) => debug!(borrower = %pos.borrower, error = ?err, "opportunity dropped"),
        }
    }

    // 4. Drain queue stats.
    let q = queue.lock().await;
    let queue_len = q.len();
    drop(q);

    info!(
        chain = %chain,
        block,
        timestamp,
        tracked = borrowers.len(),
        healthy = counts.healthy,
        near_liq = counts.near_liquidation,
        liquidatable = counts.liquidatable,
        queued,
        broadcast,
        queue_len,
        block_ms = start.elapsed().as_millis() as u64,
        "pipeline tick"
    );
}

/// Outcome of a single `process_opportunity` invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessOutcome {
    /// Dropped at a profit / simulation / gas gate. Not in the queue.
    Dropped,
    /// Accepted and pushed to the profit-ordered queue. No broadcast.
    Queued,
    /// Accepted, queued, and broadcast to the private RPC.
    Broadcast,
}

/// Run one liquidatable position through the rest of the pipeline.
/// Returns a [`ProcessOutcome`] describing how far it made it, or
/// `Err` for unexpected failures the caller should log at `debug`.
#[allow(clippy::too_many_arguments)]
async fn process_opportunity(
    pos: &charon_core::Position,
    adapter: &VenusAdapter,
    router: &FlashLoanRouter,
    tx_builder: Option<&TxBuilder>,
    simulator: Option<&Simulator>,
    exec_harness: Option<&ExecHarness>,
    prices: &PriceCache,
    token_meta: &TokenMetaCache,
    gas_snapshot: Option<GasParams>,
    provider: &alloy::providers::RootProvider<alloy::pubsub::PubSubFrontend>,
    min_profit_usd: f64,
    queued_at_block: u64,
    queue: Arc<tokio::sync::Mutex<OpportunityQueue>>,
) -> Result<ProcessOutcome> {
    // a. Adapter: build protocol-specific liquidation params (vTokens + repay).
    let params = adapter.get_liquidation_params(pos)?;
    let LiquidationParams::Venus { repay_amount, .. } = &params;
    let repay = *repay_amount;

    // b. Router: pick cheapest flash-loan source.
    let Some(quote) = router.route(pos.debt_token, repay).await else {
        return Ok(ProcessOutcome::Dropped);
    };

    // c. Real profit calc. Any missing piece of price/meta/gas data
    //    deliberately drops the opportunity rather than falling back
    //    to an optimistic default — the profit gate is the last line
    //    of defence against broadcasting an unprofitable tx.
    let Some(debt_meta) = token_meta.get(&pos.debt_token) else {
        debug!(
            borrower = %pos.borrower,
            debt_token = %pos.debt_token,
            "no token metadata — dropped"
        );
        return Ok(ProcessOutcome::Dropped);
    };
    let Some(debt_price) = prices.get(&debt_meta.symbol) else {
        debug!(
            borrower = %pos.borrower,
            symbol = %debt_meta.symbol,
            "no chainlink price (or stale) — dropped"
        );
        return Ok(ProcessOutcome::Dropped);
    };
    let Some(native_price) = prices.get(NATIVE_FEED_SYMBOL) else {
        debug!(
            borrower = %pos.borrower,
            "no BNB/USD price (or stale) — dropped"
        );
        return Ok(ProcessOutcome::Dropped);
    };
    let Some(gas_params) = gas_snapshot else {
        debug!(
            borrower = %pos.borrower,
            "gas snapshot unavailable this block — dropped"
        );
        return Ok(ProcessOutcome::Dropped);
    };

    let repay_usd_cents = amount_to_usd_cents(
        repay,
        debt_meta.decimals,
        debt_price.price,
        debt_price.decimals,
    );
    let flash_fee_usd_cents = amount_to_usd_cents(
        quote.fee,
        debt_meta.decimals,
        debt_price.price,
        debt_price.decimals,
    );
    let gas_cost_usd = gas_cost_usd_cents(
        PROFIT_GATE_ROUGH_GAS_UNITS,
        gas_params.max_fee_per_gas,
        native_price.price,
        native_price.decimals,
    );

    let profit_inputs = ProfitInputs {
        repay_amount_usd_cents: repay_usd_cents,
        liquidation_bonus_bps: pos.liquidation_bonus_bps,
        flash_fee_usd_cents,
        gas_cost_usd_cents: gas_cost_usd,
        slippage_bps: DEFAULT_SLIPPAGE_BPS,
    };
    let net = match calculate_profit(&profit_inputs, min_profit_usd) {
        Ok(n) => n,
        Err(err) => {
            debug!(borrower = %pos.borrower, error = ?err, "profit gate dropped");
            return Ok(ProcessOutcome::Dropped);
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
    let calldata = match (tx_builder, simulator) {
        (Some(builder), Some(sim)) => {
            let bytes = builder.encode_calldata(&opp, &params)?;
            if let Err(err) = sim.simulate(provider, bytes.clone()).await {
                debug!(borrower = %pos.borrower, error = ?err, "simulation gate dropped");
                return Ok(ProcessOutcome::Dropped);
            }
            Some(bytes)
        }
        _ => None,
    };

    // f. Push to the profit-ordered queue before broadcast so a later
    //    submit failure still leaves a record of the ranked candidate.
    let mut outcome = ProcessOutcome::Queued;
    {
        let mut q = queue.lock().await;
        q.push(opp.clone(), queued_at_block);
    }

    // g. Broadcast (only when --execute set every gate, and we have
    //    calldata from the sim step — `exec_harness.is_some()` implies
    //    `tx_builder.is_some()` and `simulator.is_some()`).
    if let (Some(harness), Some(builder), Some(bytes)) = (exec_harness, tx_builder, calldata) {
        match broadcast(harness, builder, provider, bytes).await {
            Ok(hash) => {
                info!(
                    borrower = %pos.borrower,
                    net_profit_cents = net.net_usd_cents,
                    %hash,
                    "liquidation broadcast"
                );
                outcome = ProcessOutcome::Broadcast;
            }
            Err(err) => {
                warn!(
                    borrower = %pos.borrower,
                    error = ?err,
                    "broadcast failed — opportunity left in queue"
                );
            }
        }
    }

    Ok(outcome)
}

/// Sign and broadcast one opportunity that already passed simulation.
///
/// Nonce-gap handling: when the node rejects with "nonce too low" /
/// "already known" / similar, force a one-shot `NonceManager::resync`
/// so the next block's broadcast sees the canonical on-chain value.
/// Connection-lost errors leave the nonce where it is — the caller
/// will reconnect and the counter stays locally consistent with what
/// the pipeline actually issued.
async fn broadcast(
    harness: &ExecHarness,
    builder: &TxBuilder,
    provider: &alloy::providers::RootProvider<alloy::pubsub::PubSubFrontend>,
    calldata: alloy::primitives::Bytes,
) -> Result<alloy::primitives::TxHash> {
    // Fetch EIP-1559 fees; `None` = max-gas ceiling tripped, skip.
    let Some(fees) = harness
        .gas_oracle
        .fetch_params(provider)
        .await
        .context("broadcast: gas oracle failed")?
    else {
        bail!("gas ceiling tripped");
    };

    // Estimate gas on a minimal request — the provider only needs
    // to / from / data / fees to simulate.
    let est_tx = TransactionRequest::default()
        .with_from(harness.signer_address)
        .with_to(builder.liquidator())
        .with_input(calldata.clone())
        .with_max_fee_per_gas(fees.max_fee_per_gas)
        .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
    let gas_units = harness
        .gas_oracle
        .estimate_gas_units(provider, &est_tx)
        .await
        .context("broadcast: estimate_gas failed")?;
    let gas_limit = gas_units.saturating_mul(GAS_LIMIT_BUFFER_NUM) / GAS_LIMIT_BUFFER_DEN;

    // Claim a nonce locally (atomic — no race with a parallel
    // opportunity in the same block) and build the signed tx.
    let nonce = harness.nonce_manager.next();
    let tx = builder.build_tx(
        calldata,
        nonce,
        fees.max_fee_per_gas,
        fees.max_priority_fee_per_gas,
        gas_limit,
    );
    let raw = match builder.sign(tx).await {
        Ok(bytes) => bytes,
        Err(err) => {
            // Nonce already consumed by `next()` above. Sign failure
            // means no tx hits the wire, so the counter is ahead of
            // the chain — force a resync to avoid a permanent gap.
            if let Err(resync_err) = harness.nonce_manager.resync(provider).await {
                warn!(error = ?resync_err, "nonce resync failed after sign error");
            }
            return Err(err.context("broadcast: sign failed"));
        }
    };

    match harness.submitter.submit(raw).await {
        Ok(hash) => Ok(hash),
        Err(err) => {
            // Any RpcRejected leaves our local counter ahead of the
            // chain — resync unconditionally so we don't poison the
            // sequence on non-nonce rejections (insufficient funds,
            // gas too low, revert-on-broadcast). ConnectionLost is
            // transport-level: the tx may still land, so leave the
            // counter alone and let the next nonce-too-high reject
            // drive a resync on the following block.
            if matches!(err, SubmitError::RpcRejected(_))
                && let Err(resync_err) = harness.nonce_manager.resync(provider).await
            {
                warn!(error = ?resync_err, "nonce resync failed after rejection");
            }
            Err(anyhow::Error::new(err).context("broadcast: submit failed"))
        }
    }
}

/// Convert a token amount into USD cents using a Chainlink price.
///
/// Inputs:
/// - `amount` — raw units of the ERC-20 (`repay_amount`, `flash_fee`, …).
/// - `token_decimals` — decimals of the ERC-20 itself (`USDT` = 6, `BTCB` = 18).
/// - `price` — raw Chainlink aggregator answer (non-negative).
/// - `price_decimals` — feed's `decimals()` (typically 8 on BSC).
///
/// Math:
/// ```text
/// usd_cents = amount × price × 100 / 10^(token_decimals + price_decimals)
/// ```
/// Every step is on `U256` with `saturating_mul`; the final cast
/// clamps to `u64::MAX` so a pathological amount never panics.
fn amount_to_usd_cents(
    amount: U256,
    token_decimals: u8,
    price: U256,
    price_decimals: u8,
) -> u64 {
    let numerator = amount
        .saturating_mul(price)
        .saturating_mul(U256::from(100u64));
    let exponent = u64::from(token_decimals) + u64::from(price_decimals);
    let divisor = U256::from(10u64).pow(U256::from(exponent));
    if divisor.is_zero() {
        return 0;
    }
    let cents = numerator / divisor;
    u64::try_from(cents).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod amount_to_usd_cents_tests {
    use super::*;

    #[test]
    fn usdt_6_decimals_at_1_dollar_gives_cents_scaled_by_100() {
        // 1_000_000 raw USDT = 1 USDT @ 6 decimals × $1.00 = 100 cents
        let cents = amount_to_usd_cents(U256::from(1_000_000u64), 6, U256::from(100_000_000u64), 8);
        assert_eq!(cents, 100);
    }

    #[test]
    fn btcb_18_decimals_at_60k_dollars_gives_six_million_cents() {
        // 1 BTCB (1e18 raw) @ price 60_000 × 1e8 → 6_000_000 cents
        let cents = amount_to_usd_cents(
            U256::from(10u64).pow(U256::from(18u64)),
            18,
            U256::from(60_000u64) * U256::from(100_000_000u64),
            8,
        );
        assert_eq!(cents, 6_000_000);
    }

    #[test]
    fn zero_price_returns_zero_cents() {
        let cents = amount_to_usd_cents(U256::from(1u64), 18, U256::ZERO, 8);
        assert_eq!(cents, 0);
    }

    #[test]
    fn saturates_on_extreme_inputs() {
        // price ~= 10^30 × amount ~= 10^30 → numerator ~10^62 /
        // divisor 10^26 = 10^36, overflows u64 → saturates.
        let cents = amount_to_usd_cents(
            U256::from(10u64).pow(U256::from(30u64)),
            18,
            U256::from(10u64).pow(U256::from(30u64)),
            8,
        );
        assert_eq!(cents, u64::MAX);
    }
}
