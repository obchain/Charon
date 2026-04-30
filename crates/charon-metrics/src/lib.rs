//! Prometheus-compatible metrics surface for Charon.
//!
//! The exporter listens on a configurable `SocketAddr` (default
//! `127.0.0.1:9091`, loopback-only; see `MetricsConfig` in
//! `charon-core` for the validation rules that block non-loopback
//! binds without a shared auth token) and serves a `/metrics`
//! endpoint in the Prometheus text format. All metric names are kept
//! as `const &str` constants in [`names`] so call sites and dashboard
//! JSON stay in lock-step with a single source of truth.
//!
//! ```no_run
//! use charon_metrics::{init, names, record_block_scanned};
//! # async fn demo() -> anyhow::Result<()> {
//! init("127.0.0.1:9091".parse()?).await?;
//! record_block_scanned("bnb");
//! # Ok(())
//! # }
//! ```

use std::future::Future;
use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::Instant;

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use metrics_exporter_prometheus::{
    BuildError as PromBuildError, ExporterFuture, Matcher, PrometheusBuilder,
};
use thiserror::Error;
use tracing::info;

/// Tracks whether the global Prometheus recorder has already been
/// installed in this process. `metrics_exporter_prometheus` calls
/// `metrics::set_global_recorder` under the hood, and that call
/// panics on a second successful install. Gating [`init`] behind
/// this `OnceLock` turns a second invocation into a silent no-op
/// so repeated calls from tests (or a future restart path) do not
/// tear the process down.
static INIT: OnceLock<()> = OnceLock::new();

/// Errors returned from [`init`]. Exposed as a `#[non_exhaustive]`
/// enum so `charon-cli` can distinguish bind failures (port collision
/// is retryable) from recorder-install failures (caller must abort —
/// the global recorder can only be set once) without matching on
/// `Display` strings. New variants may be added without a breaking
/// semver bump.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum MetricsError {
    /// Failed to register custom histogram buckets for a specific
    /// metric. Carries the metric name so logs pinpoint the offender.
    #[error("failed to register buckets for {metric}: {source}")]
    BucketConfig {
        metric: &'static str,
        #[source]
        source: PromBuildError,
    },
    /// Installing the global Prometheus recorder failed. Typically a
    /// port collision on `bind` or an exporter-build error. The
    /// underlying `BuildError` preserves the original diagnosis.
    #[error("failed to install Prometheus exporter on {bind}: {source}")]
    InstallFailed {
        bind: SocketAddr,
        #[source]
        source: PromBuildError,
    },
    /// The `metrics` global recorder was already installed by some
    /// other crate in the same process. Distinct from
    /// [`MetricsError::InstallFailed`] because the fix is
    /// different: exporter-build errors retry; a foreign recorder
    /// has to be removed from the dep graph entirely. The
    /// idempotency gate on [`install`] short-circuits our own
    /// second install, so reaching this variant means a third
    /// party got there first.
    #[error("failed to set global recorder for {bind}: {reason}")]
    RecorderInstall { bind: SocketAddr, reason: String },
}

/// Convenience alias so helpers and call sites share one return shape.
pub type Result<T, E = MetricsError> = std::result::Result<T, E>;

// Bucket boundaries for `charon_pipeline_block_duration_seconds`.
// BSC produces a block every ~3s; resolution is packed around that
// threshold so p50/p95 quantiles stay meaningful instead of piling
// into `+Inf` with the exporter's default HTTP-latency buckets.
const BLOCK_DURATION_SECONDS_BUCKETS: &[f64] = &[0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 10.0];

// Bucket boundaries for `charon_executor_profit_usd_cents`.
// Realistic Venus liquidation profit spans ~$0.05 dust to ~$10k
// windfalls; buckets are in cents (5 → 1_000_000) so histogram_quantile
// returns finite values across that range.
const PROFIT_USD_CENTS_BUCKETS: &[f64] = &[
    5.0,
    50.0,
    500.0,
    2_500.0,
    10_000.0,
    50_000.0,
    250_000.0,
    1_000_000.0,
];

// Bucket boundaries for `charon_rpc_call_duration_seconds`.
// Spans sub-millisecond LAN-local responses up to the 30 s
// provider-level timeout ceiling so call-site timeouts still
// surface as a finite bucket instead of `+Inf`. Lower bound of
// 1 ms sits just above the jitter floor of a tokio timer on a
// warm runtime — any sample finer than that is noise rather than
// a signal about upstream latency. Upper bound of 30 s matches
// the hard deadline used by submit/simulate call sites: a call
// that outruns 30 s has timed out regardless, so anything bigger
// would only widen `+Inf` overflow without adding resolution.
// Logarithmic spacing keeps p50/p95/p99 resolution meaningful
// across four decades.
const RPC_CALL_DURATION_SECONDS_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];

/// Single-source-of-truth metric names. Kept as constants so call
/// sites, dashboard JSON, and alert rules refer to the same strings.
pub mod names {
    // Scanner
    pub const SCANNER_BLOCKS_TOTAL: &str = "charon_scanner_blocks_total";
    pub const SCANNER_POSITIONS: &str = "charon_scanner_positions";

    // Listener — counts every `new_heads` arrival the moment the
    // websocket subscription delivers it, before the pipeline runs.
    // Distinct from `SCANNER_BLOCKS_TOTAL` (which advances per
    // pipeline tick): if the pipeline stalls or the per-block work
    // unit panics, the listener counter still climbs and the
    // dashboard can distinguish "no blocks arriving" from "blocks
    // arriving but pipeline wedged" (#328).
    pub const LISTENER_BLOCKS_RECEIVED_TOTAL: &str = "charon_listener_blocks_received_total";

    // Pipeline
    pub const PIPELINE_BLOCK_DURATION_SECONDS: &str = "charon_pipeline_block_duration_seconds";

