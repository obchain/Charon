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

/// `keccak256("RepayBorrow(address,address,uint256,uint256,uint256)")` —
/// Compound / Venus `RepayBorrow` emitted on every (partial or full)
/// repay. `accountBorrows == 0` means the borrower is fully closed and
/// can be pruned from the active scan set.
///
/// Layout: `data` = [payer, borrower, repayAmount, accountBorrows,
/// totalBorrows], each 32-byte word (no indexed fields on the
/// Compound-fork event).
pub const REPAY_BORROW_TOPIC0: B256 =
    b256!("1a2a22cb034d26d1854bdc6666a5b91fe25efbbb5dcad3b0355478d6f5c362a1");

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
    /// Last block at which this borrower emitted a `Borrow` or
    /// `RepayBorrow` event.
    pub last_seen_block: u64,
    /// Whether the borrower currently holds an active position. Set
    /// to `false` by [`BorrowerSet::mark_inactive`] when a
    /// `RepayBorrow` event reports `accountBorrows == 0`. Re-set to
    /// `true` by a subsequent `Borrow` upsert. Inactive entries stay
    /// in the map (so a re-borrow is a cheap upsert) but are
    /// excluded from [`BorrowerSet::snapshot`] and
    /// `fetch_positions`.
    pub active: bool,
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

    /// Insert or refresh an entry, marking it active. Returns `true`
    /// when the address was new. A `Borrow` event also re-activates
    /// a previously-inactive entry (re-borrow after full repay).
    pub fn upsert(&self, addr: Address, block: u64) -> bool {
        let mut inserted = false;
        self.inner
            .entry(addr)
            .and_modify(|existing| {
                if block > existing.last_seen_block {
                    existing.last_seen_block = block;
                }
                existing.active = true;
            })
            .or_insert_with(|| {
                inserted = true;
                BorrowerInfo {
                    last_seen_block: block,
                    active: true,
                }
            });
        inserted
    }

    /// Mark an existing entry inactive (Compound `RepayBorrow` with
    /// `accountBorrows == 0`). Idempotent: re-marking an already
    /// inactive entry is a no-op. Marking an unseen address inserts
    /// a fresh inactive record so a later `Borrow` upsert fills in
    /// the right state without losing the seen-at block. Always
    /// updates `last_seen_block` if the supplied `block` is newer
    /// than the stored one — a same-block borrow → repay sequence
    /// tracks correctly.
    pub fn mark_inactive(&self, addr: Address, block: u64) {
        self.inner
            .entry(addr)
            .and_modify(|existing| {
                if block > existing.last_seen_block {
                    existing.last_seen_block = block;
                }
                existing.active = false;
            })
            .or_insert(BorrowerInfo {
                last_seen_block: block,
                active: false,
            });
    }

    /// Snapshot of currently-active borrowers (excluding any pruned
    /// by [`mark_inactive`]). The scanner consumes this to drive the
    /// next round of position fetches.
    pub fn snapshot(&self) -> Vec<Address> {
        self.inner
            .iter()
            .filter(|kv| kv.value().active)
            .map(|kv| *kv.key())
            .collect()
    }

    /// Total tracked entries (active + inactive). Sticks with the
    /// historical name so existing call sites stay simple; for
    /// active-only counts use [`active_len`].
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Number of currently-active borrowers (those that pass the
    /// [`snapshot`] filter).
    pub fn active_len(&self) -> usize {
        self.inner.iter().filter(|kv| kv.value().active).count()
    }

    /// Number of currently-inactive borrowers (RepayBorrow with
    /// `accountBorrows == 0` observed, no subsequent `Borrow`).
    pub fn inactive_len(&self) -> usize {
        self.inner.iter().filter(|kv| !kv.value().active).count()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn contains(&self, addr: &Address) -> bool {
        self.inner.contains_key(addr)
    }

    /// Diagnostic: returns the stored `active` flag for an address,
    /// `None` when the address is not tracked. Used by tests to
    /// assert the borrow → repay → borrow cycle leaves the entry
    /// active.
    pub fn is_active(&self, addr: &Address) -> Option<bool> {
        self.inner.get(addr).map(|e| e.active)
    }

    /// Materialise every (address, info) pair as a `Vec` snapshot.
    /// Used by the persistence layer (#349) to write the borrower
    /// set to disk without holding a `DashMap` reference across an
    /// IO call.
    pub fn entries(&self) -> Vec<(Address, BorrowerInfo)> {
        self.inner
            .iter()
            .map(|kv| (*kv.key(), *kv.value()))
            .collect()
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

/// Decode `(borrower, accountBorrows)` from a Compound `RepayBorrow`
/// log. Layout (no indexed fields):
/// `data = [payer, borrower, repayAmount, accountBorrows, totalBorrows]`,
/// each a 32-byte word. Returns `None` for a short / malformed log so
/// a single bad upstream record cannot abort the discovery loop.
pub fn decode_repay_borrow(log: &Log) -> Option<(Address, alloy::primitives::U256)> {
    let data = log.data().data.as_ref();
    // 5 × 32 bytes minimum.
    if data.len() < 160 {
        return None;
    }
    let borrower = Address::from_slice(&data[32 + 12..64]);
    let account_borrows = alloy::primitives::U256::from_be_slice(&data[96..128]);
    Some((borrower, account_borrows))
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
    let topic_borrow = FixedBytes::<32>::from(BORROW_TOPIC0.0);
    let topic_repay = FixedBytes::<32>::from(REPAY_BORROW_TOPIC0.0);
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
        // Single eth_getLogs per chunk with a topic OR-set: Borrow
        // + RepayBorrow. eth_getLogs returns logs in (block,
        // log_index) order, so applying them in sequence keeps a
        // same-block borrow → repay → borrow cycle correct.
        let filter = Filter::new()
            .from_block(cursor)
            .to_block(chunk_end)
            .address(vtokens.clone())
            .event_signature(vec![topic_borrow, topic_repay]);
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
            let Some(topic0) = log.topics().first() else {
                continue;
            };
            let block = log.block_number.unwrap_or(chunk_end);
            if topic0.0 == BORROW_TOPIC0.0 {
                if let Some(borrower) = decode_borrow_borrower(log) {
                    if set.upsert(borrower, block) {
                        new_count = new_count.saturating_add(1);
                    }
                }
            } else if topic0.0 == REPAY_BORROW_TOPIC0.0 {
                if let Some((borrower, account_borrows)) = decode_repay_borrow(log) {
                    if account_borrows.is_zero() {
                        set.mark_inactive(borrower, block);
                    }
                    // accountBorrows > 0 means partial repay — keep
                    // the entry active.
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
    let topic_borrow = FixedBytes::<32>::from(BORROW_TOPIC0.0);
    let topic_repay = FixedBytes::<32>::from(REPAY_BORROW_TOPIC0.0);
    let filter = Filter::new()
        .address(vtokens)
        .event_signature(vec![topic_borrow, topic_repay]);
    let sub = provider
        .subscribe_logs(&filter)
        .await
        .context("subscribe_logs(Borrow|RepayBorrow) failed")?;
    let mut stream = sub.into_stream();
    info!("discovery live subscription established");
    while let Some(log) = stream.next().await {
        let Some(topic0) = log.topics().first() else {
            continue;
        };
        let block = log.block_number.unwrap_or(0);
        if topic0.0 == BORROW_TOPIC0.0 {
            let Some(borrower) = decode_borrow_borrower(&log) else {
                continue;
            };
            if set.upsert(borrower, block) {
                // best-effort send — if the consumer is slow we'd rather drop
                // a notification than back-pressure the log stream and risk
                // upstream subscription drop.
                let _ = sink.try_send(borrower);
            }
        } else if topic0.0 == REPAY_BORROW_TOPIC0.0 {
            if let Some((borrower, account_borrows)) = decode_repay_borrow(&log) {
                if account_borrows.is_zero() {
                    set.mark_inactive(borrower, block);
                }
            }
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
    fn snapshot_lists_only_active_addresses() {
        let set = BorrowerSet::new();
        for i in 0..5u8 {
            set.upsert(Address::from_slice(&[i; 20]), 1);
        }
        // Mark one inactive — snapshot must skip it but len() retains.
        set.mark_inactive(Address::from_slice(&[0u8; 20]), 2);
        assert_eq!(set.snapshot().len(), 4);
        assert_eq!(set.len(), 5);
        assert_eq!(set.active_len(), 4);
        assert_eq!(set.inactive_len(), 1);
    }

    fn make_repay_log(
        payer: Address,
        borrower: Address,
        repay: alloy::primitives::U256,
        account_borrows: alloy::primitives::U256,
        total: alloy::primitives::U256,
        block: u64,
    ) -> Log {
        let mut data = Vec::with_capacity(32 * 5);
        let mut word = [0u8; 32];
        word[12..32].copy_from_slice(payer.as_slice());
        data.extend_from_slice(&word);
        let mut word = [0u8; 32];
        word[12..32].copy_from_slice(borrower.as_slice());
        data.extend_from_slice(&word);
        data.extend_from_slice(&repay.to_be_bytes::<32>());
        data.extend_from_slice(&account_borrows.to_be_bytes::<32>());
        data.extend_from_slice(&total.to_be_bytes::<32>());
        Log {
            inner: alloy::primitives::Log {
                address: Address::ZERO,
                data: LogData::new_unchecked(
                    vec![FixedBytes::<32>::from(REPAY_BORROW_TOPIC0.0)],
                    Bytes::from(data),
                ),
            },
            block_hash: None,
            block_number: Some(block),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    /// Borrow then full repay leaves the entry tracked but inactive,
    /// excluded from the snapshot.
    #[test]
    fn borrow_then_full_repay_marks_inactive() {
        let set = BorrowerSet::new();
        let b = Address::from_slice(&[0xBB; 20]);
        set.upsert(b, 100);
        assert_eq!(set.is_active(&b), Some(true));

        let log = make_repay_log(
            Address::ZERO,
            b,
            alloy::primitives::U256::from(50u64),
            alloy::primitives::U256::ZERO,
            alloy::primitives::U256::ZERO,
            101,
        );
        let (decoded_b, account) = decode_repay_borrow(&log).expect("decode");
        assert_eq!(decoded_b, b);
        assert!(account.is_zero());
        set.mark_inactive(decoded_b, 101);

        assert_eq!(set.is_active(&b), Some(false));
        assert!(!set.snapshot().contains(&b));
    }

    /// Borrow → full repay → borrow ends active. Same-block ordering
    /// is preserved by the get_logs iteration order in the backfill.
    #[test]
    fn borrow_repay_borrow_ends_active() {
        let set = BorrowerSet::new();
        let b = Address::from_slice(&[0xCC; 20]);
        set.upsert(b, 100);
        set.mark_inactive(b, 100); // same-block repay
        assert_eq!(set.is_active(&b), Some(false));
        // Subsequent re-borrow flips active back on.
        set.upsert(b, 100);
        assert_eq!(set.is_active(&b), Some(true));
        assert!(set.snapshot().contains(&b));
    }

    /// Partial repay (`accountBorrows > 0`) must leave the borrower
    /// active — only `accountBorrows == 0` triggers the prune.
    #[test]
    fn partial_repay_keeps_borrower_active() {
        let set = BorrowerSet::new();
        let b = Address::from_slice(&[0xDD; 20]);
        set.upsert(b, 100);

        let log = make_repay_log(
            Address::ZERO,
            b,
            alloy::primitives::U256::from(40u64),
            alloy::primitives::U256::from(60u64), // still owes 60
            alloy::primitives::U256::from(60u64),
            101,
        );
        let (_, account) = decode_repay_borrow(&log).expect("decode");
        assert!(!account.is_zero());
        // Backfill / live-tail logic: only mark_inactive when
        // accountBorrows is zero. Partial repay path is a no-op.
        // Assert the entry remains active.
        assert_eq!(set.is_active(&b), Some(true));
        assert!(set.snapshot().contains(&b));
    }

    /// `mark_inactive` is idempotent and copes with an unseen address
    /// — the inserted record carries `active = false` so a later
    /// `Borrow` upsert flips the right state.
    #[test]
    fn mark_inactive_is_idempotent_and_handles_unseen() {
        let set = BorrowerSet::new();
        let b = Address::from_slice(&[0xEE; 20]);
        set.mark_inactive(b, 100);
        set.mark_inactive(b, 100);
        assert_eq!(set.is_active(&b), Some(false));
        // Subsequent re-borrow promotes the entry to active without
        // a second insert path.
        set.upsert(b, 200);
        assert_eq!(set.is_active(&b), Some(true));
    }
}
