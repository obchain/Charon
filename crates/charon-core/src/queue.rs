//! Profit-ordered opportunity queue.
//!
//! After the router prices a liquidation, the resulting
//! [`LiquidationOpportunity`] lands in this queue. The executor drains
//! entries highest-net-profit first, dropping anything older than
//! `ttl_blocks` (default 2) — stale quotes are priced against stale
//! balances and usually revert on `eth_call` anyway.
//!
//! Backed by `std::collections::BinaryHeap`. Ordering is defined on a
//! private `QueueEntry` wrapper so we don't put `Ord` on the public
//! `LiquidationOpportunity` type (which already derives `Serialize`
//! and could pick up unrelated semantics from a natural ordering).

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::types::LiquidationOpportunity;

/// Default TTL, in blocks. Two blocks ≈ 6 s on BSC — long enough to
/// survive one routing round-trip but short enough that stale quotes
/// don't pile up.
pub const DEFAULT_TTL_BLOCKS: u64 = 2;

/// Heap wrapper — compares by `net_profit_usd_cents` so the root of
/// the `BinaryHeap` (max-heap) is the most profitable opportunity.
#[derive(Debug, Clone)]
struct QueueEntry {
    opportunity: LiquidationOpportunity,
    queued_at_block: u64,
}

impl PartialEq for QueueEntry {
    fn eq(&self, other: &Self) -> bool {
        self.opportunity.net_profit_usd_cents == other.opportunity.net_profit_usd_cents
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
        self.opportunity
            .net_profit_usd_cents
            .cmp(&other.opportunity.net_profit_usd_cents)
    }
}

/// Priority queue of ready-to-execute liquidations.
pub struct OpportunityQueue {
    heap: BinaryHeap<QueueEntry>,
    ttl_blocks: u64,
}

impl OpportunityQueue {
    pub fn new(ttl_blocks: u64) -> Self {
        Self {
            heap: BinaryHeap::new(),
            ttl_blocks,
        }
    }

    pub fn with_default_ttl() -> Self {
        Self::new(DEFAULT_TTL_BLOCKS)
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Enqueue a freshly-priced opportunity, tagged with the block it
    /// was queued at (for TTL accounting).
    pub fn push(&mut self, opportunity: LiquidationOpportunity, queued_at_block: u64) {
        self.heap.push(QueueEntry {
            opportunity,
            queued_at_block,
        });
    }

    /// Pop the highest-profit *fresh* opportunity, silently discarding
    /// any stale entries popped along the way. Returns `None` when the
    /// queue has no fresh entries left.
    pub fn pop(&mut self, current_block: u64) -> Option<LiquidationOpportunity> {
        while let Some(entry) = self.heap.pop() {
            if !self.is_stale(&entry, current_block) {
                return Some(entry.opportunity);
            }
        }
        None
    }

    /// Remove every stale entry, returning the number dropped. Cheap
    /// to run once per block so stale opportunities don't balloon the
    /// heap between bursts.
    pub fn prune_stale(&mut self, current_block: u64) -> usize {
        let before = self.heap.len();
        let fresh: Vec<QueueEntry> = std::mem::take(&mut self.heap)
            .into_iter()
            .filter(|e| !self.is_stale(e, current_block))
            .collect();
        self.heap = BinaryHeap::from(fresh);
        before - self.heap.len()
    }

    fn is_stale(&self, entry: &QueueEntry, current_block: u64) -> bool {
        current_block.saturating_sub(entry.queued_at_block) > self.ttl_blocks
    }
}

impl Default for OpportunityQueue {
    fn default() -> Self {
        Self::with_default_ttl()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FlashLoanSource, Position, ProtocolId, SwapRoute};
    use alloy::primitives::{Address, U256, address};

    fn mk_opp(net_cents: u64) -> LiquidationOpportunity {
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
                pool_fee: 0,
            },
            net_profit_usd_cents: net_cents,
        }
    }

    #[test]
    fn pop_returns_highest_profit_first() {
        let mut q = OpportunityQueue::new(5);
        q.push(mk_opp(100), 1);
        q.push(mk_opp(500), 1);
        q.push(mk_opp(250), 1);
        assert_eq!(q.pop(1).unwrap().net_profit_usd_cents, 500);
        assert_eq!(q.pop(1).unwrap().net_profit_usd_cents, 250);
        assert_eq!(q.pop(1).unwrap().net_profit_usd_cents, 100);
        assert!(q.pop(1).is_none());
    }

    #[test]
    fn stale_entries_are_dropped_on_pop() {
        let mut q = OpportunityQueue::new(2);
        q.push(mk_opp(999), 10); // queued at block 10
        // Current block 13 → age 3 > ttl 2 → stale
        assert!(q.pop(13).is_none());
    }

    #[test]
    fn fresh_survives_ttl_boundary() {
        let mut q = OpportunityQueue::new(2);
        q.push(mk_opp(42), 10);
        // age 2 == ttl 2 → still fresh (ttl is inclusive)
        assert_eq!(q.pop(12).unwrap().net_profit_usd_cents, 42);
    }

    #[test]
    fn prune_stale_drops_old_entries_and_reports_count() {
        let mut q = OpportunityQueue::new(2);
        q.push(mk_opp(100), 5);
        q.push(mk_opp(200), 10);
        q.push(mk_opp(300), 11);
        assert_eq!(q.len(), 3);
        // At block 12: block-5 is 7 (stale), block-10 is 2 (fresh),
        // block-11 is 1 (fresh). One dropped.
        let dropped = q.prune_stale(12);
        assert_eq!(dropped, 1);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn default_ttl_is_two_blocks() {
        let q = OpportunityQueue::with_default_ttl();
        assert_eq!(q.ttl_blocks, DEFAULT_TTL_BLOCKS);
    }
}