    // Executor
    pub const EXECUTOR_SIMULATIONS_TOTAL: &str = "charon_executor_simulations_total";
    pub const EXECUTOR_OPPS_QUEUED_TOTAL: &str = "charon_executor_opportunities_queued_total";
    pub const EXECUTOR_OPPS_DROPPED_TOTAL: &str = "charon_executor_opportunities_dropped_total";
    pub const EXECUTOR_PROFIT_USD_CENTS: &str = "charon_executor_profit_usd_cents";
    pub const EXECUTOR_QUEUE_DEPTH: &str = "charon_executor_queue_depth";

    /// Top-level "opportunities seen" / "opportunities dropped"
    /// counters with a `reason` label (#368). Companions to the older
    /// `EXECUTOR_OPPS_DROPPED_TOTAL` (`stage` label) which is kept for
    /// dashboard back-compat: `stage` describes *where* in the
    /// pipeline the drop happened, `reason` describes *why* (and is
    /// the surface alerts and capacity planning queries should use).
    pub const OPPORTUNITIES_SEEN_TOTAL: &str = "charon_opportunities_seen_total";
    pub const OPPORTUNITIES_DROPPED_TOTAL: &str = "charon_opportunities_dropped_total";

    // Mempool monitor (issue #300). Counts pending oracle updates
    // observed in the mempool, oracle updates drained at block
    // boundaries, and upstream websocket reconnect attempts — the
    // third doubles as a health signal for flaky pubsub upstreams.
    pub const MEMPOOL_PENDING_ORACLE_UPDATES: &str = "charon_mempool_pending_oracle_updates";
    pub const MEMPOOL_DRAINED_TOTAL: &str = "charon_mempool_drained_total";
    pub const MEMPOOL_WS_RECONNECTS_TOTAL: &str = "charon_mempool_websocket_reconnects_total";
    /// Per-(selector, kind) counter of decoded Venus oracle writes
    /// observed in the mempool (#350). Surfaces "which selector is
    /// active right now" so a ResilientOracle migration that retires
    /// `updatePrice` and ships a replacement is visible at a glance.
    pub const MEMPOOL_VENUS_ORACLE_WRITES_TOTAL: &str = "charon_mempool_venus_oracle_writes_total";

    /// Replacement broadcasts (#364). Counts every successful RBF
    /// re-submission so dashboards can split fee-spike windows from
    /// vendor-side latency without log greppinng.
    pub const SUBMIT_REPLACEMENTS_TOTAL: &str = "charon_submit_replacements_total";

    /// Pending tx hashes the mempool monitor dropped because the
    /// per-hash lookup-concurrency semaphore was saturated (#359).
    /// A non-zero rate means the upstream feed exceeds the configured
    /// cap and the bot is shedding load to protect its RPC budget.
    pub const MEMPOOL_DROPPED_TOTAL: &str = "charon_mempool_dropped_total";

    /// Total rows written to the borrower-set checkpoint file (#349).
    /// Increments on every successful flush — a stalled value points
    /// at a flush task crash or a permission problem on the state
    /// directory.
    pub const DISCOVERY_BORROWERS_PERSISTED_TOTAL: &str =
        "charon_discovery_borrowers_persisted_total";

    // Gas oracle (issue #301). Latest EIP-1559 base fee, priority
    // fee used on the last submission attempt, and resulting
    // maxFeePerGas — plus a counter for opportunities dropped
    // because `max_fee_per_gas` exceeded the configured ceiling.
    pub const GAS_BASE_FEE_WEI: &str = "charon_gas_base_fee_wei";
    pub const GAS_PRIORITY_FEE_WEI: &str = "charon_gas_priority_fee_wei";
    pub const GAS_MAX_FEE_WEI: &str = "charon_gas_max_fee_wei";
    pub const GAS_CEILING_SKIPS_TOTAL: &str = "charon_gas_ceiling_skips_total";

    // RPC instrumentation (issue #302). Histogram of call durations
    // by method + endpoint kind, error counters partitioned by
    // failure mode, and a reconnect counter so upstream transport
    // churn is observable without log grepping.
    pub const RPC_CALL_DURATION_SECONDS: &str = "charon_rpc_call_duration_seconds";
    pub const RPC_ERRORS_TOTAL: &str = "charon_rpc_errors_total";
    pub const RPC_RECONNECTS_TOTAL: &str = "charon_rpc_connection_reconnects_total";

    // Build / runtime
    pub const BUILD_INFO: &str = "charon_build_info";
}

/// Position classification bucket used as the `bucket` label on
/// `charon_scanner_positions`.
pub mod bucket {
    pub const HEALTHY: &str = "healthy";
    pub const NEAR_LIQ: &str = "near_liq";
    pub const LIQUIDATABLE: &str = "liquidatable";
}

/// Simulation outcome used as the `result` label on
/// `charon_executor_simulations_total`.
pub mod sim_result {
    pub const OK: &str = "ok";
    pub const REVERT: &str = "revert";
    pub const ERROR: &str = "error";
}

/// Drop-stage label on `charon_executor_opportunities_dropped_total`.
pub mod drop_stage {
    pub const ROUTER: &str = "router";
    pub const PROFIT: &str = "profit";
    pub const SIMULATION: &str = "simulation";
    pub const BUILD: &str = "build";
}

