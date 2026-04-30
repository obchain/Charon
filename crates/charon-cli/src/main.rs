//! Charon command-line entrypoint.
//!
//! ```text
//! CHARON_CONFIG=/etc/charon/default.toml charon listen
//! charon --config config/default.toml listen
//! charon --config config/default.toml listen --borrower 0xABC…
//! charon --config config/default.toml listen --borrower-file borrowers.txt
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

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::eips::BlockNumberOrTag;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder, RootProvider, WsConnect};
use alloy::pubsub::PubSubFrontend;
use alloy::rpc::types::{BlockTransactionsKind, TransactionRequest};
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use charon_core::{
    Config, FlashLoanQuote, LendingProtocol, LiquidationOpportunity, LiquidationParams,
    OpportunityQueue, Position, Price, ProfitInputs, calculate_profit,
};
use charon_executor::{
    DEFAULT_SUBMIT_TIMEOUT, GasDecision, GasOracle, NonceManager, Simulator, SubmitError,
    Submitter, TxBuilder,
};
use charon_flashloan::{AaveFlashLoan, FlashLoanRouter};
use charon_metrics::{bucket, drop_reason, drop_stage, sim_result};
use charon_protocols::VenusAdapter;
use charon_scanner::{
    BlockListener, ChainEvent, ChainProvider, DEFAULT_MAX_AGE, HealthScanner, MempoolMonitor,
    OracleUpdate, PendingCache, PositionBucket, PriceCache, ScanScheduler, SimulationVerdict,
    TokenMetaCache,
};
use clap::{Parser, Subcommand};
use secrecy::ExposeSecret;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

/// Buffer size for the mempool's `OracleUpdate` channel. Sized so a
/// short burst of oracle-write txs at block-boundary time doesn't
/// back-pressure the monitor task.
const ORACLE_UPDATE_CHANNEL: usize = 256;

/// Env var the operator sets to enable the mempool monitor. Expected
/// value is the hex-encoded Venus oracle address whose write
/// selectors the monitor should track. Unset (or empty) skips the
/// mempool path cleanly so the CLI stays usable on profiles that do
/// not have a paid MEV stream. A future config-file knob can replace
/// this env var; for now keeping it env-only avoids a config-schema
/// change on feat/21.
const VENUS_ORACLE_ENV: &str = "CHARON_VENUS_ORACLE";

/// Size of the fan-in channel from listeners to the scanner pipeline.
/// One slot per ~5 blocks across all chains covers short stalls without
/// back-pressuring the listener task.
const CHAIN_EVENT_CHANNEL: usize = 1024;

/// Slippage budget applied to every profit estimate (basis points).
/// 0.5% — conservative default for PancakeSwap V3 hot-pair swaps.
/// Tracked alongside the future gas oracle (#148); promoted to
/// per-route config once the router produces live quotes.
const DEFAULT_SLIPPAGE_BPS: u16 = 50;

/// Pre-broadcast gas-units estimate used by the profit gate. Venus
/// liquidation path through the Aave flash-loan callback empirically
/// lands in ~1.1-1.6M gas; we use 1.5M to avoid gating out profitable
/// txs that would comfortably fit under the real `eth_estimateGas`
/// result fetched at broadcast time. The actual gas limit sent on
/// the wire is still `estimate_gas × 1.3` at broadcast time.
const PROFIT_GATE_ROUGH_GAS_UNITS: u64 = 1_500_000;

/// Native-asset Chainlink feed symbol on BSC. Used to price the gas
/// cost estimate (gas_units × max_fee_per_gas in native wei) into
/// debt-token wei via the ratio `native_price / debt_price`. If this
/// feed is missing from the `PriceCache` the bot refuses to start —
/// a missing BNB feed means the profit gate cannot be trusted.
const NATIVE_FEED_SYMBOL: &str = "BNB";

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

/// Wall-clock deadline for one per-block pipeline pass. If the
/// adapter, router, or simulator stalls beyond this we abandon the
/// tick so the event drain can pick up on the next block instead of
/// blocking across multiple heads.
const PER_BLOCK_TIMEOUT: Duration = Duration::from_millis(30_000);

/// Env var the operator must set (to `1`) before `--execute` is
/// honoured. A purely belt-and-braces second confirmation beyond the
/// CLI flag so a stale shell-history invocation cannot broadcast
/// signed liquidations by accident. Unset or any value other than
/// `1` refuses to build the execution harness, regardless of other
/// safety gates. Checked at startup; the listener then falls back to
/// scan+simulate and logs a loud warning so the operator notices.
const EXECUTE_CONFIRMATION_ENV: &str = "CHARON_EXECUTE_CONFIRMED";

/// Multiplicative broadcast-gas buffer on top of `eth_estimateGas`:
/// 130% (= 13/10). 30% headroom covers state drift between estimate
/// time and inclusion time — vToken index ticks, Chainlink oracle
/// writes landing in the same block, PancakeSwap reserve updates.
/// BSC gas is cheap enough that the extra buffer is worth the
/// reduction in out-of-gas reverts. Tuned alongside the simulation
/// gate's own 2 M hard ceiling so both agree on "tx will fit on
/// chain".
const BROADCAST_GAS_BUFFER_NUM: u64 = 13;
const BROADCAST_GAS_BUFFER_DEN: u64 = 10;

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

        /// Path to a text file of EIP-55 / 0x-hex borrower addresses,
        /// one per line. Lines starting with `#` and blank lines
        /// ignored. Merged with any `--borrower` flags.
        #[arg(long = "borrower-file")]
        borrower_file: Option<PathBuf>,

        /// Sign and broadcast the liquidation tx for every
        /// opportunity that clears the simulation gate. Off by
        /// default — the pipeline runs scan + simulate only. Requires
        /// all of:
        ///   * `bot.signer_key` populated (via `CHARON_SIGNER_KEY` env),
        ///   * every chain with a `[liquidator.<chain>]` section has
        ///     a non-zero `contract_address`,
        ///   * every chain has either `private_rpc_url` configured or
        ///     `allow_public_mempool = true` (dev only), and
        ///   * `CHARON_EXECUTE_CONFIRMED=1` in the environment.
        ///
        /// Any gate failing aborts startup — `--execute` is an
        /// explicit operator intent and must not silently degrade to
        /// scan-only.
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
    /// `(symbol, decimals)` for every Venus underlying the adapter
    /// discovered at startup. Used by the profit gate to convert a
    /// raw `repay_amount` into USD cents via `PriceCache` by symbol.
    /// Missing metadata (RPC failure on `symbol()` or `decimals()`)
    /// is treated the same as a missing price — the opportunity is
    /// dropped, never priced with a guess.
    token_meta: Arc<TokenMetaCache>,
    /// Per-chain EIP-1559 fee source used by the profit gate. Separate
    /// from `ExecHarness::gas_oracle` (which serves broadcast under
    /// `--execute`) so the profit path is always able to price gas,
    /// even in scan-only mode. Honours `bot.max_gas_wei` as the
    /// ceiling and the chain's `priority_fee_gwei` as the tip; has its
    /// own per-block cache so a tick with N liquidatable positions
    /// still issues a single `get_block` call.
    gas_oracle: Arc<GasOracle>,
    router: Arc<FlashLoanRouter>,
    liquidator: Address,
    provider: Arc<RootProvider<PubSubFrontend>>,
    /// Queue for opportunities that pass the simulation gate. The
    /// broadcast stage reads from this when the `--execute` harness
    /// is populated; entries are pushed *before* the broadcast call
    /// so a later submit failure still leaves a record of the ranked
    /// candidate.
    queue: Arc<OpportunityQueue>,
    /// Built lazily on first actionable opportunity so scan-only
    /// runs (no signer configured) never touch the secret.
    tx_builder: tokio::sync::OnceCell<Option<Arc<TxBuilder>>>,
    simulator: tokio::sync::OnceCell<Option<Simulator>>,
    min_profit_usd_1e6: u64,
    chain_id: u64,
    /// Present only when the operator ran `listen --execute` and
    /// every safety gate passed. `None` means scan-only or
    /// scan+simulate mode — `process_opportunity` observes the
    /// simulation gate and queues candidates, but never signs or
    /// broadcasts. Eagerly assembled at startup rather than
    /// lazy-initialised so a mis-configured private RPC or bad
    /// signer key is caught on boot, not on the first liquidatable
    /// position to land.
    exec_harness: Option<Arc<ExecHarness>>,
    /// Auto-discovered borrower set (issue #329). Populated in the
    /// background by `charon_scanner::backfill_borrowers` (one-shot
    /// historical sweep) and `run_discovery_live_once` (live WS tail
    /// of `Borrow` events). Merged into the per-block scan set so the
    /// scanner has a real population to bucket without operator
    /// `--borrower` seeding.
    discovery: charon_scanner::BorrowerSet,
}

