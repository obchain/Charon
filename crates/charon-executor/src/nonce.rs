//! Concurrent nonce manager.
//!
//! One [`NonceManager`] per `(chain × signer)` pair. Holds the next
//! nonce in an `AtomicU64` so the tx builder can hand out sequential
//! values without `&mut` plumbing through the hot path. Optimistic by
//! default — the manager bumps locally on every issue, and the caller
//! re-syncs from chain after a failed broadcast / on startup.
//!
//! Why optimistic: `eth_getTransactionCount(latest)` is one round-trip
//! we don't want on every block. Bumping locally lets multiple flying
//! txs hold contiguous nonces; if one reverts and a gap opens up, a
//! `resync` paves it over.

use std::sync::atomic::{AtomicU64, Ordering};

use alloy::primitives::Address;
use alloy::providers::Provider;
use anyhow::{Context, Result};
use tracing::{debug, info};

/// Tracks the next-to-use nonce for one signer on one chain.
#[derive(Debug)]
pub struct NonceManager {
    signer: Address,
    next: AtomicU64,
}

impl NonceManager {
    /// Build with an explicit starting value. Most callers should use
    /// [`init`] instead, which pulls the on-chain nonce.
    pub fn new(signer: Address, start: u64) -> Self {
        Self {
            signer,
            next: AtomicU64::new(start),
        }
    }

    /// Async constructor: pulls the current `eth_getTransactionCount`
    /// on the `latest` block and stores it as the starting nonce.
    pub async fn init<P, T>(provider: &P, signer: Address) -> Result<Self>
    where
        P: Provider<T>,
        T: alloy::transports::Transport + Clone,
    {
        let nonce = provider
            .get_transaction_count(signer)
            .await
            .with_context(|| format!("nonce manager: getTransactionCount({signer}) failed"))?;
        info!(%signer, nonce, "nonce manager initialised");
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

    /// Atomically claim the next nonce and bump the counter. Two
    /// concurrent calls always return distinct values.
    pub fn next(&self) -> u64 {
        let n = self.next.fetch_add(1, Ordering::AcqRel);
        debug!(signer = %self.signer, nonce = n, "nonce issued");
        n
    }

    /// Re-fetch the on-chain nonce and adopt it as the new local
    /// value. Call this after a tx fails (replaces a stuck nonce) or
    /// on a long idle (catches manual transfers from the same key).
    pub async fn resync<P, T>(&self, provider: &P) -> Result<u64>
    where
        P: Provider<T>,
        T: alloy::transports::Transport + Clone,
    {
        let chain = provider
            .get_transaction_count(self.signer)
            .await
            .with_context(|| {
                format!(
                    "nonce manager: getTransactionCount({}) during resync failed",
                    self.signer
                )
            })?;
        self.next.store(chain, Ordering::Release);
        info!(signer = %self.signer, nonce = chain, "nonce manager resynced");
        Ok(chain)
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
    }
}
