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
//!
//! ### Lease / free-list
//!
//! Issuing a nonce optimistically is fine when the broadcast succeeds.
//! When it fails (sign error, RPC reject, timeout), the nonce was
//! consumed locally but never reached the chain — leaving a
//! permanent local gap. Every later submit then either races with
//! itself or, on private RPCs, lands as `nonce too high` and sits in
//! the mempool indefinitely. `resync` cannot rescue this because it's
//! high-water-clamped.
//!
//! Fix: [`NonceManager::next`] returns a [`NonceLease`] RAII guard.
//! The caller must call [`NonceLease::commit`] once the broadcast has
//! actually entered the mempool. Dropping the lease without
//! committing returns the nonce to a free-list; the next [`next`]
//! call pops the smallest free nonce before falling back to
//! `fetch_add`. Holes are filled before new ground is broken.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::transports::TransportError;
use tracing::{debug, info, warn};

/// Errors the nonce manager can surface.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NonceError {
    #[error("provider error: {0}")]
    Provider(#[from] TransportError),

    /// The local pointer is ahead of the on-chain nonce by an amount
    /// that exceeds the configured tolerance, and the resync path
    /// cannot rewind across the high-water mark. Surface so the caller
    /// can take action (e.g. wait one mempool TTL, then force a
    /// rebroadcast or restart).
    #[error("nonce gap: local={local} on_chain={on_chain}")]
    Gap { local: u64, on_chain: u64 },
}

/// Tracks the next-to-use nonce for one signer on one chain.
#[derive(Debug)]
pub struct NonceManager {
    signer: Address,
    /// Next nonce to hand out via the bump path. Bumped via `fetch_add`.
    next: AtomicU64,
    /// Max value `next()` ever returned + 1. Used by `resync` to
    /// refuse going backwards past an already-issued nonce.
    high_water: AtomicU64,
    /// Released-but-not-committed nonces, ordered so `next()` always
    /// fills the smallest hole first. `BTreeSet` rather than `Vec`
    /// for O(log n) pop-min and dedup safety.
    free_list: Mutex<BTreeSet<u64>>,
}

impl NonceManager {
    /// Build with an explicit starting value. Most callers should use
    /// [`init`] instead, which pulls the on-chain nonce.
    pub fn new(signer: Address, start: u64) -> Self {
        Self {
            signer,
            next: AtomicU64::new(start),
            high_water: AtomicU64::new(start),
            free_list: Mutex::new(BTreeSet::new()),
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

    /// Diagnostic: snapshot the current free-list. Returned vector is
    /// sorted ascending. Empty vector if no holes pending. Used by
    /// the metrics / logs hot path so the gauge value mirrors the
    /// real free-list size.
    pub fn free_list_snapshot(&self) -> Vec<u64> {
        self.free_list
            .lock()
            .expect("nonce free-list poisoned")
            .iter()
            .copied()
            .collect()
    }

    /// Atomically claim the next nonce as a [`NonceLease`]. The lease
    /// returns the nonce to the free-list on drop unless
    /// [`NonceLease::commit`] is called. Two concurrent calls always
    /// return distinct values.
    pub fn next(&self) -> NonceLease<'_> {
        let nonce = self.claim();
        debug!(signer = %self.signer, nonce, "nonce lease issued");
        NonceLease {
            manager: self,
            nonce,
            committed: false,
        }
    }

    /// Internal: pop the smallest free nonce, or fall back to the
    /// monotonic counter. Always updates the high-water mark.
    fn claim(&self) -> u64 {
        if let Some(reused) = {
            let mut fl = self.free_list.lock().expect("nonce free-list poisoned");
            fl.pop_first()
        } {
            debug!(signer = %self.signer, nonce = reused, "nonce reissued from free list");
            // High-water already covers this value — no fetch_max needed.
            return reused;
        }
        let n = self.next.fetch_add(1, Ordering::AcqRel);
        // Update high-water mark monotonically. Using fetch_max so
        // racing threads collapse to the same final value without a
        // lost-update window.
        self.high_water.fetch_max(n + 1, Ordering::AcqRel);
        n
    }

    /// Internal: return a nonce to the free-list. Called from
    /// [`NonceLease::drop`] when the lease was not committed.
    fn release(&self, nonce: u64) {
        let mut fl = self.free_list.lock().expect("nonce free-list poisoned");
        let inserted = fl.insert(nonce);
        if inserted {
            debug!(signer = %self.signer, nonce, free = fl.len(), "nonce released to free list");
        } else {
            // Defence in depth: a double-release would silently
            // dedup, but a duplicate insert means the lease was
            // somehow released twice — surface in logs.
            warn!(signer = %self.signer, nonce, "nonce released twice — ignored");
        }
    }

    /// Re-fetch the on-chain nonce and adopt it — but never go
    /// backwards past an already-issued value.
    ///
    /// Concretely: `next` is set to `max(on_chain, high_water)`. If
    /// the chain lags our local view (common: our own pending txs
    /// haven't landed yet), we keep the local nonce. If the chain
    /// jumped ahead (cold start, manual transfer from the same key),
    /// we adopt the chain value and prune the free-list of any
    /// stale entries below the new floor.
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

        // Drop any free-list entries the chain has now consumed —
        // they correspond to nonces the chain already accepted from
        // some other source (e.g. a manual transfer that filled a
        // gap, or a duplicate signer). Reissuing them would cause
        // `nonce too low`.
        {
            let mut fl = self.free_list.lock().expect("nonce free-list poisoned");
            fl.retain(|&n| n >= on_chain);
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

    /// Detect a stuck gap: local pointer is `tolerance` or more ahead
    /// of the on-chain pending nonce, suggesting one or more
    /// in-flight txs are not progressing. Caller decides what to do
    /// (typically: rebroadcast with a fee bump, or restart).
    ///
    /// `tolerance` is in nonces, not blocks — a sensible default is
    /// `2`, matching the typical mempool TTL.
    pub async fn check_gap<P, T>(&self, provider: &P, tolerance: u64) -> Result<(), NonceError>
    where
        P: Provider<T>,
        T: alloy::transports::Transport + Clone,
    {
        let on_chain = provider
            .get_transaction_count(self.signer)
            .block_id(BlockId::Number(BlockNumberOrTag::Pending))
            .await?;
        let local = self.next.load(Ordering::Acquire);
        if local > on_chain + tolerance {
            return Err(NonceError::Gap { local, on_chain });
        }
        Ok(())
    }
}

/// RAII handle representing a nonce held by a caller mid-broadcast.
///
/// Drop without [`commit`] returns the nonce to the manager's
/// free-list. Drop after [`commit`] is a no-op (the nonce stays
/// permanently consumed). The lease intentionally borrows the manager
/// rather than holding an Arc so it cannot outlive the manager and
/// silently lose the release on drop.
#[must_use = "a nonce lease must be committed or dropped to release"]
pub struct NonceLease<'a> {
    manager: &'a NonceManager,
    nonce: u64,
    committed: bool,
}

impl NonceLease<'_> {
    /// The nonce held by this lease.
    pub fn nonce(&self) -> u64 {
        self.nonce
    }

