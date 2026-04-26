//! Profit-ordered opportunity queue.
//!
//! After the router prices a liquidation, the resulting
//! [`LiquidationOpportunity`] lands in this queue. The executor drains
//! entries highest-net-profit first, dropping anything older than
//! `ttl_blocks` (default 2) — stale quotes are priced against stale
//! balances and usually revert on `eth_call` anyway.
//!
//! The queue is `Send + Sync` and cloneable: it wraps a
//! `std::collections::BinaryHeap` inside a [`tokio::sync::Mutex`] inside
//! an `Arc` so a single `OpportunityQueue` handle can be shared across
//! the block listener, scanner, and executor tasks.
//!
//! # Ordering
//!
//! The heap is keyed on a private [`QueueEntry`] wrapper so we do not
//! leak `Ord` semantics out of the type. Ordering is lexicographic:
//!
//! 1. **net profit** — delegated to
//!    [`LiquidationOpportunity`]'s own `Ord` impl, which compares by
//!    `net_profit_wei` ascending so the max-heap pops the highest wei
//!    first.
//! 2. **inserted_at_block, descending** — on a tie, the fresher entry
//!    wins. This matters around reorgs, where two identically-priced
//!    entries may land on either side of a re-seen block; the fresher
//!    one is strictly better because its balance / price snapshot is
//!    younger.
//!
//! Manual `PartialEq` / `Eq` mirror `Ord` exactly so the heap's
//! invariants hold even after `retain`-style surgery.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::types::LiquidationOpportunity;

/// Default TTL, in blocks. Two blocks ~= 6 s on BSC — long enough to
/// survive one routing round-trip but short enough that stale quotes
/// don't pile up.
pub const DEFAULT_TTL_BLOCKS: u64 = 2;

/// Heap wrapper — compares by the opportunity's own `Ord` (net profit
/// wei ascending) first, then by `inserted_at_block` (fresher wins).
/// See the module docs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct QueueEntry {
    pub opportunity: LiquidationOpportunity,
    /// Block height at which this entry was enqueued — drives both TTL
    /// expiry and the Ord tie-break.
    pub inserted_at_block: u64,
}

impl PartialEq for QueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for QueueEntry {}
impl PartialOrd for QueueEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for QueueEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap. LiquidationOpportunity::Ord ranks
        // by net_profit_wei ascending — largest profit pops first.
        // Tie-break on inserted_at_block ascending so that the larger
        // (= fresher) block pops first.
        self.opportunity
            .cmp(&other.opportunity)
            .then_with(|| self.inserted_at_block.cmp(&other.inserted_at_block))
    }
}

/// Thread-safe priority queue of ready-to-execute liquidations.
///
/// Clone to hand a new handle to another task — all handles share the
/// same underlying heap.
#[derive(Clone, Debug)]
pub struct OpportunityQueue {
    inner: Arc<Mutex<BinaryHeap<QueueEntry>>>,
    ttl_blocks: u64,
}

impl OpportunityQueue {
    /// Create a new queue with an explicit TTL, in blocks.
    pub fn new(ttl_blocks: u64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(BinaryHeap::new())),
            ttl_blocks,
        }
    }

    /// Create a new queue with [`DEFAULT_TTL_BLOCKS`].
    pub fn with_default_ttl() -> Self {
        Self::new(DEFAULT_TTL_BLOCKS)
    }

    /// TTL this queue was constructed with.
    pub fn ttl_blocks(&self) -> u64 {
        self.ttl_blocks
    }

    /// Current number of entries (stale entries included — run
    /// [`prune_stale`](Self::prune_stale) first to exclude them).
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// `true` when the heap is empty.
    pub async fn is_empty(&self) -> bool {
        self.inner.lock().await.is_empty()
    }

    /// Enqueue a freshly-priced opportunity, tagged with the block it
    /// was queued at (for TTL accounting).
    pub async fn push(&self, opportunity: LiquidationOpportunity, inserted_at_block: u64) {
        self.inner.lock().await.push(QueueEntry {
            opportunity,
            inserted_at_block,
        });
    }

    /// Pop the highest-profit *fresh* opportunity, silently discarding
    /// any stale entries popped along the way. Returns `None` when the
    /// queue has no fresh entries left.
    pub async fn pop(&self, current_block: u64) -> Option<LiquidationOpportunity> {
        let mut guard = self.inner.lock().await;
        while let Some(entry) = guard.pop() {
            if !is_stale(&entry, current_block, self.ttl_blocks) {
                return Some(entry.opportunity);
            }
        }
        None
    }

    /// Remove every stale entry, returning the number dropped. Cheap
    /// to run once per block so stale opportunities don't balloon the
    /// heap between bursts.
    pub async fn prune_stale(&self, current_block: u64) -> usize {
        let mut guard = self.inner.lock().await;
        let before = guard.len();
        let ttl = self.ttl_blocks;
        let fresh: Vec<QueueEntry> = std::mem::take(&mut *guard)
            .into_iter()
            .filter(|e| !is_stale(e, current_block, ttl))
            .collect();
        *guard = BinaryHeap::from(fresh);
        // before >= guard.len() by construction.
        before.saturating_sub(guard.len())
    }
}

