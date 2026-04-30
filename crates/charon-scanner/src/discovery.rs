//! Borrower auto-discovery — issue #329.
//!
//! Scans Venus vToken contracts for `Borrow(address borrower, uint, uint, uint)`
//! events to populate a persistent borrower set without requiring operator
//! `--borrower` seeding. Backfills a configurable history window on startup,
//! then tails new logs over WebSocket.
//!
//! The borrower address is the first 32 bytes of `data` (Venus does not
//! index `borrower` on the Compound-fork `Borrow` event), so decoding is a
//! trivial slice + `Address::from_slice` rather than a topic lookup.

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256, FixedBytes, b256};
use alloy::providers::{Provider, RootProvider};
use alloy::pubsub::PubSubFrontend;
use alloy::rpc::types::eth::{Filter, Log};
use anyhow::{Context, Result};
use dashmap::DashMap;
use futures_util::StreamExt;
use rand::Rng as _;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use charon_core::DiscoveryConfig;

use crate::token_meta::is_transient;

/// `keccak256("Borrow(address,uint256,uint256,uint256)")` — the Compound /
/// Venus `Borrow` event signature. Unindexed `borrower` lives in `data[0..32]`.
pub const BORROW_TOPIC0: B256 =
    b256!("13ed6866d4e1ee6da46f845c46d7e54120883d75c5ea9a2dacc1c4ca8984ab80");

/// Default backfill window — last 7 days at BSC's ~3s block time ≈ 200_000 blocks.
pub const DEFAULT_BACKFILL_BLOCKS: u64 = 200_000;

/// Maximum span per `eth_getLogs` chunk. Free-tier BSC RPCs cap range queries
/// at 10_000 blocks and reject anything larger with `code: 35`.
pub const MAX_LOG_CHUNK_BLOCKS: u64 = 9_500;

/// Live-tail mpsc capacity — bounded so the heartbeat channel cannot grow
/// without limit. The producer uses `try_send` and silently drops on a full
/// channel: the canonical sink for discovered borrowers is the
/// [`BorrowerSet`] itself; this channel only feeds an operator-visible
/// debug-log heartbeat, so dropping a notification has no correctness
/// impact and we explicitly avoid back-pressuring the upstream WS
/// subscription.
pub const DISCOVERY_CHANNEL_CAPACITY: usize = 1_024;

// Backfill pacing / retry knobs are now sourced from
// `charon_core::DiscoveryConfig` (see `[chain.<name>.discovery]` in
// `config/default.toml`). The previous module-level consts have moved
// to `DiscoveryConfig::default()` so the same values keep applying when
// the operator omits the `[discovery]` block.

/// Reconnect-backoff ceiling for [`run_discovery_live_with_reconnect`].
/// Mirrors `BlockListener::run`'s 30 s cap so a long upstream outage
/// stops hammering the WS endpoint.
const DISCOVERY_RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Per-borrower book-keeping carried alongside the address set.
#[derive(Debug, Clone, Copy)]
pub struct BorrowerInfo {
    /// Last block at which this borrower emitted a `Borrow` event.
    pub last_seen_block: u64,
}

/// Append-only borrower set populated by both the backfill and the live tail.
///
/// Wrapped in `Arc<DashMap>` so the subscription task and the scanner pipeline
/// can both touch it without coordinating locks.
#[derive(Debug, Default, Clone)]
pub struct BorrowerSet {
    inner: Arc<DashMap<Address, BorrowerInfo>>,
}

impl BorrowerSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or refresh an entry. Returns `true` when the address was new.
    pub fn upsert(&self, addr: Address, block: u64) -> bool {
        let mut inserted = false;
        self.inner
            .entry(addr)
            .and_modify(|existing| {
                if block > existing.last_seen_block {
                    existing.last_seen_block = block;
                }
            })
            .or_insert_with(|| {
                inserted = true;
                BorrowerInfo {
                    last_seen_block: block,
                }
            });
        inserted
    }

    pub fn snapshot(&self) -> Vec<Address> {
        self.inner.iter().map(|kv| *kv.key()).collect()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn contains(&self, addr: &Address) -> bool {
        self.inner.contains_key(addr)
    }
}

/// Decode the `borrower` address from a Venus `Borrow` log.
///
/// Returns `None` if `data` is shorter than 32 bytes — never happens for a
/// well-formed event, but we'd rather skip a malformed log than panic the
/// listener task.
pub fn decode_borrow_borrower(log: &Log) -> Option<Address> {
    let data = log.data().data.as_ref();
    if data.len() < 32 {
        return None;
    }
    Some(Address::from_slice(&data[12..32]))
}

