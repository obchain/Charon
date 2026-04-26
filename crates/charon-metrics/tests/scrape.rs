#![allow(clippy::unwrap_used, clippy::expect_used)]

//! End-to-end scrape test for the Prometheus exporter.
//!
//! Lives in `tests/` rather than `src/lib.rs`'s `#[cfg(test)]` module
//! so it runs as its own integration-test binary with a fresh process
//! — the exporter installs a global recorder, and a second install in
//! the same process is a silent no-op (`charon-metrics` #223), so the
//! integration-test binary must not share process state with the unit
//! tests.
//!
//! Regression gate for #224: it was previously possible to ship a
//! broken exporter (metric name typo, missing `describe_*` call,
//! listener not bound) without any test catching it — this test
//! scrapes `/metrics` with a raw HTTP client and asserts the
//! Prometheus text-format response contains the expected helpers.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::Duration;

use charon_metrics::{init, names, record_block_scanned};

/// Fixed loopback port picked to avoid collision with the default
/// exporter port (`9091`) and with common dev services. If the port
/// is genuinely in use on a contributor's box, the test fails loudly
/// with a bind error rather than silently passing — acceptable
/// tradeoff for not plumbing the bound addr out of the exporter lib.
const TEST_PORT: u16 = 19_091;

/// Scrape the exporter after a counter has been incremented and
/// verify the Prometheus text-format body contains both `# HELP`
/// metadata and the metric line.
///
/// We deliberately avoid pulling `reqwest` just to fetch one URL —
/// the text format is plain HTTP/1.1 so a raw-TCP request keeps the
/// dev-dep surface small and sidesteps TLS/async runtime questions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scrape_returns_valid_prometheus_text() {
    let bind = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, TEST_PORT));

    init(bind).await.expect("exporter init must succeed");

    // Small yield so the listener's spawn has a chance to bind
    // before we connect. Without this, a fast test machine can race
    // the exporter's `tokio::spawn` and see `connection refused`.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Touch a counter and a gauge so the snapshot is non-empty. The
    // counter name must round-trip through the exporter's dedup +
    // label-sorting, so asserting on its presence validates the
    // whole pipeline — constants, descriptor registration, encoder.
    record_block_scanned("bnb");
    charon_metrics::set_position_bucket("bnb", charon_metrics::bucket::HEALTHY, 1);

    // Mempool (#300), gas (#301), and RPC (#302) series — record
    // at least one sample per series so the scrape surfaces the
    // metric name and its expected label set. Each of these is the
    // end-to-end regression gate for a typo in the name constant
    // or a missing `describe_*` registration.
    charon_metrics::set_mempool_pending_oracle_updates("bnb", 2);
    charon_metrics::record_mempool_drained("bnb", 5);
    charon_metrics::record_mempool_ws_reconnect("bnb");

    charon_metrics::set_gas_base_fee_wei("bnb", 3_000_000_000);
    charon_metrics::set_gas_priority_fee_wei("bnb", 1_000_000_000);
    charon_metrics::set_gas_max_fee_wei("bnb", 5_000_000_000);
    charon_metrics::record_gas_ceiling_skip("bnb", charon_metrics::gas_skip_reason::CEILING);

    charon_metrics::record_rpc_call(
        charon_metrics::rpc_method::ETH_CALL,
        charon_metrics::endpoint_kind::PUBLIC,
        0.012,
    );
    charon_metrics::record_rpc_error(
        charon_metrics::rpc_method::ETH_CALL,
        charon_metrics::rpc_error::TIMEOUT,
    );
    charon_metrics::record_rpc_reconnect(charon_metrics::endpoint_kind::PRIVATE);

    // Give the recorder a beat to flush the new sample into the
    // renderer's internal state.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let body = tokio::task::spawn_blocking(move || scrape(bind))
        .await
        .expect("scrape task must not panic")
        .expect("scrape must succeed");

    // `# HELP` lines are emitted by `describe_*` calls — their
    // presence proves `describe_all()` ran.
    assert!(
        body.contains("# HELP"),
        "scrape body missing `# HELP` metadata; got:\n{body}"
    );
    assert!(
        body.contains("# TYPE"),
        "scrape body missing `# TYPE` metadata; got:\n{body}"
    );

    // Metric-name constants must flow end-to-end. Assert on the raw
    // strings from `names` so a typo in the constant or a call-site
    // drift surfaces here rather than in a silent Grafana panel.
    assert!(
        body.contains(names::SCANNER_BLOCKS_TOTAL),
        "scrape body missing `{}`; got:\n{body}",
        names::SCANNER_BLOCKS_TOTAL,
    );
    assert!(
        body.contains(names::SCANNER_POSITIONS),
        "scrape body missing `{}`; got:\n{body}",
        names::SCANNER_POSITIONS,
    );

    // Label round-trip: the `chain="bnb"` label must show up on the
    // counter line. Guards against a regression where label keys are
    // dropped by the exporter's relabeling / matcher config.
    assert!(
        body.contains("chain=\"bnb\""),
        "scrape body missing expected `chain=\"bnb\"` label; got:\n{body}"
    );

    // Each new series (#300 / #301 / #302) must appear by name so
    // typos in the constants or a missing describe_* call surface
    // here. Label values exercised above are checked for at least
    // one representative sample per metric family.
    for name in [
        names::MEMPOOL_PENDING_ORACLE_UPDATES,
        names::MEMPOOL_DRAINED_TOTAL,
        names::MEMPOOL_WS_RECONNECTS_TOTAL,
        names::GAS_BASE_FEE_WEI,
        names::GAS_PRIORITY_FEE_WEI,
        names::GAS_MAX_FEE_WEI,
        names::GAS_CEILING_SKIPS_TOTAL,
        names::RPC_CALL_DURATION_SECONDS,
        names::RPC_ERRORS_TOTAL,
        names::RPC_RECONNECTS_TOTAL,
    ] {
        assert!(
            body.contains(name),
            "scrape body missing `{name}`; got:\n{body}"
        );
    }

    // Label-set regression gates: one representative pair per
    // metric family. A drift in label naming (e.g. renaming
    // `endpoint_kind` to `kind`) otherwise silently breaks every
    // dashboard that pivots on the label.
    assert!(
        body.contains("reason=\"ceiling\""),
        "gas ceiling-skip counter missing `reason` label; got:\n{body}"
    );
    assert!(
        body.contains("method=\"eth_call\""),
        "rpc duration histogram missing `method` label; got:\n{body}"
    );
    assert!(
        body.contains("endpoint_kind=\"public\""),
        "rpc duration histogram missing `endpoint_kind` label; got:\n{body}"
    );
    assert!(
        body.contains("error_kind=\"timeout\""),
        "rpc error counter missing `error_kind` label; got:\n{body}"
    );
    assert!(
        body.contains("endpoint_kind=\"private\""),
        "rpc reconnect counter missing `endpoint_kind=\"private\"`; got:\n{body}"
    );
}

/// Minimal HTTP/1.1 GET over raw TCP. Returns the response body
/// (everything after the first `\r\n\r\n`). Anything non-200 or a
/// malformed response is surfaced as an `Err`.
fn scrape(bind: SocketAddr) -> std::io::Result<String> {
    let mut stream = TcpStream::connect_timeout(&bind, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let request = format!(
        "GET /metrics HTTP/1.1\r\nHost: {bind}\r\nConnection: close\r\nUser-Agent: charon-test\r\n\r\n",
    );
    stream.write_all(request.as_bytes())?;

    let mut raw = Vec::with_capacity(8 * 1024);
    stream.read_to_end(&mut raw)?;

    let text = String::from_utf8_lossy(&raw).into_owned();
    let status_line = text.lines().next().unwrap_or("");
    if !status_line.starts_with("HTTP/1.1 200") && !status_line.starts_with("HTTP/1.0 200") {
        return Err(std::io::Error::other(format!(
            "unexpected response status: {status_line}"
        )));
    }

    let body_start = text
        .find("\r\n\r\n")
        .and_then(|i| i.checked_add(4))
        .unwrap_or(0);
    Ok(text[body_start..].to_owned())
}