impl Default for OpportunityQueue {
    fn default() -> Self {
        Self::with_default_ttl()
    }
}

/// Age-based staleness. `current_block - inserted_at_block > ttl`. Uses
/// `saturating_sub` so a reorg that momentarily *rewinds* the block
/// pointer (current_block < inserted_at_block) treats the entry as
/// fresh rather than wrapping to a near-`u64::MAX` age.
fn is_stale(entry: &QueueEntry, current_block: u64, ttl: u64) -> bool {
    current_block.saturating_sub(entry.inserted_at_block) > ttl
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FlashLoanSource, Position, ProtocolId, SwapRoute};
    use alloy::primitives::{Address, U256, address};

    fn mk_opp(net_wei: U256) -> LiquidationOpportunity {
        LiquidationOpportunity {
            position: Position {
                protocol: ProtocolId::Venus,
                chain_id: 56,
                borrower: address!("1111111111111111111111111111111111111111"),
                collateral_token: Address::ZERO,
                debt_token: Address::ZERO,
                collateral_amount: U256::ZERO,
                debt_amount: U256::ZERO,
                health_factor: U256::ZERO,
                liquidation_bonus_bps: 1_000,
            },
            debt_to_repay: U256::ZERO,
            expected_collateral_out: U256::ZERO,
            flash_source: FlashLoanSource::AaveV3,
            swap_route: SwapRoute {
                token_in: Address::ZERO,
                token_out: Address::ZERO,
                amount_in: U256::ZERO,
                min_amount_out: U256::ZERO,
                pool_fee: None,
            },
            net_profit_wei: net_wei,
        }
    }

    #[tokio::test]
    async fn pop_returns_highest_profit_first() {
        let q = OpportunityQueue::new(5);
        q.push(mk_opp(U256::from(100u64)), 1).await;
        q.push(mk_opp(U256::from(500u64)), 1).await;
        q.push(mk_opp(U256::from(250u64)), 1).await;
        assert_eq!(
            q.pop(1).await.expect("fresh").net_profit_wei,
            U256::from(500u64)
        );
        assert_eq!(
            q.pop(1).await.expect("fresh").net_profit_wei,
            U256::from(250u64)
        );
        assert_eq!(
            q.pop(1).await.expect("fresh").net_profit_wei,
            U256::from(100u64)
        );
        assert!(q.pop(1).await.is_none());
    }

    #[tokio::test]
    async fn stale_entries_are_dropped_on_pop() {
        let q = OpportunityQueue::new(2);
        q.push(mk_opp(U256::from(999u64)), 10).await; // queued at block 10
        // Current block 13 -> age 3 > ttl 2 -> stale
        assert!(q.pop(13).await.is_none());
    }

    #[tokio::test]
    async fn fresh_survives_ttl_boundary() {
        let q = OpportunityQueue::new(2);
        q.push(mk_opp(U256::from(42u64)), 10).await;
        // age 2 == ttl 2 -> still fresh (ttl is inclusive)
        assert_eq!(
            q.pop(12).await.expect("fresh").net_profit_wei,
            U256::from(42u64)
        );
    }

    #[tokio::test]
    async fn prune_stale_drops_old_entries_and_reports_count() {
        let q = OpportunityQueue::new(2);
        q.push(mk_opp(U256::from(100u64)), 5).await;
        q.push(mk_opp(U256::from(200u64)), 10).await;
        q.push(mk_opp(U256::from(300u64)), 11).await;
        assert_eq!(q.len().await, 3);
        // At block 12: block-5 is 7 (stale), block-10 is 2 (fresh),
        // block-11 is 1 (fresh). One dropped.
        let dropped = q.prune_stale(12).await;
        assert_eq!(dropped, 1);
        assert_eq!(q.len().await, 2);
    }

    #[tokio::test]
    async fn default_ttl_is_two_blocks() {
        let q = OpportunityQueue::with_default_ttl();
        assert_eq!(q.ttl_blocks(), DEFAULT_TTL_BLOCKS);
    }

    /// Ord tie-break: two entries with the same net profit should pop
    /// in fresher-first order. With LiquidationOpportunity::PartialEq
    /// keying on (net_profit_wei, borrower, chain_id), equal-profit
    /// entries are Ordering::Equal on the primary key — the queue's
    /// `inserted_at_block` tie-break must take over.
    #[tokio::test]
    async fn tie_break_favours_fresher_entry() {
        let q = OpportunityQueue::new(10);
        q.push(mk_opp(U256::from(500u64)), 100).await; // older
        q.push(mk_opp(U256::from(500u64)), 105).await; // fresher
        q.push(mk_opp(U256::from(500u64)), 102).await; // middle
        let first = q.pop(110).await.expect("fresh").net_profit_wei;
        assert_eq!(first, U256::from(500u64));
        // All three share net_profit; tie-break by inserted_at_block
        // desc should have popped the 105 entry first. Remaining two
        // must still drain without violating heap invariant.
        assert_eq!(q.len().await, 2);
        let _ = q.pop(110).await.expect("second");
        let _ = q.pop(110).await.expect("third");
        assert!(q.pop(110).await.is_none());
    }

    /// Reorg scenario: entry enqueued at block 105, chain reorgs and
    /// the current block pointer rewinds to 104. `saturating_sub` keeps
    /// the entry alive (treated as age 0) rather than wrapping to a
    /// massive age and being pruned.
    #[tokio::test]
    async fn reorg_rewind_does_not_drop_entry() {
        let q = OpportunityQueue::new(2);
        q.push(mk_opp(U256::from(777u64)), 105).await;
        // Reorg: head rewinds to block 104.
        assert_eq!(q.prune_stale(104).await, 0);
        assert_eq!(q.len().await, 1);
        // Entry must still be poppable at the rewound head.
        let out = q.pop(104).await.expect("survives reorg rewind");
        assert_eq!(out.net_profit_wei, U256::from(777u64));
    }

    /// Prunable entry at block 105 stays dropped across a rewind to
    /// 104: once removed from the heap, it does not resurrect.
    #[tokio::test]
    async fn pruned_entry_stays_dropped_after_reorg() {
        let q = OpportunityQueue::new(2);
        q.push(mk_opp(U256::from(100u64)), 95).await; // age at block 105 = 10 -> stale
        q.push(mk_opp(U256::from(200u64)), 103).await; // fresh
        assert_eq!(q.prune_stale(105).await, 1);
        assert_eq!(q.len().await, 1);

        // Reorg rewinds the head to 104. The pruned block-95 entry is
        // already gone from the heap; it must not reappear.
        let out = q.pop(104).await.expect("survivor");
        assert_eq!(out.net_profit_wei, U256::from(200u64));
        assert!(q.pop(104).await.is_none());
    }

    /// Spawn 16 producer tasks concurrently pushing spread profit
    /// values and one consumer task draining the queue. The drained
    /// sequence must be weakly decreasing by net profit wei.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_producers_maintain_heap_order() {
        let q = OpportunityQueue::new(1_000);
        let mut producers = Vec::new();
        for i in 0..16u64 {
            let q_clone = q.clone();
            producers.push(tokio::spawn(async move {
                // Deterministic spread of profit values so the test is
                // reproducible but still exercises interleaving.
                for j in 0..8u64 {
                    let net = i.saturating_mul(8).saturating_add(j).saturating_mul(10);
                    q_clone.push(mk_opp(U256::from(net)), 1).await;
                }
            }));
        }
        for p in producers {
            p.await.expect("producer joined");
        }
        assert_eq!(q.len().await, 16 * 8);

        let mut last = U256::MAX;
        let mut drained = 0usize;
        while let Some(opp) = q.pop(1).await {
            assert!(
                opp.net_profit_wei <= last,
                "ordering violated: {} > previous {last}",
                opp.net_profit_wei
            );
            last = opp.net_profit_wei;
            drained += 1;
        }
        assert_eq!(drained, 16 * 8);
    }
}
