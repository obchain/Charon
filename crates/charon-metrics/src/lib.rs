//! Prometheus-compatible metrics surface for Charon.
//!
//! The exporter listens on a configurable `SocketAddr` (default
//! `0.0.0.0:9091`) and serves a `/metrics` endpoint in the Prometheus
//! text format. All metric names are kept as `const &str` constants in
//! [`names`] so call sites and dashboard JSON stay in lock-step with a
//! single source of truth.
//!
//! ```no_run
//! use charon_metrics::{init, names, record_block_scanned};
//! # async fn demo() -> anyhow::Result<()> {
//! init("0.0.0.0:9091".parse()?).await?;
//! record_block_scanned("bnb");
//! # Ok(())
//! # }
//! ```

use std::net::SocketAddr;

use anyhow::{Context, Result};
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use metrics_exporter_prometheus::PrometheusBuilder;
use tracing::info;

/// Single-source-of-truth metric names. Kept as constants so call
/// sites, dashboard JSON, and alert rules refer to the same strings.
pub mod names {
    // Scanner
    pub const SCANNER_BLOCKS_TOTAL: &str = "charon_scanner_blocks_total";
    pub const SCANNER_POSITIONS: &str = "charon_scanner_positions";

    // Pipeline
    pub const PIPELINE_BLOCK_DURATION_SECONDS: &str = "charon_pipeline_block_duration_seconds";

    // Executor
    pub const EXECUTOR_SIMULATIONS_TOTAL: &str = "charon_executor_simulations_total";
    pub const EXECUTOR_OPPS_QUEUED_TOTAL: &str = "charon_executor_opportunities_queued_total";
    pub const EXECUTOR_OPPS_DROPPED_TOTAL: &str = "charon_executor_opportunities_dropped_total";
    pub const EXECUTOR_PROFIT_USD_CENTS: &str = "charon_executor_profit_usd_cents";
    pub const EXECUTOR_QUEUE_DEPTH: &str = "charon_executor_queue_depth";

    // Build / runtime
    pub const BUILD_INFO: &str = "charon_build_info";
    pub const RUN_MODE: &str = "charon_run_mode";
}

/// Run-mode label value on `charon_run_mode`. `FULL` means flashloan +
/// liquidator are both configured for the active chain and the
/// opportunity-processing arm is live; `READ_ONLY` means one or both
/// are intentionally absent (e.g. testnet) and the scanner + metrics
/// stay up for observability but no liquidation can execute.
pub mod run_mode {
    pub const FULL: &str = "full";
    pub const READ_ONLY: &str = "read_only";
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

/// Install the global Prometheus recorder and start the HTTP listener.
///
/// Safe to call at most once per process; subsequent calls return an
/// error because the global recorder can only be set once. The exporter
/// task runs for the lifetime of the tokio runtime — no handle is
/// returned because it never needs to be stopped in-process.
pub async fn init(bind: SocketAddr) -> Result<()> {
    PrometheusBuilder::new()
        .with_http_listener(bind)
        .install()
        .with_context(|| format!("failed to install Prometheus exporter on {bind}"))?;

    describe_all();

    info!(bind = %bind, path = "/metrics", "metrics exporter listening");
    Ok(())
}

/// Emit Prometheus `# HELP` + `# TYPE` descriptors for every metric
/// Charon exposes. Called once from [`init`] so the exporter's first
/// scrape surfaces human-readable help text even before any counter
/// has been incremented.
fn describe_all() {
    describe_counter!(
        names::SCANNER_BLOCKS_TOTAL,
        "Total blocks drained from chain listeners."
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
        "Liquidation opportunities that passed every gate and landed in the queue."
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
        names::BUILD_INFO,
        "Build metadata as labels; value is always 1."
    );
    describe_gauge!(
        names::RUN_MODE,
        "Bot run mode as a `mode` label; value is 1 for the active mode and 0 for the inactive one. Lets dashboards colour `charon_scanner_positions{bucket=\"liquidatable\"}` growth as expected (read-only demos) vs alarming (full mode)."
    );
}

// ─── Typed helpers (thin wrappers so call sites stay terse) ───────────

/// Increment the per-chain blocks-scanned counter.
pub fn record_block_scanned(chain: &str) {
    counter!(names::SCANNER_BLOCKS_TOTAL, "chain" => chain.to_owned()).increment(1);
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
pub fn record_opportunity_queued(chain: &str, profit_usd_cents: u64) {
    counter!(names::EXECUTOR_OPPS_QUEUED_TOTAL, "chain" => chain.to_owned()).increment(1);
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

/// Publish the bot's run mode. Sets `charon_run_mode{mode=<active>}`
/// to 1 and the other label value to 0 so dashboards can select on
/// either series without ambiguity. Call once at startup after
/// `Config::validate` has decided whether the profile is full or
/// read-only.
pub fn set_run_mode(active: &str) {
    for m in [run_mode::FULL, run_mode::READ_ONLY] {
        let value = if m == active { 1.0 } else { 0.0 };
        gauge!(names::RUN_MODE, "mode" => m.to_owned()).set(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddrV4};
    use tokio::net::TcpStream;
    use tokio::time::{Duration, sleep};

    /// Smoke-test: `init` must bind the HTTP listener so a subsequent
    /// TCP connect to `/metrics` succeeds. A failed listener bind is
    /// the single most common regression when swapping exporter
    /// versions; this catches it without asserting on the text body.
    #[tokio::test]
    async fn init_binds_prometheus_http_listener() {
        // Port 0 asks the OS for an ephemeral port, avoiding collisions
        // with any concurrent test run. We then need to know which port
        // was picked so we can connect back — bind a probe socket first
        // just to reserve a port number, drop it, hand the number to
        // the exporter. Races are technically possible but vanishingly
        // rare in practice on `127.0.0.1`.
        let probe = std::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
            .expect("probe bind");
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        let bind = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        init(bind).await.expect("init should succeed");

        // Small yield so the listener's spawn has a chance to bind
        // before the connect probe fires.
        sleep(Duration::from_millis(50)).await;

        TcpStream::connect(bind)
            .await
            .expect("listener should accept TCP connections");
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
        record_opportunity_queued("bnb", 1_234);
        record_opportunity_dropped("bnb", drop_stage::ROUTER);
        record_opportunity_dropped("bnb", drop_stage::PROFIT);
        record_opportunity_dropped("bnb", drop_stage::SIMULATION);
        record_opportunity_dropped("bnb", drop_stage::BUILD);
        set_queue_depth(3);
        set_build_info("0.1.0", "deadbeef");
        set_run_mode(run_mode::FULL);
        set_run_mode(run_mode::READ_ONLY);
    }
}