    /// Mark the lease as committed: the broadcast succeeded and the
    /// nonce is now permanently consumed. Subsequent drop is a no-op.
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl std::fmt::Debug for NonceLease<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NonceLease")
            .field("nonce", &self.nonce)
            .field("committed", &self.committed)
            .finish()
    }
}

impl Drop for NonceLease<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.manager.release(self.nonce);
        }
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
        let l1 = m.next();
        assert_eq!(l1.nonce(), 7);
        l1.commit();
        let l2 = m.next();
        assert_eq!(l2.nonce(), 8);
        l2.commit();
        let l3 = m.next();
        assert_eq!(l3.nonce(), 9);
        l3.commit();
        assert_eq!(m.current(), 10);
        assert_eq!(m.high_water(), 10);
    }

    #[test]
    fn high_water_tracks_max_issued() {
        let m = NonceManager::new(signer(), 0);
        for _ in 0..5 {
            m.next().commit();
        }
        assert_eq!(m.high_water(), 5);
    }

    #[test]
    fn lease_dropped_without_commit_returns_nonce_to_free_list() {
        let m = NonceManager::new(signer(), 0);
        {
            let _lease = m.next();
            // Lease dropped here without commit.
        }
        assert_eq!(m.free_list_snapshot(), vec![0]);

        // Next claim reuses the released nonce.
        let lease = m.next();
        assert_eq!(lease.nonce(), 0, "must reuse the released nonce");
        assert!(m.free_list_snapshot().is_empty());
        lease.commit();
    }

    #[test]
    fn committed_lease_never_returns_to_free_list() {
        let m = NonceManager::new(signer(), 0);
        let l = m.next();
        assert_eq!(l.nonce(), 0);
        l.commit();
        // Drop happens here — must NOT release.
        assert!(m.free_list_snapshot().is_empty());

        // Subsequent next() bumps to 1, doesn't reuse 0.
        let l2 = m.next();
        assert_eq!(l2.nonce(), 1);
        l2.commit();
    }

    #[test]
    fn next_commit_next_drop_next_reuses_dropped_slot() {
        let m = NonceManager::new(signer(), 0);

        let a = m.next();
        assert_eq!(a.nonce(), 0);
        a.commit();

        let b = m.next();
        assert_eq!(b.nonce(), 1);
        // Drop b without commit — releases nonce 1.
        drop(b);

        let c = m.next();
        assert_eq!(c.nonce(), 1, "dropped slot must be reused");
        c.commit();
    }

    #[test]
    fn free_list_pops_smallest_first() {
        let m = NonceManager::new(signer(), 0);
        let a = m.next();
        let b = m.next();
        let c = m.next();
        // Drop b (1) and c (2) without commit, then a (0) commits.
        drop(c);
        drop(b);
        a.commit();

        assert_eq!(m.free_list_snapshot(), vec![1, 2]);
        let l = m.next();
        assert_eq!(l.nonce(), 1, "smallest free nonce served first");
        l.commit();
        let l = m.next();
        assert_eq!(l.nonce(), 2);
        l.commit();
        // Free list drained — next claim bumps the counter.
        let l = m.next();
        assert_eq!(l.nonce(), 3);
        l.commit();
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
                    let lease = m.next();
                    local.push(lease.nonce());
                    lease.commit();
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

    #[test]
    fn nonce_error_gap_is_constructible_and_displayed() {
        let err = NonceError::Gap {
            local: 47,
            on_chain: 42,
        };
        let s = format!("{err}");
        assert!(s.contains("47"), "{s}");
        assert!(s.contains("42"), "{s}");
    }
}