/// `eth_getLogs` wrapper with bounded exponential-backoff retry on
/// transient upstream failures (5xx, `-32603` "temporary internal
/// error", rate limits, transport drops). Permanent errors — range
/// too large, malformed filter — surface on the first attempt so the
/// operator notices instead of waiting through retries.
async fn get_logs_with_retry(
    provider: &RootProvider<PubSubFrontend>,
    filter: &Filter,
    from_block: u64,
    to_block: u64,
    cfg: &DiscoveryConfig,
) -> Result<Vec<Log>, alloy::transports::RpcError<alloy::transports::TransportErrorKind>> {
    let max_attempts = cfg.chunk_max_attempts.max(1);
    let initial = Duration::from_millis(cfg.chunk_initial_backoff_ms);
    let cap = Duration::from_millis(cfg.chunk_max_backoff_ms);
    let mut backoff = initial;
    for attempt in 1..=max_attempts {
        match provider.get_logs(filter).await {
            Ok(logs) => return Ok(logs),
            Err(err) => {
                if !is_transient(&err) || attempt == max_attempts {
                    return Err(err);
                }
                warn!(
                    from_block,
                    to_block,
                    attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    error = ?err,
                    "discovery backfill: transient eth_getLogs failure — retrying",
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2).min(cap);
            }
        }
    }
    unreachable!("get_logs_with_retry: loop must return on final attempt")
}

/// Backfill the borrower set by paging `eth_getLogs` across the requested
/// block range in `MAX_LOG_CHUNK_BLOCKS`-sized windows. Returns the number
/// of distinct addresses observed.
///
/// All vTokens are filtered in a single `Filter::address(vec![...])` per
/// chunk so the upstream gets one request per chunk rather than one per
/// market. Each chunk is wrapped in [`get_logs_with_retry`] so a single
/// transient 5xx no longer leaves the borrower set empty for the run.
pub async fn backfill_borrowers(
    provider: &RootProvider<PubSubFrontend>,
    vtokens: Vec<Address>,
    set: &BorrowerSet,
    from_block: u64,
    to_block: u64,
) -> Result<usize> {
    backfill_borrowers_with_config(
        provider,
        vtokens,
        set,
        from_block,
        to_block,
        &DiscoveryConfig::default(),
    )
    .await
}

/// Same as [`backfill_borrowers`] but with an explicit
/// [`DiscoveryConfig`] driving chunk size, pacing, and retry budget.
/// New code should call this directly so operators can tune cold-start
/// behaviour from `[chain.<name>.discovery]` (#365); the legacy
/// constants-only entry point is kept as a thin wrapper.
pub async fn backfill_borrowers_with_config(
    provider: &RootProvider<PubSubFrontend>,
    vtokens: Vec<Address>,
    set: &BorrowerSet,
    from_block: u64,
    to_block: u64,
    cfg: &DiscoveryConfig,
) -> Result<usize> {
    if vtokens.is_empty() {
        return Ok(0);
    }
    if from_block > to_block {
        return Ok(0);
    }

    let chunk_span = cfg.log_chunk_blocks.max(1);
    let pacing = Duration::from_millis(cfg.inter_chunk_pacing_ms);
    let mut new_count: usize = 0;
    let topic = FixedBytes::<32>::from(BORROW_TOPIC0.0);
    let mut cursor = from_block;
    while cursor <= to_block {
        // Saturating arithmetic — the chunk window is bounded by
        // `chunk_span` and clamped to `to_block`, so an overflow here
        // would already mean the input range was pathological. Use
        // `saturating_*` to satisfy the workspace's
        // `arithmetic_side_effects` lint without changing behaviour.
        let chunk_end = cursor
            .saturating_add(chunk_span)
            .saturating_sub(1)
            .min(to_block);
        let filter = Filter::new()
            .from_block(cursor)
            .to_block(chunk_end)
            .address(vtokens.clone())
            .event_signature(topic);
        let logs = get_logs_with_retry(provider, &filter, cursor, chunk_end, cfg)
            .await
            .with_context(|| format!("eth_getLogs {}..{}", cursor, chunk_end))?;
        debug!(
            from = cursor,
            to = chunk_end,
            log_count = logs.len(),
            "discovery backfill chunk"
        );
        for log in &logs {
            if let Some(borrower) = decode_borrow_borrower(log) {
                let block = log.block_number.unwrap_or(chunk_end);
                if set.upsert(borrower, block) {
                    new_count = new_count.saturating_add(1);
                }
            }
        }
        // saturating_add — at u64::MAX we want the loop to terminate
        // rather than wrap to 0 and re-issue the same chunk forever.
        cursor = chunk_end.saturating_add(1);
        if chunk_end == u64::MAX {
            break;
        }
        // Inter-chunk pacing — only sleep when there is another chunk
        // to fetch, so a single-chunk backfill stays instant. Skip the
        // sleep when pacing is zero (paid RPCs).
        if cursor <= to_block && !pacing.is_zero() {
            tokio::time::sleep(pacing).await;
        }
    }
    info!(
        from_block,
        to_block,
        discovered = new_count,
        cumulative = set.len(),
        "discovery backfill complete"
    );
    Ok(new_count)
}