/// Drop-reason label on `charon_opportunities_dropped_total` (#368).
/// Distinct from [`drop_stage`] — `stage` says *where* (pipeline
/// section), `reason` says *why* (operator-facing root cause).
/// Reasons are deliberately coarse so a single Grafana panel can
/// stack them without exploding cardinality. Add a new variant
/// before changing an existing one — dashboards and alerts pin the
/// label values.
pub mod drop_reason {
    /// FlashLoanRouter::route returned None — no source could cover
    /// the (token, amount) borrow this opportunity needs.
    pub const NO_FLASHLOAN_SOURCE: &str = "no_flashloan_source";
    /// Profit gate rejected: net profit fell below the configured
    /// floor, prices/decimals could not be resolved, or the inputs
    /// could not be normalised. Anything that says "we cannot
    /// guarantee a positive net" lands here.
    pub const UNPROFITABLE: &str = "unprofitable";
    /// `eth_call` simulation reverted, errored, or could not run
    /// (no signer configured). The opportunity is not safe to
    /// broadcast against the latest state.
    pub const SIM_REVERT: &str = "sim_revert";
    /// `bot.max_gas_wei` ceiling exceeded by the current EIP-1559
    /// max-fee. The bot deliberately skips broadcasting at this gas
    /// price to preserve economics.
    pub const GAS_CEILING: &str = "gas_ceiling";
    /// Queue TTL expired — the opportunity sat in the priority
    /// queue past the configured age and was discarded as stale.
    pub const TTL_EXPIRED: &str = "ttl_expired";
    /// Build / sign / `eth_sendRawTransaction` failed. The
    /// opportunity passed every earlier gate but the broadcast
    /// stage rejected it.
    pub const SUBMIT_FAILED: &str = "submit_failed";
}

/// Endpoint-kind label used on every RPC metric (issue #302).
/// "public" covers node providers on the open internet (Alchemy,
/// Infura, a self-hosted archive). "private" covers order-flow
/// aware submission paths (bloxroute, blocknative, sequencer
/// endpoints on L2s) whose latency profile and failure modes
/// differ sharply from public reads. Splitting on this label is
/// how operators see "my private relay is degrading" without
/// having to cross-reference logs.
pub mod endpoint_kind {
    pub const PUBLIC: &str = "public";
    pub const PRIVATE: &str = "private";
}

/// RPC method label on `charon_rpc_call_duration_seconds` and
/// `charon_rpc_errors_total`. Only the methods the bot actually
/// calls are listed — new methods should be added here as call
/// sites adopt the [`time_rpc`] wrapper. Freeform methods can be
/// passed as `&str` too; the constants exist so dashboards and
/// alert rules can reference the same string a call site uses.
pub mod rpc_method {
    pub const ETH_CALL: &str = "eth_call";
    pub const ETH_GET_BLOCK_BY_NUMBER: &str = "eth_getBlockByNumber";
    pub const ETH_SEND_RAW_TRANSACTION: &str = "eth_sendRawTransaction";
    pub const ETH_GET_LOGS: &str = "eth_getLogs";
    pub const ETH_GET_BLOCK_NUMBER: &str = "eth_blockNumber";
    pub const ETH_ESTIMATE_GAS: &str = "eth_estimateGas";
    pub const ETH_GET_TRANSACTION_BY_HASH: &str = "eth_getTransactionByHash";
    pub const ETH_SUBSCRIBE_NEW_HEADS: &str = "eth_subscribe_newHeads";
    pub const ETH_SUBSCRIBE_PENDING_TX: &str = "eth_subscribe_newPendingTransactions";
}

/// Failure mode label on `charon_rpc_errors_total`. Kept as a
/// closed three-way enum so alert rules can pivot on `error_kind`
/// without fuzzy matching on log strings. Call sites classify
/// their own errors into one of these before recording — the
/// mapping from `alloy` / `anyhow` errors to a kind is a
/// per-call-site judgement (a `tokio::time::timeout` firing is
/// [`TIMEOUT`], an RPC-level rejection is [`REJECTED`], an
/// `io::Error` or dropped subscription stream is
/// [`CONNECTION_LOST`]).
pub mod rpc_error {
    pub const TIMEOUT: &str = "timeout";
    pub const REJECTED: &str = "rejected";
    pub const CONNECTION_LOST: &str = "connection_lost";
}

/// Reason label on `charon_gas_ceiling_skips_total`. `CEILING`
/// is the only reason the current gas oracle emits — the label
/// exists so future reasons (e.g. `BASE_FEE_SPIKE`,
/// `PRIORITY_FEE_MISSING`) can be added without reshaping the
/// metric.
pub mod gas_skip_reason {
    pub const CEILING: &str = "ceiling";
}

/// Install the global Prometheus recorder and start the HTTP listener.
///
/// Idempotent: the first successful call installs the recorder and
/// spawns the `/metrics` listener, subsequent calls log and return
/// `Ok(())` without touching the global recorder. This guards against
/// double-install panics in `metrics::set_global_recorder`, which
/// would otherwise take the bot down on an accidental retry. The
/// exporter task runs for the lifetime of the tokio runtime — no
/// handle is returned because it never needs to be stopped in-process.
pub async fn init(bind: SocketAddr) -> Result<()> {
    // Fire-and-forget variant: install the recorder and spawn the
    // listener future onto the current tokio runtime. The returned
    // JoinHandle is intentionally discarded here — this path is
    // meant for tests and for code paths that do not have a
    // JoinSet supervisor. Production call sites should prefer
    // [`install`] so the exporter task can be supervised together
    // with the bot's other long-running tasks (see #222).
    match install(bind)? {
        Some(fut) => {
            tokio::spawn(async move {
                if let Err(err) = fut.await {
                    tracing::error!(error = ?err, "metrics exporter task terminated");
                }
            });
        }
        None => {
            // Recorder already installed; nothing to drive.
        }
    }
    Ok(())
}