/// Bundle of executor components needed to broadcast a simulated
/// opportunity. Present only when the operator ran `listen --execute`
/// and every safety gate passed; `None` means the pipeline is in
/// scan-only or scan+simulate mode.
///
/// Single-chain scope (BNB) for v0.1, matching the rest of the
/// `VenusPipeline` — a multi-chain harness is a follow-up when a
/// second adapter lands.
struct ExecHarness {
    /// Per-chain EIP-1559 fee source. Honours `bot.max_gas_wei` as
    /// the ceiling and the chain's `priority_fee_gwei` as the tip.
    /// Cached per block so a single tick doesn't spam the RPC with
    /// repeated `eth_feeHistory` reads.
    gas_oracle: GasOracle,
    /// Local atomic nonce counter. Initialised against the pending
    /// block on startup, incremented per `next()`, and resynced to
    /// the chain on any rejection that leaves the counter ahead of
    /// confirmed state.
    nonce_manager: Arc<NonceManager>,
    /// Private-RPC submitter. HTTPS / WSS only. Single-shot per
    /// submit — the caller owns retry + staleness decisions via the
    /// opportunity queue TTL.
    submitter: Arc<Submitter>,
    /// Hot-wallet signer address. Pre-materialised here so the
    /// broadcast path never re-derives it from the `TxBuilder` on
    /// the hot path.
    signer_address: Address,
    /// Host-only label for logs (scheme + host, no api-key in query
    /// string). Logged on every submit so operators can pivot on
    /// submit latency per endpoint.
    submitter_label: String,
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
        Command::Listen {
            borrowers,
            borrower_file,
            execute,
        } => {
            run_listen(&config, borrowers, borrower_file, execute).await?;
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

/// Parse a newline-delimited borrower file into a `Vec<Address>`.
///
/// File format:
/// - One EIP-55 / 0x-hex address per line.
/// - Blank lines and lines starting with `#` are ignored.
/// - A malformed line is logged at `warn!` (with the 1-based line
///   number) and skipped — partial recovery beats aborting the whole
///   ingest on one bad entry.
/// - A missing file emits a single `warn!` and returns an empty vec
///   so the caller can fall back to whatever `--borrower` flags
///   supplied without crashing.
fn parse_borrower_file(path: &std::path::Path) -> Vec<Address> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) => {
            warn!(
                path = %path.display(),
                error = ?err,
                "borrower file not readable — continuing with empty set from this source"
            );
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    let mut errors: usize = 0;
    for (idx, line) in raw.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match Address::from_str(trimmed) {
            Ok(addr) => out.push(addr),
            Err(err) => {
                errors = errors.saturating_add(1);
                // 1-based line number for operator-friendly logs.
                let line_no = idx.saturating_add(1);
                warn!(
                    path = %path.display(),
                    line = line_no,
                    error = ?err,
                    "borrower file: malformed address — skipping"
                );
            }
        }
    }
    if errors > 0 {
        info!(
            path = %path.display(),
            parsed = out.len(),
            errors,
            "borrower file parse summary"
        );
    }
    out
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
async fn run_listen(
    config: &Config,
    borrowers: Vec<Address>,
    borrower_file: Option<PathBuf>,
    execute: bool,
) -> Result<()> {
    // Merge `--borrower-file` (if any) into the seed list. Parse errors
    // on individual lines are warned-and-skipped — partial recovery is
    // strictly better than aborting startup on a single malformed line.
    // A missing file is also non-fatal: warn and continue with whatever
    // `--borrower` flags supplied.
    let mut borrowers = borrowers;
    if let Some(path) = borrower_file.as_ref() {
        let parsed = parse_borrower_file(path);
        info!(
            path = %path.display(),
            loaded = parsed.len(),
            "borrower file ingested"
        );
        borrowers.extend(parsed);
        // Dedupe via HashSet round-trip — preserves correctness even
        // when the operator double-lists an address across `--borrower`
        // flags and the file.
        let dedup: HashSet<Address> = borrowers.into_iter().collect();
        borrowers = dedup.into_iter().collect();
    }
    if config.chain.is_empty() {
        anyhow::bail!("no chains configured — nothing to listen to");
    }

    // ── Execute-gate safety checks (#305) ─────────────────────────────
    //
    // `--execute` is an explicit operator intent to sign and broadcast
    // liquidations — it must never silently degrade to scan-only. If
    // any gate below fails we abort startup with a descriptive error
    // rather than spinning up a half-wired pipeline.
    //
    // Four gates, all mandatory when `--execute` is set:
    //
    //   1. `bot.signer_key` is populated (empty strings are already
    //      collapsed to `None` by `normalize_empty_secrets` — this
    //      only checks presence, never inspects the value).
    //   2. Every chain that has a `[liquidator.<chain>]` entry
    //      references a non-zero `contract_address`. A zero address
    //      here would route `executeOperation` into the zero address
    //      on broadcast.
    //   3. Every chain has either `private_rpc_url` configured or
    //      has `allow_public_mempool = true` (dev-only). Enforced
    //      centrally in `Config::validate()` — re-checked here with
    //      a precise error message.
    //   4. `CHARON_EXECUTE_CONFIRMED=1` is set in the environment.
    //      Belt-and-braces second confirmation so stale shell
    //      history cannot broadcast by accident.
    //
    // These gates run *before* any WS connection or RPC call so a
    // misconfigured profile fails fast.
    if execute {
        if config.bot.signer_key.is_none() {
            bail!(
                "--execute refuses to start: bot.signer_key is not set (expected via \
                 CHARON_SIGNER_KEY env in the signer_key = \"${{CHARON_SIGNER_KEY}}\" substitution)"
            );
        }
        for (chain_name, liq_cfg) in &config.liquidator {
            if liq_cfg.contract_address == Address::ZERO {
                bail!(
                    "--execute refuses to start: [liquidator.{chain_name}] has zero-address \
                     contract_address — deploy the liquidator and set the address before \
                     broadcasting"
                );
            }
        }
        for (chain_name, chain_cfg) in &config.chain {
            if chain_cfg.private_rpc_url.is_none() && !chain_cfg.allow_public_mempool {
                bail!(
                    "--execute refuses to start: chain '{chain_name}' has no private_rpc_url \
                     and allow_public_mempool is false — liquidation txs must not leak to the \
                     public mempool"
                );
            }
        }
        let confirmed = std::env::var(EXECUTE_CONFIRMATION_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        if confirmed != "1" {
            bail!(
                "--execute refuses to start: set {EXECUTE_CONFIRMATION_ENV}=1 in the environment \
                 to confirm you intend to sign and broadcast liquidations"
            );
        }
        warn!(
            "execute mode confirmed — bot will sign and broadcast liquidations on every \
             simulation-passing opportunity"
        );
    }

    // Borrower-discovery background tasks (issue #329). Stored
    // outside the protocol-pipeline match so the SIGINT/SIGTERM path
    // can `abort()` them on graceful shutdown — the discovery tasks
    // do not run on the supervisor `JoinSet` because that set is
    // constructed later in this function, after the Venus pipeline
    // assembly that produces them.
    let mut discovery_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

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

            // ── Borrower auto-discovery (issue #329) ─────────────────
            //
            // Subscribe to Venus vToken `Borrow(address,uint,uint,uint)`
            // logs so the scanner has a real population to bucket without
            // requiring `--borrower 0x...` seeding. Backfill runs as a
            // bounded background task so startup is not blocked by a slow
            // free-tier RPC; the live tail starts immediately and merges
            // discovered addresses into the per-block scan set.
            //
            // Spawned task handles are pushed onto `discovery_tasks` so
            // the SIGINT/SIGTERM path can abort them cleanly — see the
            // shutdown branches at the bottom of this function.
            let discovery = charon_scanner::BorrowerSet::new();
            let vtokens_for_discovery = adapter.markets().await;
            if vtokens_for_discovery.is_empty() {
                warn!(
                    chain = %chain_name,
                    "discovery: VenusAdapter reports zero markets — discovery tasks not spawned"
                );
            } else {
                {
                    let provider = provider.clone();
                    let set = discovery.clone();
                    let vtokens = vtokens_for_discovery.clone();
                    let chain = chain_name.clone();
                    let discovery_cfg = chain_cfg.discovery.clone();
                    discovery_tasks.push(tokio::spawn(async move {
                        let head = match provider.get_block_number().await {
                            Ok(h) => h,
                            Err(err) => {
                                warn!(
                                    chain = %chain,
                                    error = ?err,
                                    "discovery backfill: get_block_number failed"
                                );
                                return;
                            }
                        };
                        let from = head.saturating_sub(discovery_cfg.backfill_blocks);
                        if let Err(err) = charon_scanner::backfill_borrowers_with_config(
                            provider.as_ref(),
                            vtokens,
                            &set,
                            from,
                            head,
                            &discovery_cfg,
                        )
                        .await
                        {
                            warn!(chain = %chain, error = ?err, "discovery backfill failed");
                        }
                    }));
                }
                {
                    let provider = provider.clone();
                    let set = discovery.clone();
                    let vtokens = vtokens_for_discovery.clone();
                    let chain_log = chain_name.clone();
                    let chain_drain = chain_name.clone();
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<alloy::primitives::Address>(
                        charon_scanner::DISCOVERY_CHANNEL_CAPACITY,
                    );
                    // Drain the notification channel into a debug log
                    // — the canonical sink is the BorrowerSet itself;
                    // this just gives operators a visible heartbeat
                    // that discovery is observing live events.
                    discovery_tasks.push(tokio::spawn(async move {
                        while let Some(addr) = rx.recv().await {
                            debug!(
                                chain = %chain_drain,
                                borrower = %addr,
                                "discovery: new borrower"
                            );
                        }
                    }));
                    // Live-tail supervisor — jittered exponential
                    // backoff with a 30 s cap, mirroring
                    // `BlockListener::run` so a flapping upstream WS
                    // does not cause a thundering-herd reconnect.
                    discovery_tasks.push(tokio::spawn(async move {
                        charon_scanner::run_discovery_live_with_reconnect(
                            provider, vtokens, set, tx, chain_log,
                        )
                        .await;
                    }));
                }
            }

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
            let price_feeds = config
                .chainlink
                .get(chain_name)
                .cloned()
                .unwrap_or_default();
            // Per-symbol max_age overrides: stable feeds (USDT/USDC/FDUSD)
            // update on deviation, not heartbeat, so the global 600s
            // default flags them as stale even when the price has not
            // moved. Operators set these in `[chainlink_max_age_secs.<chain>]`.
            let per_symbol_max_age: HashMap<String, Duration> = config
                .chainlink_max_age_secs
                .get(chain_name)
                .map(|m| {
                    m.iter()
                        .map(|(sym, secs)| (sym.clone(), Duration::from_secs(*secs)))
                        .collect()
                })
                .unwrap_or_default();
            let prices = Arc::new(PriceCache::with_per_symbol_max_age(
                provider.clone(),
                price_feeds,
                DEFAULT_MAX_AGE,
                per_symbol_max_age,
            ));
            // Native-feed preflight with bounded retry. Free-tier
            // RPCs throttle the very first batch of `latestRoundData`
            // calls (BSC oracle aggregator slot reads), so a single
            // 429 on BNB used to kill startup with "feed missing or
            // stale". Retry up to 5 times with 5 s gaps so the
            // throttle window passes; total worst case 25 s,
            // negligible vs. cold-start cost. After the final
            // attempt we still bail — a genuinely dead feed must
            // not silently degrade gas pricing.
            const PREFLIGHT_ATTEMPTS: usize = 5;
            const PREFLIGHT_GAP: Duration = Duration::from_secs(5);
            let mut bnb_ready = false;
            for attempt in 1..=PREFLIGHT_ATTEMPTS {
                prices.refresh_all().await;
                if prices.get(NATIVE_FEED_SYMBOL).is_some() {
                    bnb_ready = true;
                    break;
                }
                if attempt < PREFLIGHT_ATTEMPTS {
                    tracing::warn!(
                        symbol = NATIVE_FEED_SYMBOL,
                        attempt,
                        retry_in_ms = PREFLIGHT_GAP.as_millis() as u64,
                        "chainlink native feed not ready — retrying"
                    );
                    tokio::time::sleep(PREFLIGHT_GAP).await;
                }
            }

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

            // Native-feed preflight. The profit gate converts gas
            // cost (priced in native wei) into debt-token wei via the
            // ratio `native_price / debt_price`. Without the native
            // feed we would be guessing. Refuse to start rather than
            // silently drop every opportunity.
            if !bnb_ready {
                bail!(
                    "chainlink feed for '{NATIVE_FEED_SYMBOL}' missing or stale on chain \
                     '{chain_name}' after {PREFLIGHT_ATTEMPTS} attempts — gas cost cannot be priced"
                );
            }

            // Token metadata (symbol + decimals) for every Venus
            // underlying. Queried once at startup; the profit gate
            // needs both fields to convert a raw repay amount into
            // USD cents via the price cache. A token whose meta
            // calls fail is silently skipped by `TokenMetaCache` and
            // will be seen as "unknown meta" by the profit gate (→
            // opportunity dropped, not mispriced).
            let underlyings = adapter.underlying_tokens().await;
            let token_meta = Arc::new(
                TokenMetaCache::build(provider.as_ref(), underlyings.iter().copied()).await,
            );
            info!(
                chain = %chain_name,
                tokens_cached = token_meta.len(),
                "token metadata cache built"
            );
            if token_meta.is_empty() {
                bail!(
                    "token metadata cache is empty on chain '{chain_name}' — no Venus \
                     underlying resolved its symbol/decimals; profit gate would drop every \
                     opportunity. Check RPC and adapter wiring."
                );
            }

            // Gas oracle wired into the profit gate so every
            // opportunity is priced against the live base-fee
            // observed on-chain, not a static debt-wei constant.
            // ExecHarness builds its own oracle for broadcast; the
            // per-block cache on each instance means both paths
            // converge on one RPC call per tick even without
            // sharing state.
            let profit_gas_oracle = Arc::new(GasOracle::new_for_chain(
                chain_name.clone(),
                config.bot.max_gas_wei,
                chain_cfg.priority_fee_gwei,
            ));

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
                    // Cross-check `pool` against
                    // `IPoolAddressesProvider.getPool()` before standing up
                    // the adapter. Skipped on fork profile so a local anvil
                    // can run with a forked pool (the registry call works
                    // there too, but skipping keeps the fork resilient to
                    // anvil quirks). On mainnet RPCs this is a one-shot
                    // round-trip per startup that catches a stale `pool`
                    // address before the bot burns budget on reverts.
                    if let Some(addresses_provider) = fl_cfg.addresses_provider {
                        if config.bot.profile_tag.as_deref() == Some("fork") {
                            let url_hint = chain_cfg.http_url.as_str();
                            let looks_mainnet = ["bsc-dataseed", "binance.org", "quiknode.pro"]
                                .iter()
                                .any(|s| url_hint.contains(s));
                            if looks_mainnet {
                                error!(
                                    chain = %chain_name,
                                    rpc = url_hint,
                                    "fork profile bypassed Aave AddressesProvider check against \
                                     a URL that looks like mainnet — verify config/fork.toml is \
                                     pointed at loopback before continuing"
                                );
                            } else {
                                info!(
                                    chain = %chain_name,
                                    "skipping Aave AddressesProvider check (fork profile)"
                                );
                            }
                        } else {
                            AaveFlashLoan::validate_against_addresses_provider(
                                provider.clone(),
                                addresses_provider,
                                fl_cfg.pool,
                                "aave_v3_bsc",
                            )
                            .await
                            .context("aave v3: pool address mismatch — refusing to start")?;
                        }
                    }
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
                    Some((
                        Arc::new(FlashLoanRouter::new(vec![aave])),
                        liq_cfg.contract_address,
                    ))
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
                Some((router, liquidator)) => {
                    // ── Execution harness (--execute only) ────────────
                    //
                    // Eagerly assemble gas oracle + nonce manager +
                    // submitter when the operator opted in. Any
                    // failure here aborts startup: by this point
                    // `--execute` has already cleared the four safety
                    // gates above, so a missing private RPC here would
                    // be a config bug we refuse to paper over. The
                    // signer is materialised once, used only to derive
                    // the address for the nonce manager, and dropped
                    // — the builder+simulator path in
                    // `ensure_executor` re-parses the key from
                    // `SecretString` for its own use so the raw bytes
                    // never outlive either call site.
                    let exec_harness: Option<Arc<ExecHarness>> = if execute {
                        let signer_key = config
                            .bot
                            .signer_key
                            .as_ref()
                            .expect("--execute safety gate guarantees signer_key is Some");
                        let raw = signer_key.expose_secret();
                        let signer: PrivateKeySigner = raw.parse().context(
                            "--execute: bot.signer_key failed to parse as a PrivateKeySigner",
                        )?;
                        let signer_address = signer.address();
                        drop(signer);

                        let private_url = chain_cfg.private_rpc_url.as_ref().context(
                            "--execute: chain has no private_rpc_url (allow_public_mempool \
                             is dev-only and is not supported by the Submitter)",
                        )?;
                        let submitter = Submitter::connect(
                            private_url,
                            chain_cfg.private_rpc_auth.as_ref(),
                            chain_cfg.chain_id,
                            DEFAULT_SUBMIT_TIMEOUT,
                        )
                        .await
                        .context("--execute: failed to connect private-RPC submitter")?;
                        let submitter_label = submitter.endpoint().to_string();

                        let nonce_manager = NonceManager::init(provider.as_ref(), signer_address)
                            .await
                            .context("--execute: failed to initialise nonce manager")?;

                        let gas_oracle = GasOracle::new_for_chain(
                            chain_name.clone(),
                            config.bot.max_gas_wei,
                            chain_cfg.priority_fee_gwei,
                        );

                        warn!(
                            chain = %chain_name,
                            signer = %signer_address,
                            liquidator = %liquidator,
                            submitter = %submitter_label,
                            max_gas_wei = %config.bot.max_gas_wei,
                            priority_fee_gwei = chain_cfg.priority_fee_gwei,
                            "execute harness ready — liquidations will be signed and broadcast"
                        );

                        Some(Arc::new(ExecHarness {
                            gas_oracle,
                            nonce_manager: Arc::new(nonce_manager),
                            submitter: Arc::new(submitter),
                            signer_address,
                            submitter_label,
                        }))
                    } else {
                        None
                    };

                    Some(Arc::new(VenusPipeline {
                        chain_name: chain_name.clone(),
                        adapter,
                        scanner,
                        scheduler,
                        prices,
                        token_meta,
                        gas_oracle: profit_gas_oracle,
                        router,
                        liquidator,
                        provider,
                        queue: Arc::new(OpportunityQueue::with_default_ttl()),
                        tx_builder: tokio::sync::OnceCell::new(),
                        simulator: tokio::sync::OnceCell::new(),
                        min_profit_usd_1e6: config.bot.min_profit_usd_1e6,
                        chain_id,
                        exec_harness,
                        discovery: discovery.clone(),
                    }))
                }
                None => {
                    if execute {
                        bail!(
                            "--execute requires a [flashloan.aave_v3_bsc] and \
                             [liquidator.<chain>] pair — configure both before enabling \
                             broadcast"
                        );
                    }
                    None
                }
            }
        }
        None => {
            if execute {
                bail!(
                    "--execute requires [protocol.venus] to be configured — refusing to \
                     start an execute-mode listener without a protocol pipeline"
                );
            }
            info!("no [protocol.venus] configured — listener will drain events without scanning");
            None
        }
    };

    let (tx, mut rx) = mpsc::channel::<ChainEvent>(CHAIN_EVENT_CHANNEL);
    let mut listeners: tokio::task::JoinSet<(String, Result<()>)> = tokio::task::JoinSet::new();

    // ── Prometheus exporter (#222) ────────────────────────────────────
    // Install the global metrics recorder and push the HTTP-listener
    // future onto the same JoinSet that supervises block/mempool tasks
    // so a panic in `hyper`/`tokio` inside the exporter triggers the
    // same controlled-shutdown path (SIGINT / SIGTERM / supervise).
    // `install` returns `Ok(None)` on a repeat call in the same
    // process (#223) — nothing to supervise on re-invocation.
    if config.metrics.enabled {
        match charon_metrics::install(config.metrics.bind) {
            Ok(Some(exporter)) => {
                charon_metrics::set_build_info(
                    env!("CARGO_PKG_VERSION"),
                    option_env!("CHARON_GIT_SHA").unwrap_or("unknown"),
                );
                listeners.spawn(async move {
                    let res: Result<()> = exporter
                        .await
                        .map_err(|err| anyhow::anyhow!("metrics exporter: {err:?}"));
                    ("metrics".to_string(), res)
                });
                info!(bind = %config.metrics.bind, "metrics exporter listening on /metrics");
            }
            Ok(None) => {
                info!(
                    bind = %config.metrics.bind,
                    "metrics exporter already installed — skipping duplicate install"
                );
            }
            Err(err) => {
                // Refuse to start with a broken exporter: dashboards
                // would silently go dark and an operator would not
                // catch it until the next alert fire.
                return Err(anyhow::anyhow!(
                    "failed to install metrics exporter on {}: {err}",
                    config.metrics.bind
                ));
            }
        }
    } else {
        info!("metrics exporter disabled via [metrics].enabled = false");
    }

    // ── Mempool monitor (#46 / #299) ──────────────────────────────────
    // Spawn the pending-tx monitor alongside `BlockListener` on the
    // Venus pipeline's shared provider. Enabled only when the operator
    // sets `CHARON_VENUS_ORACLE` to a hex-encoded oracle address — most
    // public BSC RPCs do not expose `newPendingTransactions` (see the
    // mempool module's RPC-requirements docs). The returned
    // [`PendingCache`] is retained so the block-event drain can call
    // `drain_for_block` with the real confirmed-tx set each tick; the
    // [`OracleUpdate`] channel is currently logged only (pre-sign
    // builder wiring is explicitly non-goal for #299, so updates are
    // observed and dropped until the signer + deployed liquidator
    // bridge lands in a follow-up).
    //
    // The monitor is only wired when a Venus pipeline exists; without
    // one there is no consumer for either the cache drain or the
    // oracle-update channel.
    let mempool_cache: Option<Arc<PendingCache>> =
        match (venus.as_ref(), std::env::var(VENUS_ORACLE_ENV)) {
            (Some(pipeline), Ok(hex)) if !hex.is_empty() => {
                match Address::from_str(hex.trim()) {
                    Ok(oracle) => {
                        let monitor = Arc::new(MempoolMonitor::with_defaults_for_chain(
                            pipeline.chain_name.clone(),
                            pipeline.provider.clone(),
                            oracle,
                        ));
                        let cache = monitor.cache();
                        let (oracle_tx, mut oracle_rx) =
                            mpsc::channel::<OracleUpdate>(ORACLE_UPDATE_CHANNEL);
                        let monitor_for_task = monitor.clone();
                        let mempool_task_name = format!("mempool/{}", pipeline.chain_name);
                        listeners.spawn(async move {
                            let name = mempool_task_name;
                            let res: Result<()> = monitor_for_task
                                .run(oracle_tx)
                                .await
                                .map_err(|err| anyhow::anyhow!("mempool monitor: {err}"));
                            (name, res)
                        });
                        let watch_task_name = format!("oracle-watch/{}", pipeline.chain_name);
                        listeners.spawn(async move {
                            let name = watch_task_name;
                            // Non-goal: forwarding OracleUpdate into a
                            // pre-sign builder or into PriceCache
                            // refresh (signer + liquidator bridge and
                            // price-cache push-update API tracked
                            // separately). Log at debug so operators
                            // can verify the monitor is actually
                            // decoding oracle writes on their upstream
                            // without the flood reaching info.
                            while let Some(update) = oracle_rx.recv().await {
                                debug!(
                                    tx = %update.tx_hash(),
                                    asset = %update.asset(),
                                    kind = update.kind(),
                                    "oracle update observed (pre-sign builder not yet wired)"
                                );
                            }
                            (name, Ok::<(), anyhow::Error>(()))
                        });
                        info!(
                            oracle = %oracle,
                            chain = %pipeline.chain_name,
                            "mempool monitor spawned"
                        );
                        Some(cache)
                    }
                    Err(err) => {
                        warn!(
                            env = VENUS_ORACLE_ENV,
                            error = ?err,
                            "mempool oracle env var set but unparseable; mempool monitor disabled"
                        );
                        None
                    }
                }
            }
            (None, _) => {
                info!(
                    env = VENUS_ORACLE_ENV,
                    "mempool monitor disabled (no venus pipeline configured)"
                );
                None
            }
            _ => {
                info!(
                    env = VENUS_ORACLE_ENV,
                    "mempool monitor disabled (no oracle address configured)"
                );
                None
            }
        };

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

    // Operator heartbeat cadence (#333). Default 50 blocks ≈ 150s
    // on BSC. The block listener used to log only at DEBUG, so a
    // bot running cleanly under default `RUST_LOG=info` produced no
    // post-startup output for minutes at a time and operators
    // routinely assumed it had hung. `heartbeat_blocks = 0`
    // disables the heartbeat entirely (e.g. JSON-log pipelines that
    // prefer to derive liveness from the metrics surface).
    let heartbeat_blocks = config.bot.heartbeat_blocks;

    // The first real (non-backfill) block on the Venus chain seeds
    // the scanner with the operator-supplied borrower list.
    // Subsequent scans pull from the scheduler-selected bucket
    // membership so we don't burn RPC re-fetching COLD positions
    // every block.
    let mut seeded = false;
    tokio::select! {
        _ = async {
            while let Some(event) = rx.recv().await {
                #[allow(clippy::single_match)]
                match event {
                    ChainEvent::NewBlock {
                        chain,
                        number,
                        timestamp,
                        block_hash,
                        backfill,
                    } => {
                        tracing::debug!(
                            chain = %chain,
                            block = number,
                            timestamp = timestamp,
                            %block_hash,
                            backfill,
                            "cli drained event"
                        );
                        // Operator-visible heartbeat. Keyed off the
                        // chain block number so the cadence is
                        // deterministic across restarts (versus an
                        // internal counter that resets to 0 on every
                        // boot). Skip backfill heads so a reconnect
                        // storm does not produce a heartbeat per
                        // replayed block.
                        if !backfill
                            && heartbeat_blocks != 0
                            && number % heartbeat_blocks == 0
                        {
                            tracing::info!(
                                chain = %chain,
                                block = number,
                                cadence_blocks = heartbeat_blocks,
                                "block listener heartbeat"
                            );
                        }
                        if backfill {
                            // Skip backfill — the next real head will
                            // snapshot the final state of the missed
                            // range. The mempool drain is intentionally
                            // skipped here too: backfilled blocks are
                            // already several heads behind, so any
                            // pre-signed tx tied to them would have
                            // long since expired via cache TTL.
                            continue;
                        }
                        let Some(pipeline) = venus.as_ref() else {
                            continue;
                        };
                        if pipeline.chain_name != chain {
                            continue;
                        }

                        // Drain any pre-signed liquidations whose
                        // oracle trigger landed in this block before
                        // running the main scan pass. Independent of
                        // the scan — a mempool hiccup must not block
                        // the block pipeline.
                        drain_mempool_for_block(
                            pipeline.as_ref(),
                            block_hash,
                            mempool_cache.as_deref(),
                            signer_key.as_ref(),
                        )
                        .await;

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

    // Cancel the borrower-discovery background tasks on every exit
    // path. `JoinSet::shutdown` only abort()s tasks it owns; the
    // discovery tasks (issue #329) live outside that set because they
    // are constructed before `listeners`, so we abort them explicitly
    // here. `abort()` is best-effort but the tasks hold no on-chain
    // locks — only an Arc<DashMap> that drops cleanly.
    for handle in &discovery_tasks {
        handle.abort();
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

    // One-shot block counter per pipeline pass (#222). Counted even
    // when `scan_set` is empty — the block still ticked through the
    // drain loop and dashboards otherwise silently lose visibility on
    // "bot is alive, nothing to scan" intervals.
    charon_metrics::record_block_scanned(pipeline.chain_name.as_str());

    // Which borrowers to scan this tick. First real block uses the
    // operator's seed list; thereafter the scheduler picks buckets
    // whose cadence fires. Either way, every newly discovered borrower
    // (issue #329 — populated by the background `charon_scanner`
    // discovery tasks) is merged in so we ingest fresh addresses
    // without waiting a full COLD cadence.
    let mut scan_set: Vec<Address> = if !*seeded {
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
    // Pull in the discovered set every block. Addresses already in
    // `scan_set` (operator seed or bucket member) are de-duplicated
    // below, so the cost of including them here is one DashMap snapshot.
    for addr in pipeline.discovery.snapshot() {
        scan_set.push(addr);
    }
    scan_set.sort_unstable();
    scan_set.dedup();
    if scan_set.is_empty() {
        // Idle-tick observability: emit the per-bucket gauges, queue
        // depth, and block-duration histogram even when there is
        // nothing to scan, so dashboards distinguish "bot alive but
        // nothing to do" from "metrics pipeline broken". Without
        // these, only the scanner block counter advances and every
        // other panel renders "No data" until the first liquidatable
        // borrower lands.
        let chain = pipeline.chain_name.as_str();
        let counts = pipeline.scanner.bucket_counts();
        charon_metrics::set_position_bucket(chain, bucket::HEALTHY, counts.healthy as u64);
        charon_metrics::set_position_bucket(
            chain,
            bucket::NEAR_LIQ,
            counts.near_liquidation as u64,
        );
        charon_metrics::set_position_bucket(
            chain,
            bucket::LIQUIDATABLE,
            counts.liquidatable as u64,
        );
        charon_metrics::set_queue_depth(pipeline.queue.len().await as u64);
        charon_metrics::observe_block_duration(chain, start.elapsed().as_secs_f64());
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
            // Emit last-known per-bucket gauges + queue depth + block
            // duration even when the upstream RPC drops the entire
            // fetch — otherwise a single failing scan tick blanks
            // every dashboard panel until the next successful fetch.
            // Mirrors the idle-tick observability branch so "fetch
            // failed" is distinguishable from "metrics pipeline
            // broken".
            let chain = pipeline.chain_name.as_str();
            let counts = pipeline.scanner.bucket_counts();
            charon_metrics::set_position_bucket(chain, bucket::HEALTHY, counts.healthy as u64);
            charon_metrics::set_position_bucket(
                chain,
                bucket::NEAR_LIQ,
                counts.near_liquidation as u64,
            );
            charon_metrics::set_position_bucket(
                chain,
                bucket::LIQUIDATABLE,
                counts.liquidatable as u64,
            );
            charon_metrics::set_queue_depth(pipeline.queue.len().await as u64);
            charon_metrics::observe_block_duration(chain, start.elapsed().as_secs_f64());
            return;
        }
    };

    let returned = positions.len();
    pipeline.scanner.upsert(positions.clone());
    pipeline.scanner.prune(&positions);
    let counts = pipeline.scanner.bucket_counts();
    metrics::histogram!("charon_scanner_scan_duration_seconds")
        .record(start.elapsed().as_secs_f64());

    // Per-bucket position gauges — feat/22 `set_position_bucket`.
    // Emitted every tick so dashboards track live bucket sizes
    // rather than a stale counter that decays with TTL.
    let chain = pipeline.chain_name.as_str();
    charon_metrics::set_position_bucket(chain, bucket::HEALTHY, counts.healthy as u64);
    charon_metrics::set_position_bucket(chain, bucket::NEAR_LIQ, counts.near_liquidation as u64);
    charon_metrics::set_position_bucket(chain, bucket::LIQUIDATABLE, counts.liquidatable as u64);

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
    // Queue depth + full per-block pipeline duration. The histogram
    // uses the domain-scaled buckets registered in
    // `charon_metrics::install` so BSC's ~3s heartbeat lands inside
    // meaningful quantiles rather than collapsing into `+Inf`.
    charon_metrics::set_queue_depth(queue_len as u64);
    charon_metrics::observe_block_duration(chain, start.elapsed().as_secs_f64());
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
impl SimGate for ProductionSimGate<'_> {
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

    let chain = pipeline.chain_name.as_str();

    // Top-level "opportunities seen" counter (#368). Bumped before
    // any drop gate so the dropped-by-reason ratios denominate
    // against the same population.
    charon_metrics::record_opportunity_seen(chain);

    // b. Router: pick cheapest flash-loan source for (debt token,
    //    repay amount).
    let Some(quote) = pipeline.router.route(pos.debt_token, repay).await else {
        charon_metrics::record_opportunity_dropped(chain, drop_stage::ROUTER);
        charon_metrics::record_opportunity_dropped_reason(chain, drop_reason::NO_FLASHLOAN_SOURCE);
        return Ok(false);
    };

    // c. Profit calc — wei-native NetProfit breakdown with real
    //    per-token pricing (#148 follow-up / #306). Every missing
    //    piece of price/meta/gas data is a hard drop: the profit
    //    gate is the last line of defence against broadcasting an
    //    unprofitable tx, so a "maybe profitable" signal is never
    //    produced against fallback values.
    let Some(debt_meta) = pipeline.token_meta.get(&pos.debt_token) else {
        charon_metrics::record_opportunity_dropped(chain, drop_stage::PROFIT);
        charon_metrics::record_opportunity_dropped_reason(chain, drop_reason::UNPROFITABLE);
        debug!(
            borrower = %pos.borrower,
            debt_token = %pos.debt_token,
            "no token metadata — dropped"
        );
        return Ok(false);
    };
    let Some(debt_cached) = pipeline.prices.get(&debt_meta.symbol) else {
        charon_metrics::record_opportunity_dropped(chain, drop_stage::PROFIT);
        charon_metrics::record_opportunity_dropped_reason(chain, drop_reason::UNPROFITABLE);
        debug!(
            borrower = %pos.borrower,
            symbol = %debt_meta.symbol,
            "no chainlink price (or stale) — dropped"
        );
        return Ok(false);
    };
    let Some(native_cached) = pipeline.prices.get(NATIVE_FEED_SYMBOL) else {
        charon_metrics::record_opportunity_dropped(chain, drop_stage::PROFIT);
        charon_metrics::record_opportunity_dropped_reason(chain, drop_reason::UNPROFITABLE);
        debug!(
            borrower = %pos.borrower,
            "no native/USD price (or stale) — dropped"
        );
        return Ok(false);
    };

    // Chainlink answers arrive with the feed's native decimals
    // (`decimals` is typically 8 on BSC but is read per-feed); the
    // `Price` wire-format used by `ProfitInputs` is strictly 1e8.
    // `scaled_to(8)` normalises both without relying on a constant.
    let debt_price_1e8 = match u64::try_from(debt_cached.scaled_to(8)) {
        Ok(v) if v > 0 => v,
        _ => {
            charon_metrics::record_opportunity_dropped(chain, drop_stage::PROFIT);
            charon_metrics::record_opportunity_dropped_reason(chain, drop_reason::UNPROFITABLE);
            debug!(
                borrower = %pos.borrower,
                symbol = %debt_meta.symbol,
                "debt price out of u64 range or zero — dropped"
            );
            return Ok(false);
        }
    };
    let native_price_1e8 = match u64::try_from(native_cached.scaled_to(8)) {
        Ok(v) if v > 0 => v,
        _ => {
            charon_metrics::record_opportunity_dropped(chain, drop_stage::PROFIT);
            charon_metrics::record_opportunity_dropped_reason(chain, drop_reason::UNPROFITABLE);
            debug!(
                borrower = %pos.borrower,
                "native price out of u64 range or zero — dropped"
            );
            return Ok(false);
        }
    };
    let debt_price = match Price::new(debt_price_1e8) {
        Ok(p) => p,
        Err(err) => {
            charon_metrics::record_opportunity_dropped(chain, drop_stage::PROFIT);
            charon_metrics::record_opportunity_dropped_reason(chain, drop_reason::UNPROFITABLE);
            debug!(borrower = %pos.borrower, error = ?err, "debt Price rejected");
            return Ok(false);
        }
    };

    // Gas cost in debt-token wei:
    //
    //   native_wei  = gas_units * max_fee_per_gas
    //   debt_wei    = native_wei * native_price_1e8 / debt_price_1e8
    //               * 10^debt_decimals / 10^native_decimals
    //
    // BSC native (BNB) is 18 decimals. We assume 18 here — the same
    // assumption baked into `STATIC_GAS_FLOOR_DEBT_WEI` / previous
    // placeholder pricing. A per-chain native-decimals lookup is a
    // follow-up if non-EVM or non-18-dec native chains enter scope.
    const NATIVE_DECIMALS: u8 = 18;
    let gas_decision = match pipeline
        .gas_oracle
        .fetch_params(pipeline.provider.as_ref(), Some(block))
        .await
    {
        Ok(d) => d,
        Err(err) => {
            charon_metrics::record_opportunity_dropped(chain, drop_stage::PROFIT);
            charon_metrics::record_opportunity_dropped_reason(chain, drop_reason::UNPROFITABLE);
            debug!(
                borrower = %pos.borrower,
                error = ?err,
                "gas oracle fetch failed — dropped"
            );
            return Ok(false);
        }
    };
    let gas_params = match gas_decision {
        GasDecision::Proceed(p) => p,
        GasDecision::SkipCeilingExceeded {
            max_fee_wei,
            ceiling_wei,
        } => {
            charon_metrics::record_opportunity_dropped(chain, drop_stage::PROFIT);
            charon_metrics::record_opportunity_dropped_reason(chain, drop_reason::GAS_CEILING);
            debug!(
                borrower = %pos.borrower,
                %max_fee_wei,
                %ceiling_wei,
                "gas ceiling exceeded — dropped"
            );
            return Ok(false);
        }
        // `GasDecision` is `#[non_exhaustive]` — any future skip
        // variant must fail closed rather than silently proceed.
        _ => {
            charon_metrics::record_opportunity_dropped(chain, drop_stage::PROFIT);
            charon_metrics::record_opportunity_dropped_reason(chain, drop_reason::UNPROFITABLE);
            debug!(
                borrower = %pos.borrower,
                "unrecognised GasDecision variant — dropped"
            );
            return Ok(false);
        }
    };
    let gas_cost_debt_wei = gas_cost_in_debt_wei(
        PROFIT_GATE_ROUGH_GAS_UNITS,
        gas_params.max_fee_per_gas,
        native_price_1e8,
        debt_price_1e8,
        NATIVE_DECIMALS,
        debt_meta.decimals,
    );

    let opp_preview = preview_opportunity(pos, &quote, repay);
    let inputs = match ProfitInputs::from_opportunity(
        &opp_preview,
        opp_preview.expected_collateral_out,
        quote.fee,
        gas_cost_debt_wei,
        DEFAULT_SLIPPAGE_BPS,
        debt_price,
        debt_meta.decimals,
    ) {
        Ok(i) => i,
        Err(err) => {
            charon_metrics::record_opportunity_dropped(chain, drop_stage::PROFIT);
            charon_metrics::record_opportunity_dropped_reason(chain, drop_reason::UNPROFITABLE);
            debug!(borrower = %pos.borrower, error = ?err, "profit inputs rejected");
            return Ok(false);
        }
    };
    let net = match calculate_profit(&inputs, pipeline.min_profit_usd_1e6) {
        Ok(n) => n,
        Err(err) => {
            charon_metrics::record_opportunity_dropped(chain, drop_stage::PROFIT);
            charon_metrics::record_opportunity_dropped_reason(chain, drop_reason::UNPROFITABLE);
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
        // Scan-only mode: no signer, no simulation, no enqueue. Count
        // as a simulation-stage drop so dashboards surface scan-only
        // runs without hiding them under router/profit gates.
        charon_metrics::record_opportunity_dropped(chain, drop_stage::SIMULATION);
        charon_metrics::record_opportunity_dropped_reason(chain, drop_reason::SIM_REVERT);
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
        charon_metrics::record_simulation(chain, sim_result::REVERT);
        charon_metrics::record_opportunity_dropped(chain, drop_stage::SIMULATION);
        charon_metrics::record_opportunity_dropped_reason(chain, drop_reason::SIM_REVERT);
        debug!(borrower = %pos.borrower, error = ?err, "simulation gate dropped");
        return Ok(false);
    }
    charon_metrics::record_simulation(chain, sim_result::OK);

    // f. Push to the profit-ordered queue. `simulated = true` because
    //    the production path only reaches here after a successful
    //    `eth_call` gate — dry-run entries never get here.
    //    **Queue-before-broadcast**: insert into the queue first so a
    //    later submit failure still leaves a record of the ranked
    //    candidate, and so the (future) broadcast-retry stage can
    //    walk the queue without racing the broadcast attempt below.
    let profit_cents = wei_to_usd_cents(opp.net_profit_wei);
    pipeline.queue.push(opp.clone(), block).await;
    charon_metrics::record_opportunity_queued(chain, profit_cents, true);

    // g. Broadcast stage — only when `--execute` assembled the
    //    harness on startup. Re-encodes calldata (pure local work;
    //    no RPC) rather than threading the sim calldata through the
    //    gate trait. The broadcast is deliberately best-effort: any
    //    failure is logged and the opportunity stays queued, so a
    //    future retry stage can pick it up. `Ok(true)` still reports
    //    "enqueued"; broadcast success is an additional metric label.
    if let Some(harness) = pipeline.exec_harness.as_ref() {
        match broadcast_opportunity(pipeline.as_ref(), harness, &opp, &params, builder).await {
            Ok(tx_hash) => {
                info!(
                    chain = %pipeline.chain_name,
                    borrower = %pos.borrower,
                    %tx_hash,
                    submitter = %harness.submitter_label,
                    net_profit_cents = profit_cents,
                    "liquidation broadcast"
                );
            }
            Err(err) => {
                charon_metrics::record_opportunity_dropped(chain, drop_stage::BUILD);
                charon_metrics::record_opportunity_dropped_reason(
                    chain,
                    drop_reason::SUBMIT_FAILED,
                );
                warn!(
                    chain = %pipeline.chain_name,
                    borrower = %pos.borrower,
                    error = %format!("{err:#}"),
                    submitter = %harness.submitter_label,
                    "broadcast failed — opportunity left in queue for future retry"
                );
            }
        }
    }

    Ok(true)
}

/// Sign and broadcast one simulation-passing opportunity.
///
/// Flow:
///   1. `GasOracle::fetch_params` for the current block. A
///      `SkipCeilingExceeded` verdict drops the opportunity (the
///      current tip of the mempool is too expensive for our
///      `bot.max_gas_wei` ceiling to make economic sense).
///   2. `eth_estimateGas` on a request with the resolved fees, then
///      scale by [`BROADCAST_GAS_BUFFER_NUM`]/[`BROADCAST_GAS_BUFFER_DEN`]
///      (30% headroom) to cover state drift between estimate and
///      inclusion.
///   3. `NonceManager::next()` claims a nonce atomically. The local
///      counter is the source of truth for the in-flight window; the
///      pending-block read only runs on init + resync, never on the
///      hot path.
///   4. `TxBuilder::build_tx` + `TxBuilder::sign` produce the raw
///      EIP-2718 envelope.
///   5. `Submitter::submit` posts to the private RPC with a single
///      attempt. Timeout / rejection handling is encoded in
///      `SubmitError`.
///
/// Nonce-gap handling — invariants mirror the submit doc:
/// * Sign failure: tx never hit the wire but `next()` already
///   consumed a nonce, so the counter is ahead of the chain. Force a
///   resync before returning so the next broadcast sees canonical
///   state.
/// * `SubmitError::RpcRejected`: node rejected the tx (bad nonce,
///   revert-on-broadcast, insufficient funds, rate-limit). Counter is
///   ahead of the chain — resync.
/// * `SubmitError::Timeout` / `SubmitError::ConnectionLost`: tx may
///   still land (transport blip, vendor side spike). Leaving the
///   counter alone is the correct call — a bogus resync here would
///   reuse a nonce that a later block confirms. The next
///   rejection-with-nonce-too-low drives a recovery.
async fn broadcast_opportunity(
    pipeline: &VenusPipeline,
    harness: &ExecHarness,
    opp: &LiquidationOpportunity,
    params: &LiquidationParams,
    builder: &TxBuilder,
) -> Result<alloy::primitives::TxHash> {
    let calldata: Bytes = builder
        .encode_calldata(opp, params)
        .context("broadcast: re-encode calldata failed")?;

    // 1. Gas params (fee pair + ceiling check).
    let decision = harness
        .gas_oracle
        .fetch_params(pipeline.provider.as_ref(), None)
        .await
        .context("broadcast: gas oracle fetch_params failed")?;
    let gas_params = match decision {
        GasDecision::Proceed(p) => p,
        GasDecision::SkipCeilingExceeded {
            max_fee_wei,
            ceiling_wei,
        } => {
            bail!(
                "gas ceiling tripped: max_fee_wei={max_fee_wei} exceeds \
                 bot.max_gas_wei={ceiling_wei}"
            );
        }
        // `GasDecision` is `#[non_exhaustive]`; a new skip reason
        // added upstream lands here and is treated as a drop until
        // the broadcast call site is taught to handle it. Safer than
        // a blanket `SkipCeilingExceeded` mapping that would silently
        // reinterpret unrelated drops.
        _ => bail!("broadcast: unknown gas-oracle decision variant"),
    };

    // 2. Estimate gas on a minimal request — provider needs
    //    from/to/data + fees to simulate execution. Then apply a
    //    130% buffer (30% headroom). `provider.estimate_gas` is used
    //    directly rather than `GasOracle::estimate_gas_units`
    //    because the oracle's internal 120% buffer would compound
    //    with ours; we want one explicit buffer at one call site.
    let est_tx = TransactionRequest::default()
        .with_from(harness.signer_address)
        .with_to(builder.liquidator())
        .with_input(calldata.clone())
        .with_max_fee_per_gas(gas_params.max_fee_per_gas)
        .with_max_priority_fee_per_gas(gas_params.max_priority_fee_per_gas);
    let gas_units = pipeline
        .provider
        .estimate_gas(&est_tx)
        .await
        .context("broadcast: eth_estimateGas failed")?;
    let gas_limit = gas_units.saturating_mul(BROADCAST_GAS_BUFFER_NUM) / BROADCAST_GAS_BUFFER_DEN;

    // 3. Claim a nonce locally — atomic, no race with a parallel
    //    opportunity in the same block.
    let nonce = harness.nonce_manager.next();

    // 4. Build + sign.
    let tx = builder
        .build_tx(
            calldata,
            nonce,
            gas_params.max_fee_per_gas,
            gas_params.max_priority_fee_per_gas,
            gas_limit,
        )
        .context("broadcast: build_tx failed")?;
    let raw = match builder.sign(tx).await {
        Ok(bytes) => bytes,
        Err(err) => {
            // Nonce consumed but no tx hit the wire — resync so the
            // counter doesn't leave a permanent gap.
            if let Err(resync_err) = harness
                .nonce_manager
                .resync(pipeline.provider.as_ref())
                .await
            {
                warn!(
                    error = %format!("{resync_err:#}"),
                    "nonce resync failed after sign error"
                );
            }
            return Err(anyhow::Error::new(err).context("broadcast: sign failed"));
        }
    };

    // 5. Submit.
    match harness.submitter.submit(raw).await {
        Ok(hash) => Ok(hash),
        Err(err) => {
            if matches!(err, SubmitError::RpcRejected(_)) {
                if let Err(resync_err) = harness
                    .nonce_manager
                    .resync(pipeline.provider.as_ref())
                    .await
                {
                    warn!(
                        error = %format!("{resync_err:#}"),
                        "nonce resync failed after RPC rejection"
                    );
                }
            }
            Err(anyhow::Error::new(err).context("broadcast: submit failed"))
        }
    }
}

/// Convert a `net_profit_wei` (debt-token smallest units, assumed
/// 18-decimal stablecoin for v0.1) to USD cents for the profit
/// histogram. Saturates on overflow so a corrupted upper-bound sample
/// never crashes the recorder. Placeholder until the per-token
/// decimals + price bridge lands (#148).
fn wei_to_usd_cents(wei: U256) -> u64 {
    // 1 stable unit (18 decimals) ≈ $1 → 100 cents. Divide by 1e16.
    let scale = U256::from(10u64).pow(U256::from(16u64));
    let cents = wei / scale;
    u64::try_from(cents).unwrap_or(u64::MAX)
}

/// Convert a gas cost originally denominated in the chain's native
/// token (BNB on BSC) into the debt-token's wei, using Chainlink
/// prices normalised to 1e8 and the two tokens' decimals.
///
/// ```text
/// native_wei = gas_units * max_fee_per_gas
/// debt_wei   = native_wei
///            * native_price_1e8 / debt_price_1e8
///            * 10^debt_decimals / 10^native_decimals
/// ```
///
/// All math is `U256` with `saturating_mul`; a zero or pathological
/// `debt_price_1e8` is caller-gated (see `process_opportunity`) so
/// the divisor here is never zero in practice. `decimals > 18` is
/// already rejected inside `ProfitInputs::from_opportunity`.
fn gas_cost_in_debt_wei(
    gas_units: u64,
    max_fee_per_gas: u128,
    native_price_1e8: u64,
    debt_price_1e8: u64,
    native_decimals: u8,
    debt_decimals: u8,
) -> U256 {
    if debt_price_1e8 == 0 {
        return U256::ZERO;
    }
    let native_wei = U256::from(gas_units).saturating_mul(U256::from(max_fee_per_gas));
    let usd_numerator = native_wei.saturating_mul(U256::from(native_price_1e8));
    // Apply the decimal delta between native and debt. Two separate
    // branches avoid `pow(0)` path-noise and keep the intent obvious.
    let usd_scaled = match debt_decimals.cmp(&native_decimals) {
        std::cmp::Ordering::Equal => usd_numerator,
        std::cmp::Ordering::Greater => {
            let diff = debt_decimals - native_decimals;
            usd_numerator.saturating_mul(U256::from(10u64).pow(U256::from(diff)))
        }
        std::cmp::Ordering::Less => {
            let diff = native_decimals - debt_decimals;
            usd_numerator / U256::from(10u64).pow(U256::from(diff))
        }
    };
    usd_scaled / U256::from(debt_price_1e8)
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod gas_cost_in_debt_wei_tests {
    use super::*;

    #[test]
    fn identical_tokens_and_prices_are_a_passthrough() {
        // 100k gas * 10 gwei = 1e15 native wei. Same token, same
        // price → same number of debt-wei.
        let got = gas_cost_in_debt_wei(
            100_000,
            10_000_000_000u128, // 10 gwei
            100_000_000,        // $1 @ 1e8
            100_000_000,        // $1 @ 1e8
            18,
            18,
        );
        assert_eq!(got, U256::from(1_000_000_000_000_000u128));
    }

    #[test]
    fn native_at_600_usd_debt_at_1_usd_scales_by_600() {
        // 100k gas * 10 gwei = 1e15 native wei.
        // BNB @ $600, USDT @ $1 → 6e17 USDT-wei (18 decimals each).
        let got = gas_cost_in_debt_wei(
            100_000,
            10_000_000_000u128,
            60_000_000_000u64, // $600 × 1e8
            100_000_000,
            18,
            18,
        );
        assert_eq!(got, U256::from(600_000_000_000_000_000u128));
    }

    #[test]
    fn debt_with_6_decimals_shrinks_by_1e12() {
        // 100k gas * 10 gwei = 1e15 native wei.
        // BNB @ $600, USDT @ $1, USDT is 6-dec → 6e5 USDT-wei.
        let got = gas_cost_in_debt_wei(
            100_000,
            10_000_000_000u128,
            60_000_000_000u64,
            100_000_000,
            18,
            6,
        );
        assert_eq!(got, U256::from(600_000u64));
    }

    #[test]
    fn zero_debt_price_returns_zero_not_panic() {
        let got = gas_cost_in_debt_wei(100_000, 10_000_000_000u128, 60_000_000_000u64, 0, 18, 18);
        assert_eq!(got, U256::ZERO);
    }
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

/// Drain pre-signed liquidations whose oracle trigger confirmed in
/// `block_hash` and run each through the executor's simulation gate
/// before the broadcast step (still non-goal per #299).
///
/// Fetches the block's confirmed tx-hash set via
/// `eth_getBlockByHash` (hashes-only payload), calls
/// [`PendingCache::drain_for_block`], and for each returned
/// [`charon_scanner::UnverifiedPreSigned`] rebuilds the liquidator
/// calldata via the adapter + builder, runs it through
/// [`Simulator::simulate`], and only hands the pre-sign a
/// [`SimulationVerdict::Ok`] proof token when the simulator returns
/// success. `verify(Ok)` unwraps the pre-sign into a full
/// `PreSignedLiquidation`; broadcast is explicitly out of scope
/// (signer + liquidator bridge tracked separately) so the drained
/// tx is logged and dropped.
///
/// Silently no-ops when the cache is `None` (mempool monitor is
/// disabled) or when the builder/simulator/params for a pre-sign
/// are unavailable — there is no way to honour the eth_call gate
/// without them, so the safer action is to re-insert-or-drop per
/// the cache's TTL and surface a warning.
///
/// Never panics. Every RPC/encode/sim failure is logged and the
/// drain loop continues with the next pre-sign; the block-scanner
/// path is independent and must not be blocked by mempool hiccups.
async fn drain_mempool_for_block(
    pipeline: &VenusPipeline,
    block_hash: B256,
    cache: Option<&PendingCache>,
    signer_key: Option<&secrecy::SecretString>,
) {
    let Some(cache) = cache else {
        return;
    };
    let chain = pipeline.chain_name.as_str();

    // Fetch the block with hashes-only payload. `Hashes` keeps the
    // response small — we only need the set membership check for
    // `drain_for_block`, not full transaction envelopes.
    let block = match pipeline
        .provider
        .get_block_by_hash(block_hash, BlockTransactionsKind::Hashes)
        .await
    {
        Ok(Some(b)) => b,
        Ok(None) => {
            warn!(%block_hash, "block not found when draining mempool cache");
            return;
        }
        Err(err) => {
            warn!(%block_hash, ?err, "get_block_by_hash failed when draining mempool cache");
            return;
        }
    };
    let confirmed: HashSet<B256> = block.transactions.hashes().collect();

    let drained = cache.drain_for_block(block_hash, &confirmed);
    if drained.is_empty() {
        return;
    }
    debug!(
        chain,
        %block_hash,
        drained = drained.len(),
        confirmed_tx_count = confirmed.len(),
        "mempool cache drained for block"
    );

    // Materialise the executor pair lazily — if the operator runs
    // scan-only (no signer) we cannot honour the eth_call gate, so we
    // drop drained pre-signs with a warning. Same contract as
    // `process_opportunity`: no signer → no simulation → no
    // broadcast-ready artefact.
    let Some((builder, sim)) = ensure_executor(pipeline, signer_key).await else {
        warn!(
            chain,
            drained = drained.len(),
            "pre-signs drained but no signer configured — dropping (sim gate cannot be honoured)"
        );
        return;
    };

    for presigned in drained {
        let borrower = presigned.borrower();
        let trigger = presigned.trigger_tx();
        let opp = presigned.opportunity().clone();

        // Rebuild calldata from the opportunity via the protocol
        // adapter + builder — the pre-sign's own `raw_tx` is the
        // signed envelope, which is intentionally unreachable without
        // a `SimulationVerdict`.
        let params = match pipeline.adapter.get_liquidation_params(&opp.position) {
            Ok(p) => p,
            Err(err) => {
                warn!(
                    chain,
                    %borrower,
                    error = ?err,
                    "failed to rebuild liquidation params for drained pre-sign"
                );
                continue;
            }
        };
        let calldata: Bytes = match builder.encode_calldata(&opp, &params) {
            Ok(c) => c,
            Err(err) => {
                warn!(
                    chain,
                    %borrower,
                    error = ?err,
                    "failed to encode calldata for drained pre-sign"
                );
                continue;
            }
        };
        match sim
            .simulate(pipeline.provider.as_ref(), calldata, SIMULATION_GAS_LIMIT)
            .await
        {
            Ok(()) => match presigned.verify(SimulationVerdict::approve()) {
                Ok(ready) => {
                    // Non-goal: eth_sendRawTransaction. The
                    // `PreSignedLiquidation` is fully verified and
                    // ready for the future broadcast call site; log
                    // loudly so operators running the monitor end-to-end
                    // can see the gate opening.
                    info!(
                        chain,
                        %borrower,
                        %trigger,
                        raw_tx_len = ready.raw_tx.len(),
                        "pre-sign simulated OK — ready for broadcast (broadcast wiring follow-up)"
                    );
                }
                Err((returned, verdict)) => {
                    warn!(
                        chain,
                        borrower = %returned.borrower(),
                        ?verdict,
                        "simulation verdict inconsistent with simulate outcome — dropping"
                    );
                }
            },
            Err(err) => {
                debug!(
                    chain,
                    %borrower,
                    %trigger,
                    error = ?err,
                    "pre-sign simulation reverted — dropping"
                );
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod parse_borrower_file_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Cargo runs each `#[test]` on a fresh thread but shares the
    /// process-level temp dir, so we synthesise a unique filename per
    /// test invocation to keep parallel runs isolated.
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn write_temp(name: &str, contents: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "charon-borrower-file-test-{}-{}-{}.txt",
            std::process::id(),
            n,
            name
        ));
        std::fs::write(&path, contents).expect("write temp borrower file");
        path
    }

    #[test]
    fn three_valid_addresses_parse() {
        let body = "\
0x0000000000000000000000000000000000000001
0x0000000000000000000000000000000000000002
0x0000000000000000000000000000000000000003
";
        let path = write_temp("three_valid", body);
        let out = parse_borrower_file(&path);
        assert_eq!(out.len(), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let body = "\
# header comment
0x0000000000000000000000000000000000000001

# another comment
0x0000000000000000000000000000000000000002

";
        let path = write_temp("comments", body);
        let out = parse_borrower_file(&path);
        assert_eq!(out.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn malformed_line_is_skipped_not_fatal() {
        let body = "\
0x0000000000000000000000000000000000000001
not-a-real-address
0x0000000000000000000000000000000000000002
";
        let path = write_temp("malformed", body);
        let out = parse_borrower_file(&path);
        assert_eq!(out.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_returns_empty_no_panic() {
        let path = std::env::temp_dir().join(format!(
            "charon-borrower-file-test-{}-missing-{}.txt",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        // Don't create it.
        let out = parse_borrower_file(&path);
        assert!(out.is_empty());
    }
}