/// Subscribe to the live `Borrow` log stream for the given vTokens and
/// forward decoded borrower addresses to `sink`. The function returns when
/// the upstream subscription closes; the caller is responsible for the
/// outer reconnect loop (see [`run_discovery_live_with_reconnect`]).
pub async fn run_discovery_live_once(
    provider: Arc<RootProvider<PubSubFrontend>>,
    vtokens: Vec<Address>,
    set: BorrowerSet,
    sink: mpsc::Sender<Address>,
) -> Result<()> {
    if vtokens.is_empty() {
        warn!("discovery: no vTokens supplied — live subscription is a no-op");
        return Ok(());
    }
    let topic = FixedBytes::<32>::from(BORROW_TOPIC0.0);
    let filter = Filter::new().address(vtokens).event_signature(topic);
    let sub = provider
        .subscribe_logs(&filter)
        .await
        .context("subscribe_logs(Borrow) failed")?;
    let mut stream = sub.into_stream();
    info!("discovery live subscription established");
    while let Some(log) = stream.next().await {
        let Some(borrower) = decode_borrow_borrower(&log) else {
            continue;
        };
        let block = log.block_number.unwrap_or(0);
        if set.upsert(borrower, block) {
            // best-effort send — if the consumer is slow we'd rather drop
            // a notification than back-pressure the log stream and risk
            // upstream subscription drop.
            let _ = sink.try_send(borrower);
        }
    }
    Ok(())
}

/// Run [`run_discovery_live_once`] forever, reconnecting on any
/// subscription failure with jittered exponential backoff. Mirrors
/// the reconnect discipline of `BlockListener::run` (1 s base, x2 per
/// failure, capped at [`DISCOVERY_RECONNECT_MAX_BACKOFF`], up-to-25%
/// jitter) so a flapping upstream WS does not cause a thundering-herd
/// reconnect across multiple bots and a long outage does not melt the
/// endpoint.
///
/// Returns only if the task is cancelled (e.g. by `JoinHandle::abort`
/// on shutdown). The `chain` argument is included on every log line
/// so multi-chain deployments can grep their event stream.
pub async fn run_discovery_live_with_reconnect(
    provider: Arc<RootProvider<PubSubFrontend>>,
    vtokens: Vec<Address>,
    set: BorrowerSet,
    sink: mpsc::Sender<Address>,
    chain: String,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        match run_discovery_live_once(provider.clone(), vtokens.clone(), set.clone(), sink.clone())
            .await
        {
            Ok(()) => {
                // Subscription closed cleanly (upstream drop, no
                // protocol error). Reset backoff and reconnect
                // immediately so we don't penalise a clean cycle.
                backoff = Duration::from_secs(1);
                warn!(
                    chain = %chain,
                    "discovery live subscription closed — reconnecting"
                );
            }
            Err(err) => {
                let cap_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX);
                let jitter_ms = rand::thread_rng().gen_range(0..=cap_ms.saturating_div(4));
                let wait = backoff.saturating_add(Duration::from_millis(jitter_ms));
                warn!(
                    chain = %chain,
                    error = ?err,
                    wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                    "discovery live subscription dropped — reconnecting after jittered backoff"
                );
                tokio::time::sleep(wait).await;
                backoff = backoff
                    .saturating_mul(2)
                    .min(DISCOVERY_RECONNECT_MAX_BACKOFF);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Bytes, LogData};

    fn make_log(borrower: Address) -> Log {
        let mut data = vec![0u8; 32 * 4];
        // address occupies bytes 12..32 of the first word
        data[12..32].copy_from_slice(borrower.as_slice());
        Log {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: LogData::new_unchecked(
                    vec![FixedBytes::<32>::from(BORROW_TOPIC0.0)],
                    Bytes::from(data),
                ),
            },
            block_hash: None,
            block_number: Some(123),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    #[test]
    fn decode_picks_borrower_from_data_first_word() {
        let borrower = Address::from_slice(&[0xAB; 20]);
        let log = make_log(borrower);
        assert_eq!(decode_borrow_borrower(&log), Some(borrower));
    }

    #[test]
    fn decode_short_data_returns_none() {
        let mut log = make_log(Address::ZERO);
        log.inner.data = LogData::new_unchecked(
            vec![FixedBytes::<32>::from(BORROW_TOPIC0.0)],
            Bytes::from(vec![0u8; 16]),
        );
        assert_eq!(decode_borrow_borrower(&log), None);
    }

    #[test]
    fn upsert_returns_true_only_for_first_insert() {
        let set = BorrowerSet::new();
        let a = Address::from_slice(&[0x01; 20]);
        assert!(set.upsert(a, 100));
        assert!(!set.upsert(a, 200));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn upsert_advances_last_seen_only_forward() {
        let set = BorrowerSet::new();
        let a = Address::from_slice(&[0x02; 20]);
        set.upsert(a, 200);
        set.upsert(a, 100);
        let info = *set.inner.get(&a).unwrap();
        assert_eq!(info.last_seen_block, 200);
        set.upsert(a, 300);
        let info = *set.inner.get(&a).unwrap();
        assert_eq!(info.last_seen_block, 300);
    }

    #[test]
    fn snapshot_lists_all_addresses() {
        let set = BorrowerSet::new();
        for i in 0..5u8 {
            set.upsert(Address::from_slice(&[i; 20]), 1);
        }
        assert_eq!(set.snapshot().len(), 5);
    }
}