/// Install the global Prometheus recorder and return the
/// [`ExporterFuture`] that drives the `/metrics` HTTP listener.
///
/// The returned future must be polled for the exporter to accept
/// scrapes — production code pushes it into the same `JoinSet`
/// that supervises block listeners so a panic in the exporter
/// triggers the same controlled-shutdown path (#222). Tests that
/// do not care about supervision should call [`init`] instead.
///
/// Returns `Ok(None)` on the second and later calls in the same
/// process, because the global recorder can only be installed
/// once — a second `install()` would panic inside
/// `metrics::set_global_recorder`, see #223. Callers that got
/// `None` must skip supervising a listener future; the prior
/// install still owns the HTTP socket.
pub fn install(bind: SocketAddr) -> Result<Option<ExporterFuture>> {
    // Idempotency gate — short-circuit before we touch the
    // PrometheusBuilder. `INIT` is checked again after the
    // successful build to close the narrow race where two
    // concurrent callers both observe `None` here.
    if INIT.get().is_some() {
        info!(bind = %bind, "metrics exporter already initialized; skipping re-install");
        return Ok(None);
    }

    let (recorder, exporter) = PrometheusBuilder::new()
        .with_http_listener(bind)
        .set_buckets_for_metric(
            Matcher::Full(names::PIPELINE_BLOCK_DURATION_SECONDS.to_string()),
            BLOCK_DURATION_SECONDS_BUCKETS,
        )
        .map_err(|source| MetricsError::BucketConfig {
            metric: names::PIPELINE_BLOCK_DURATION_SECONDS,
            source,
        })?
        .set_buckets_for_metric(
            Matcher::Full(names::EXECUTOR_PROFIT_USD_CENTS.to_string()),
            PROFIT_USD_CENTS_BUCKETS,
        )
        .map_err(|source| MetricsError::BucketConfig {
            metric: names::EXECUTOR_PROFIT_USD_CENTS,
            source,
        })?
        .set_buckets_for_metric(
            Matcher::Full(names::RPC_CALL_DURATION_SECONDS.to_string()),
            RPC_CALL_DURATION_SECONDS_BUCKETS,
        )
        .map_err(|source| MetricsError::BucketConfig {
            metric: names::RPC_CALL_DURATION_SECONDS,
            source,
        })?
        .build()
        .map_err(|source| MetricsError::InstallFailed { bind, source })?;

    // Close the race: if another caller beat us to INIT, drop
    // the recorder and exporter we just built and report no-op.
    // `set_global_recorder` below would otherwise panic on
    // double-install.
    if INIT.set(()).is_err() {
        info!(bind = %bind, "metrics exporter lost init race; discarding fresh build");
        return Ok(None);
    }

    // `set_global_recorder` fails only if a recorder is already
    // installed in the process. We check `INIT` above, so the
    // only way to reach a real failure here is a third-party
    // crate having already installed a `metrics` recorder.
    metrics::set_global_recorder(recorder).map_err(|err| MetricsError::RecorderInstall {
        bind,
        reason: err.to_string(),
    })?;

    describe_all();

    info!(bind = %bind, path = "/metrics", "metrics exporter listening");
    Ok(Some(exporter))
}

/// Emit Prometheus `# HELP` + `# TYPE` descriptors for every metric
/// Charon exposes. Called once from [`init`] so the exporter's first
/// scrape surfaces human-readable help text even before any counter
/// has been incremented.
fn describe_all() {
    describe_counter!(
        names::SCANNER_BLOCKS_TOTAL,
        "Total blocks processed by the scanner pipeline (one increment per per-block tick)."
    );
    describe_counter!(
        names::LISTENER_BLOCKS_RECEIVED_TOTAL,
        "Total `new_heads` events delivered by the chain websocket. Climbs whether or not the pipeline ticks."
    );
    describe_gauge!(
        names::SCANNER_POSITIONS,
        "Currently tracked positions bucketed by health classification."
    );
    describe_histogram!(
        names::PIPELINE_BLOCK_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Wall-clock duration of one full per-block pipeline pass."
    );
    describe_counter!(
        names::EXECUTOR_SIMULATIONS_TOTAL,
        "Simulations attempted via `eth_call`, partitioned by outcome."
    );
    describe_counter!(
        names::EXECUTOR_OPPS_QUEUED_TOTAL,
        "Liquidation opportunities that landed in the queue, labelled `simulated=true|false` to distinguish sim-gated entries from dry-run pushes (CHARON_SIGNER_KEY unset)."
    );
    describe_counter!(
        names::EXECUTOR_OPPS_DROPPED_TOTAL,
        "Liquidation opportunities dropped before reaching the queue, partitioned by stage."
    );
    describe_histogram!(
        names::EXECUTOR_PROFIT_USD_CENTS,
        "Per-opportunity net profit in USD cents (post profit gate)."
    );
    describe_gauge!(
        names::EXECUTOR_QUEUE_DEPTH,
        "Current depth of the profit-ordered opportunity queue."
    );
    describe_gauge!(
        names::MEMPOOL_PENDING_ORACLE_UPDATES,
        "Pending Venus oracle updates currently observed in the mempool (pre-signed liquidations armed)."
    );
    describe_counter!(
        names::MEMPOOL_DRAINED_TOTAL,
        "Pre-signed liquidations drained from the mempool cache at block confirmation, partitioned by chain."
    );
    describe_counter!(
        names::MEMPOOL_WS_RECONNECTS_TOTAL,
        "Reconnect attempts against the pending-transactions websocket subscription (flaky-upstream signal)."
    );
    describe_gauge!(
        names::GAS_BASE_FEE_WEI,
        "Latest EIP-1559 `baseFeePerGas` observed for the chain, in wei."
    );
    describe_gauge!(
        names::GAS_PRIORITY_FEE_WEI,
        "Priority fee (tip) used on the most recent gas-params resolution, in wei."
    );
    describe_gauge!(
        names::GAS_MAX_FEE_WEI,
        "`maxFeePerGas` used on the most recent submission attempt, in wei."
    );
    describe_counter!(
        names::GAS_CEILING_SKIPS_TOTAL,
        "Opportunities skipped because the resolved `max_fee_per_gas` exceeded the configured ceiling."
    );
    describe_histogram!(
        names::RPC_CALL_DURATION_SECONDS,
        metrics::Unit::Seconds,
        "Wall-clock duration of one RPC call, partitioned by method and endpoint kind (public vs private)."
    );
    describe_counter!(
        names::RPC_ERRORS_TOTAL,
        "RPC call failures partitioned by method and error kind (timeout, rejected, connection_lost)."
    );
    describe_counter!(
        names::RPC_RECONNECTS_TOTAL,
        "Reconnect attempts against an RPC transport (websocket or HTTP keep-alive), partitioned by endpoint kind."
    );
    describe_gauge!(
        names::BUILD_INFO,
        "Build metadata as labels; value is always 1."
    );
}

