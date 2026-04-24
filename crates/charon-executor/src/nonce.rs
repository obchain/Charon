//! Concurrent nonce manager.
//!
//! One [`NonceManager`] per `(chain × signer)` pair. Holds the next
//! nonce in an `AtomicU64` so the tx builder can hand out sequential
//! values without `&mut` plumbing through the hot path. Optimistic by
//! default — the manager bumps locally on every issue, and the caller
//! re-syncs from chain after a failed broadcast / on startup.
//!
//! Why optimistic: `eth_getTransactionCount(pending)` is one
//! round-trip we don't want on every block. Bumping locally lets
//! multiple in-flight txs hold contiguous nonces; if one reverts and
//! a gap opens up, a `resync` paves it over.
//!
//! ### `pending` vs `latest`
//!
//! Both `init` and `resync` query the **pending** block tag, not
//! `latest`. Reason: if we've already broadcast N txs this block and
//! resync against `latest`, the mempool-side nonce we get back is
//! stale — it reflects the state *before* our pending bundle. Using
//! `pending` counts our own in-flight txs and avoids reusing a nonce
//! that's sitting in the mempool waiting for inclusion.
//!
//! ### High-water-mark guard
//!
//! `resync` is a rescue path, not a rollback. If the chain reports
//! nonce = 42 but we've already handed out 47 locally, snapping to 42
//! would double-issue nonces 42–46 — every one of those txs would
//! revert with `nonce too low` once the in-flight ones land. The
//! high-water mark tracks the maximum value [`next`] ever handed out
//! and clamps `resync` to `max(on_chain, high_water + 1)`.

use std::sync::atomic::{AtomicU64, Ordering};

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::transports::TransportError;
use tracing::{debug, info, warn};

/// Errors the nonce manager can surface. One variant for now —
/// `#[non_exhaustive]` keeps the door open for variants like
/// `NonceGap { on_chain, local }` without a breaking change later.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NonceError {
    #[error("provider error: {0}")]
    Provider(#[from] TransportError),
}

/// Tracks the next-to-use nonce for one signer on one chain.
#[derive(Debug)]
pub struct NonceManager {
    signer: Address,
    /// Next nonce to hand out. Bumped via `fetch_add`.
    next: AtomicU64,
    /// Max value `next()` ever returned + 1. Used by `resync` to
    /// refuse going backwards past an already-issued nonce.
    high_water: AtomicU64,
}

impl NonceManager {
    /// Build with an explicit starting value. Most callers should use
    /// [`init`] instead, which pulls the on-chain nonce.
    pub fn new(signer: Address, start: u64) -> Self {
        Self {
            signer,
            next: AtomicU64::new(start),
            high_water: AtomicU64::new(start),
        }
    }

    /// Async constructor: pulls `eth_getTransactionCount(pending)`
    /// and stores it as the starting nonce. `pending` is mandatory
    /// here — see module-level docs.
    pub async fn init<P, T>(provider: &P, signer: Address) -> Result<Self, NonceError>
    where
        P: Provider<T>,
        T: alloy::transports::Transport + Clone,
    {
        let nonce = provider
            .get_transaction_count(signer)
            .block_id(BlockId::Number(BlockNumberOrTag::Pending))
            .await?;
        info!(%signer, nonce, "nonce manager initialised (pending tag)");
        Ok(Self::new(signer, nonce))
    }

    /// Signer the manager tracks — handy for sanity assertions.
    pub fn signer(&self) -> Address {
        self.signer
    }

    /// Peek without consuming. Useful for logging.
    pub fn current(&self) -> u64 {
        self.next.load(Ordering::Acquire)
    }

    /// Highest nonce ever issued (or `start` if nothing has been
    /// issued). Public for diagnostics; `resync` consults it
    /// internally.
    pub fn high_water(&self) -> u64 {
        self.high_water.load(Ordering::Acquire)
    }

    /// Atomically claim the next nonce and bump the counter. Two
    /// concurrent calls always return distinct values.
    pub fn next(&self) -> u64 {
        let n = self.next.fetch_add(1, Ordering::AcqRel);
        // Update high-water mark monotonically. Using fetch_max so
        // racing threads collapse to the same final value without a
        // lost-update window.
        self.high_water.fetch_max(n + 1, Ordering::AcqRel);
        debug!(signer = %self.signer, nonce = n, "nonce issued");
        n
    }

    /// Re-fetch the on-chain nonce and adopt it — but never go
    /// backwards past an already-issued value.
    ///
    /// Concretely: `next` is set to `max(on_chain, high_water)`. If
    /// the chain lags our local view (common: our own pending txs
    /// haven't landed yet), we keep the local nonce. If the chain
    /// jumped ahead (cold start, manual transfer from the same key),
    /// we adopt the chain value.
    ///
    /// Returns the value `next` was set to.
    pub async fn resync<P, T>(&self, provider: &P) -> Result<u64, NonceError>
    where
        P: Provider<T>,
        T: alloy::transports::Transport + Clone,
    {
        let on_chain = provider
            .get_transaction_count(self.signer)
            .block_id(BlockId::Number(BlockNumberOrTag::Pending))
            .await?;
        let hw = self.high_water.load(Ordering::Acquire);
        let target = on_chain.max(hw);

        if on_chain < hw {
            warn!(
                signer = %self.signer,
                on_chain,
                high_water = hw,
                "resync: chain lags local high-water, keeping local value"
            );
        }

        self.next.store(target, Ordering::Release);
        // Keep high_water in lockstep — if chain jumped ahead, our
        // new baseline is the chain value.
        self.high_water.fetch_max(target, Ordering::AcqRel);
        info!(
            signer = %self.signer,
            nonce = target,
            on_chain,
            "nonce manager resynced (pending tag, hw-guarded)"
        );
        Ok(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use std::sync::Arc;

    fn signer() -> Address {
        address!("1111111111111111111111111111111111111111")
    }

    #[test]
    fn next_returns_sequential_values() {
        let m = NonceManager::new(signer(), 7);
        assert_eq!(m.current(), 7);
        assert_eq!(m.next(), 7);
        assert_eq!(m.next(), 8);
        assert_eq!(m.next(), 9);
        assert_eq!(m.current(), 10);
        assert_eq!(m.high_water(), 10);
    }

    #[test]
    fn high_water_tracks_max_issued() {
        let m = NonceManager::new(signer(), 0);
        for _ in 0..5 {
            m.next();
        }
        assert_eq!(m.high_water(), 5);
    }

    #[test]
    fn concurrent_callers_get_distinct_nonces() {
        // Hammer the atomic from 32 threads × 100 calls each. Every
        // returned nonce must be unique and the final `current()` must
        // equal start + 32 × 100.
        const THREADS: usize = 32;
        const PER: usize = 100;
        let m = Arc::new(NonceManager::new(signer(), 0));

        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let m = m.clone();
            handles.push(std::thread::spawn(move || {
                let mut local = Vec::with_capacity(PER);
                for _ in 0..PER {
                    local.push(m.next());
                }
                local
            }));
        }

        let mut all = Vec::with_capacity(THREADS * PER);
        for h in handles {
            all.extend(h.join().expect("test thread panicked"));
        }
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), THREADS * PER, "duplicate nonce issued");
        assert_eq!(m.current(), (THREADS * PER) as u64);
        assert_eq!(m.high_water(), (THREADS * PER) as u64);
    }
}