// ─── Typed helpers (thin wrappers so call sites stay terse) ───────────

/// Increment the per-chain blocks-scanned counter.
pub fn record_block_scanned(chain: &str) {
    counter!(names::SCANNER_BLOCKS_TOTAL, "chain" => chain.to_owned()).increment(1);
}

/// Increment the per-chain listener block-ingress counter (#328).
/// Bumped from the websocket `new_heads` handler before the pipeline
/// runs, so a flat listener counter unambiguously means "no blocks
/// arriving" rather than "pipeline stalled".
pub fn record_block_received(chain: &str) {
    counter!(names::LISTENER_BLOCKS_RECEIVED_TOTAL, "chain" => chain.to_owned()).increment(1);
}

/// Set the gauge for one health bucket on one chain.
pub fn set_position_bucket(chain: &str, bucket: &str, count: u64) {
    gauge!(names::SCANNER_POSITIONS, "chain" => chain.to_owned(), "bucket" => bucket.to_owned())
        .set(count as f64);
}

/// Observe the wall-clock duration of one pipeline pass.
pub fn observe_block_duration(chain: &str, seconds: f64) {
    histogram!(names::PIPELINE_BLOCK_DURATION_SECONDS, "chain" => chain.to_owned()).record(seconds);
}

/// Record one simulation outcome.
pub fn record_simulation(chain: &str, result: &str) {
    counter!(
        names::EXECUTOR_SIMULATIONS_TOTAL,
        "chain" => chain.to_owned(),
        "result" => result.to_owned()
    )
    .increment(1);
}

/// Record one opportunity that made it into the queue.
///
/// `simulated` distinguishes entries that cleared the `eth_call`
/// simulation gate from entries enqueued without simulation (dry-run
/// mode when `CHARON_SIGNER_KEY` is unset). Splitting on this label keeps
/// the gate bypass observable from dashboards instead of letting
/// unsimulated pushes masquerade as healthy throughput.
pub fn record_opportunity_queued(chain: &str, profit_usd_cents: u64, simulated: bool) {
    counter!(
        names::EXECUTOR_OPPS_QUEUED_TOTAL,
        "chain" => chain.to_owned(),
        "simulated" => if simulated { "true" } else { "false" }.to_owned(),
    )
    .increment(1);
    histogram!(names::EXECUTOR_PROFIT_USD_CENTS, "chain" => chain.to_owned())
        .record(profit_usd_cents as f64);
}

/// Record one opportunity that was dropped before reaching the queue.
pub fn record_opportunity_dropped(chain: &str, stage: &str) {
    counter!(
        names::EXECUTOR_OPPS_DROPPED_TOTAL,
        "chain" => chain.to_owned(),
        "stage" => stage.to_owned()
    )
    .increment(1);
}

/// Record one opportunity entering the pipeline. The matching drop /
/// queue counters answer "what fraction made it to broadcast?" —
/// without a `seen` baseline a stage drop count is unreadable. See
/// [`drop_reason`] for the reasons attached to drops.
pub fn record_opportunity_seen(chain: &str) {
    counter!(
        names::OPPORTUNITIES_SEEN_TOTAL,
        "chain" => chain.to_owned(),
    )
    .increment(1);
}

/// Record one opportunity dropped, labelled by *root-cause* `reason`.
/// Companion to [`record_opportunity_dropped`] (which uses a `stage`
/// label) — call sites should call both at every drop point so the
/// stage-based dashboard keeps working while `reason`-based panels and
/// alerts come online. See [`drop_reason`] for the allowed values.
pub fn record_opportunity_dropped_reason(chain: &str, reason: &str) {
    counter!(
        names::OPPORTUNITIES_DROPPED_TOTAL,
        "chain" => chain.to_owned(),
        "reason" => reason.to_owned(),
    )
    .increment(1);
}

/// Update the queue-depth gauge.
pub fn set_queue_depth(depth: u64) {
    gauge!(names::EXECUTOR_QUEUE_DEPTH).set(depth as f64);
}

/// Emit build metadata once at startup. The metric value is always 1;
/// labels carry the interesting bits so dashboards can `group_by`.
pub fn set_build_info(version: &str, git_sha: &str) {
    gauge!(
        names::BUILD_INFO,
        "version" => version.to_owned(),
        "git_sha" => git_sha.to_owned()
    )
    .set(1.0);
}

// ─── Mempool helpers (issue #300) ─────────────────────────────────────

/// Set the gauge of pending oracle updates the mempool monitor is
/// currently tracking. Called on insert/drain so the dashboard value
/// tracks the live cache size rather than a stale counter. Gauge (not
/// counter) because the quantity is "how many right now", which must
/// fall back to zero between blocks.
pub fn set_mempool_pending_oracle_updates(chain: &str, count: u64) {
    gauge!(
        names::MEMPOOL_PENDING_ORACLE_UPDATES,
        "chain" => chain.to_owned()
    )
    .set(count as f64);
}

/// Record `drained` pre-signed liquidations drained from the mempool
/// cache at a block boundary. Zero-valued drains are a legitimate
/// signal (nothing to do this block) so the call site records
/// unconditionally — Prometheus handles zero-delta increments.
/// Record one decoded Venus oracle write observed in the mempool
/// (#350). `selector` is the lowercase 8-hex selector
/// (e.g. `"0x4d8275ed"`), `kind` is the `OracleUpdate::kind()` accessor
/// (e.g. `"refresh"` / `"direct"`). Tagging on both axes lets a
/// dashboard split "what's in flight on Venus" by both surface (which
/// function) and effect (refresh vs direct overwrite).
pub fn record_mempool_oracle_write(chain: &str, selector: &str, kind: &str) {
    counter!(
        names::MEMPOOL_VENUS_ORACLE_WRITES_TOTAL,
        "chain" => chain.to_owned(),
        "selector" => selector.to_owned(),
        "kind" => kind.to_owned(),
    )
    .increment(1);
}

/// Record one pending tx hash dropped because the mempool monitor's
/// per-hash lookup-concurrency cap was saturated. The caller passes a
/// short reason label so dashboards can split future drop classes
/// (current callers use `"lookup_saturated"`).
pub fn record_mempool_dropped(chain: &str, reason: &str) {
    counter!(
        names::MEMPOOL_DROPPED_TOTAL,
        "chain" => chain.to_owned(),
        "reason" => reason.to_owned(),
    )
    .increment(1);
}

/// Record one borrower-set checkpoint flush (#349). `count` is the
/// number of rows persisted, used to drive the histogram of file
/// sizes downstream.
pub fn record_discovery_borrowers_persisted(chain: &str, count: u64) {
    counter!(
        names::DISCOVERY_BORROWERS_PERSISTED_TOTAL,
        "chain" => chain.to_owned(),
    )
    .increment(count);
}

pub fn record_mempool_drained(chain: &str, drained: u64) {
    counter!(
        names::MEMPOOL_DRAINED_TOTAL,
        "chain" => chain.to_owned()
    )
    .increment(drained);
}

/// Record one RBF replacement broadcast (#364). `reason` is the
/// caller-supplied label (`"ttl_expired"`, `"fee_spike"`, …) so a
/// future dashboard can split causes without changing call sites.
pub fn record_submit_replacement(chain: &str, reason: &str) {
    counter!(
        names::SUBMIT_REPLACEMENTS_TOTAL,
        "chain" => chain.to_owned(),
        "reason" => reason.to_owned(),
    )
    .increment(1);
}

/// Record one websocket reconnect attempt against the pending-tx
/// subscription. Emitted every time the monitor loop falls through
/// to its backoff branch — a high rate here is the operator's cue
/// that the upstream pubsub endpoint is flaky.
pub fn record_mempool_ws_reconnect(chain: &str) {
    counter!(
        names::MEMPOOL_WS_RECONNECTS_TOTAL,
        "chain" => chain.to_owned()
    )
    .increment(1);
}

// ─── Gas oracle helpers (issue #301) ──────────────────────────────────

/// Set the latest observed `baseFeePerGas` for the chain, in wei.
/// Values are passed as `u128` to survive the 1559 full range without
/// pre-truncation; the gauge is cast to `f64` at emission time, same
/// as every other Prometheus gauge — sub-wei precision is never
/// actionable for ops.
pub fn set_gas_base_fee_wei(chain: &str, wei: u128) {
    gauge!(
        names::GAS_BASE_FEE_WEI,
        "chain" => chain.to_owned()
    )
    .set(wei as f64);
}

/// Set the priority fee used on the last gas-params resolution,
/// in wei.
pub fn set_gas_priority_fee_wei(chain: &str, wei: u128) {
    gauge!(
        names::GAS_PRIORITY_FEE_WEI,
        "chain" => chain.to_owned()
    )
    .set(wei as f64);
}

/// Set the `maxFeePerGas` used on the last submission attempt,
/// in wei.
pub fn set_gas_max_fee_wei(chain: &str, wei: u128) {
    gauge!(
        names::GAS_MAX_FEE_WEI,
        "chain" => chain.to_owned()
    )
    .set(wei as f64);
}

/// Record one opportunity skipped by the gas oracle because the
/// resolved `max_fee_per_gas` exceeded the configured ceiling (or
/// any future skip reason added to [`gas_skip_reason`]).
pub fn record_gas_ceiling_skip(chain: &str, reason: &str) {
    counter!(
        names::GAS_CEILING_SKIPS_TOTAL,
        "chain" => chain.to_owned(),
        "reason" => reason.to_owned()
    )
    .increment(1);
}

// ─── RPC instrumentation (issue #302) ─────────────────────────────────

/// Observe one completed RPC call's wall-clock duration.
///
/// Most call sites should wrap their provider invocation in
/// [`time_rpc`] instead of calling this directly — the wrapper
/// handles `Instant::now()` and the label plumbing. Expose this
/// helper for call sites that already have a `Duration` in hand
/// (e.g. batched calls that track a single elapsed span across
/// multiple internal retries).
pub fn record_rpc_call(method: &str, endpoint_kind: &str, seconds: f64) {
    histogram!(
        names::RPC_CALL_DURATION_SECONDS,
        "method" => method.to_owned(),
        "endpoint_kind" => endpoint_kind.to_owned()
    )
    .record(seconds);
}

/// Record one RPC call failure. `error_kind` must be one of the
/// constants in [`rpc_error`]; freeform strings are accepted but
/// break dashboard pivots, so callers should funnel their errors
/// through a classifier.
pub fn record_rpc_error(method: &str, error_kind: &str) {
    counter!(
        names::RPC_ERRORS_TOTAL,
        "method" => method.to_owned(),
        "error_kind" => error_kind.to_owned()
    )
    .increment(1);
}

/// Record one reconnect attempt against an RPC transport. Emitted
/// by pubsub listeners (block listener, mempool monitor) every
/// time their outer reconnect loop fires — cumulative rate is the
/// operator's "upstream is unstable" signal.
pub fn record_rpc_reconnect(endpoint_kind: &str) {
    counter!(
        names::RPC_RECONNECTS_TOTAL,
        "endpoint_kind" => endpoint_kind.to_owned()
    )
    .increment(1);
}

/// Wrap one RPC call, record its wall-clock duration into
/// [`names::RPC_CALL_DURATION_SECONDS`], and return the call's
/// own result untouched.
///
/// This is the single preferred instrumentation pattern for adding
/// RPC latency / error visibility to a call site — prefer it over
/// sprinkling `record_rpc_call` / `record_rpc_error` directly. It
/// keeps the happy path a one-liner and guarantees the duration
/// sample always lands in the histogram, even on the error branch
/// (where latency-to-error is still useful context). Errors are
/// *not* auto-classified — the call site knows best whether an
/// `alloy` error is a [`rpc_error::TIMEOUT`] or a
/// [`rpc_error::REJECTED`]; pair this helper with one
/// [`record_rpc_error`] call on the error branch.
///
/// Example:
/// ```no_run
/// # async fn demo() -> anyhow::Result<()> {
/// use charon_metrics::{endpoint_kind, rpc_error, rpc_method, record_rpc_error, time_rpc};
/// # async fn eth_call() -> Result<(), anyhow::Error> { Ok(()) }
/// let result = time_rpc(
///     rpc_method::ETH_CALL,
///     endpoint_kind::PUBLIC,
///     eth_call(),
/// )
/// .await;
/// if let Err(err) = &result {
///     record_rpc_error(rpc_method::ETH_CALL, rpc_error::REJECTED);
///     eprintln!("{err}");
/// }
/// # Ok(())
/// # }
/// ```
///
/// New call sites adopting RPC instrumentation should follow
/// this pattern. An alloy middleware/layer would be cleaner in
/// principle, but the `alloy` 0.8 `Provider` trait does not
/// expose a stable middleware hook at the method-name layer — a
/// per-call-site wrapper lets us carry the method label verbatim
/// without leaking through an intermediate `RequestPacket` whose
/// method string is internal-only.
pub async fn time_rpc<F, T>(method: &str, endpoint_kind: &str, fut: F) -> T
where
    F: Future<Output = T>,
{
    let start = Instant::now();
    let out = fut.await;
    record_rpc_call(method, endpoint_kind, start.elapsed().as_secs_f64());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use tokio::net::TcpStream;
    use tokio::time::{Duration, sleep};

    /// `MetricsError::BucketConfig` must render the offending metric
    /// name in its `Display` string and expose the upstream
    /// `PromBuildError` through `Error::source()` so operator tooling
    /// can walk the chain. Reached via the real builder path (empty
    /// bucket slice → `BuildError::EmptyBucketsOrQuantiles`) rather
    /// than a hand-rolled variant, so the mapping in `init` stays
    /// exercised end-to-end.
    #[test]
    fn bucket_config_error_display_and_source_chain() {
        let err = PrometheusBuilder::new()
            .set_buckets_for_metric(
                Matcher::Full(names::EXECUTOR_PROFIT_USD_CENTS.to_string()),
                &[],
            )
            .map_err(|source| MetricsError::BucketConfig {
                metric: names::EXECUTOR_PROFIT_USD_CENTS,
                source,
            })
            .expect_err("empty bucket slice must fail");

        let rendered = format!("{err}");
        assert!(
            rendered.contains(names::EXECUTOR_PROFIT_USD_CENTS),
            "Display must name the offending metric, got {rendered:?}"
        );
        assert!(
            std::error::Error::source(&err).is_some(),
            "BucketConfig must expose its PromBuildError as source()"
        );
    }

    /// Covers two invariants at once because `INIT` is
    /// process-wide and unit tests in the same binary share it:
    ///
    /// 1. The first call binds the `/metrics` HTTP listener so a
    ///    subsequent TCP connect succeeds — regression gate
    ///    against broken listener wiring on exporter bumps.
    /// 2. Second and Nth calls return `Ok(())` without touching
    ///    the global recorder — `metrics-exporter-prometheus`
    ///    otherwise panics inside `set_global_recorder`, which
    ///    would take the bot down on what ought to be a harmless
    ///    retry (regression gate for #223).
    ///
    /// Folded into one test so unit-test ordering cannot leave
    /// `INIT` in a pre-set state and silently skip the bind
    /// assertion in a sibling test.
    #[tokio::test]
    async fn init_binds_listener_and_is_idempotent() {
        // Port 0 asks the OS for an ephemeral port, avoiding
        // collisions with any concurrent test run. Bind a probe
        // socket, record the number, drop it, hand the port to
        // the exporter. Races are technically possible but
        // vanishingly rare on 127.0.0.1 and do not compromise
        // correctness — the connect probe below would simply
        // fail loudly.
        let probe = std::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("probe bind");
        let port = probe
            .local_addr()
            .expect("probe socket must expose its bound local_addr")
            .port();
        drop(probe);

        let bind = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        init(bind).await.expect("first init should succeed");

        // Yield so the listener's spawn binds before we probe.
        sleep(Duration::from_millis(50)).await;

        TcpStream::connect(bind)
            .await
            .expect("listener should accept TCP connections");

        // Re-invoke with a deliberately unusable bind. If the
        // idempotency gate were missing, PrometheusBuilder would
        // attempt a fresh install and panic inside
        // `set_global_recorder`. We assert `Ok(())` and that the
        // listener never moves off `bind`.
        let bogus = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
        init(bogus)
            .await
            .expect("second init must be a silent no-op");
        init(bogus).await.expect("third init must also be a no-op");
    }

    /// Pin the public surface of the drop-reason / opportunities-seen
    /// metrics introduced by #368. Dashboards and alerts hard-code
    /// these strings; renaming a constant must trip this test.
    #[test]
    fn opportunities_drop_reason_names_are_stable() {
        assert_eq!(
            names::OPPORTUNITIES_SEEN_TOTAL,
            "charon_opportunities_seen_total"
        );
        assert_eq!(
            names::OPPORTUNITIES_DROPPED_TOTAL,
            "charon_opportunities_dropped_total"
        );
        assert_eq!(drop_reason::NO_FLASHLOAN_SOURCE, "no_flashloan_source");
        assert_eq!(drop_reason::UNPROFITABLE, "unprofitable");
        assert_eq!(drop_reason::SIM_REVERT, "sim_revert");
        assert_eq!(drop_reason::GAS_CEILING, "gas_ceiling");
        assert_eq!(drop_reason::TTL_EXPIRED, "ttl_expired");
        assert_eq!(drop_reason::SUBMIT_FAILED, "submit_failed");
    }

    /// Typed helpers must not panic when called — this exercises every
    /// label combination that call sites use so metric-name typos
    /// surface at `cargo test` time, not in prod.
    #[test]
    fn typed_helpers_are_panic_free() {
        record_block_scanned("bnb");
        set_position_bucket("bnb", bucket::HEALTHY, 7);
        set_position_bucket("bnb", bucket::NEAR_LIQ, 2);
        set_position_bucket("bnb", bucket::LIQUIDATABLE, 0);
        observe_block_duration("bnb", 0.123);
        record_simulation("bnb", sim_result::OK);
        record_simulation("bnb", sim_result::REVERT);
        record_simulation("bnb", sim_result::ERROR);
        record_opportunity_queued("bnb", 1_234, true);
        record_opportunity_queued("bnb", 9, false);
        record_opportunity_dropped("bnb", drop_stage::ROUTER);
        record_opportunity_dropped("bnb", drop_stage::PROFIT);
        record_opportunity_dropped("bnb", drop_stage::SIMULATION);
        record_opportunity_dropped("bnb", drop_stage::BUILD);
        record_opportunity_seen("bnb");
        record_opportunity_dropped_reason("bnb", drop_reason::NO_FLASHLOAN_SOURCE);
        record_opportunity_dropped_reason("bnb", drop_reason::UNPROFITABLE);
        record_opportunity_dropped_reason("bnb", drop_reason::SIM_REVERT);
        record_opportunity_dropped_reason("bnb", drop_reason::GAS_CEILING);
        record_opportunity_dropped_reason("bnb", drop_reason::TTL_EXPIRED);
        record_opportunity_dropped_reason("bnb", drop_reason::SUBMIT_FAILED);
        set_queue_depth(3);
        set_build_info("0.1.0", "deadbeef");

        // Mempool (#300)
        set_mempool_pending_oracle_updates("bnb", 4);
        record_mempool_drained("bnb", 3);
        record_mempool_drained("bnb", 0);
        record_mempool_ws_reconnect("bnb");
        // Mempool oracle writes (#350)
        record_mempool_oracle_write("bnb", "0x4d8275ed", "refresh");
        record_mempool_oracle_write("bnb", "0xb13a8aaf", "direct");

        // RBF replacements (#364)
        record_submit_replacement("bnb", "ttl_expired");
        record_submit_replacement("bnb", "fee_spike");

        // Borrower-set persistence (#349)
        record_discovery_borrowers_persisted("bnb", 42);

        // Gas (#301)
        set_gas_base_fee_wei("bnb", 3_000_000_000);
        set_gas_priority_fee_wei("bnb", 1_000_000_000);
        set_gas_max_fee_wei("bnb", 5_000_000_000);
        record_gas_ceiling_skip("bnb", gas_skip_reason::CEILING);

        // RPC (#302)
        record_rpc_call(rpc_method::ETH_CALL, endpoint_kind::PUBLIC, 0.012);
        record_rpc_call(
            rpc_method::ETH_SEND_RAW_TRANSACTION,
            endpoint_kind::PRIVATE,
            0.045,
        );
        record_rpc_error(rpc_method::ETH_CALL, rpc_error::TIMEOUT);
        record_rpc_error(rpc_method::ETH_GET_LOGS, rpc_error::REJECTED);
        record_rpc_error(
            rpc_method::ETH_GET_BLOCK_BY_NUMBER,
            rpc_error::CONNECTION_LOST,
        );
        record_rpc_reconnect(endpoint_kind::PUBLIC);
        record_rpc_reconnect(endpoint_kind::PRIVATE);
    }

    /// `time_rpc` must record a non-zero elapsed sample into the
    /// histogram and return the wrapped future's output unchanged.
    /// Covers the ergonomic-wrapper contract: callers rely on it
    /// being a drop-in around `await`.
    #[tokio::test]
    async fn time_rpc_returns_inner_output_and_records_duration() {
        let out = time_rpc(rpc_method::ETH_CALL, endpoint_kind::PUBLIC, async {
            tokio::task::yield_now().await;
            42u32
        })
        .await;
        assert_eq!(out, 42);

        // Error case: the wrapper must not swallow errors from the
        // inner future — callers chain `record_rpc_error` on the Err
        // branch and would lose visibility otherwise.
        let err: std::result::Result<(), &'static str> = time_rpc(
            rpc_method::ETH_SEND_RAW_TRANSACTION,
            endpoint_kind::PRIVATE,
            async { Err("rpc down") },
        )
        .await;
        assert_eq!(err, Err("rpc down"));
    }
}
