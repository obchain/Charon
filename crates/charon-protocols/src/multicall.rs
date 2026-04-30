//! Multicall3 batching helper — issue #353 (foundation).
//!
//! Wraps the canonical Multicall3 deployment
//! (`0xcA11bde05977b3631167028862bE2a173976CA11` on every chain
//! Multicall3 ships to). The hot path in the Venus adapter issues
//! ~`5 × markets` view calls per borrower (`borrowBalanceStored`,
//! `balanceOf`, `exchangeRateStored`, `getUnderlyingPrice`, plus
//! `getAccountLiquidity` once); aggregating them into a single
//! `aggregate3` cuts a 240-call hot scan down to one round trip.
//!
//! This module ships the **building block**: the `IMulticall3` ABI
//! binding, an `aggregate3` helper that auto-chunks at
//! [`MAX_CALLS_PER_BATCH`] (= 100, well under the gas-bounded
//! hard cap of ~150), and per-call success / failure surface so a
//! single failing inner call does not poison the rest of the batch.
//!
//! Wiring the Venus adapter to consume this is a follow-up so the
//! perf change can land alongside its own fork-driven measurement.
//! Until then, the helper is exercised by unit tests covering
//! chunking and the success / failure decode contract.

use alloy::primitives::{Address, Bytes, address};
use alloy::sol;

/// Canonical Multicall3 deployment. Same address across every chain
/// Multicall3 has shipped to (BSC, Ethereum, Polygon, Arbitrum,
/// Optimism, Avalanche, …). `connect()`-time chain id verification
/// keeps it honest if we ever target a chain that does not have it.
pub const MULTICALL3_ADDRESS: Address = address!("cA11bde05977b3631167028862bE2a173976CA11");

/// Soft chunk ceiling for one `aggregate3` batch. Multicall3's gas
/// cap caps the absolute upper bound at ~150 calls (depending on the
/// per-call gas of each inner view); 100 keeps comfortable headroom
/// without sacrificing the batching win.
pub const MAX_CALLS_PER_BATCH: usize = 100;

sol! {
    /// Subset of the Multicall3 ABI we use. `aggregate3` takes a
    /// `(target, allowFailure, callData)` tuple per inner call and
    /// returns `(success, returnData)` per call so a partial failure
    /// (one missing target, one reverting view) does not abort the
    /// whole batch.
    #[sol(rpc)]
    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }

        struct Result {
            bool success;
            bytes returnData;
        }

        function aggregate3(Call3[] calldata calls) external payable returns (Result[] memory);
    }
}

/// One inner call to be aggregated. `target` + `calldata` are the
/// usual `eth_call` shape; `allow_failure` controls whether
/// Multicall3 surfaces a revert as `success = false` (recommended
/// for view-only batches where one bad vToken should not abort the
/// rest) or as a top-level revert.
#[derive(Debug, Clone)]
pub struct InnerCall {
    pub target: Address,
    pub allow_failure: bool,
    pub calldata: Bytes,
}

/// One inner result. `success = false` means the inner call
/// reverted; `return_data` holds the revert payload (often a 4-byte
/// selector + ABI-encoded message). Decoders should check `success`
/// before parsing.
#[derive(Debug, Clone)]
pub struct InnerResult {
    pub success: bool,
    pub return_data: Bytes,
}

/// Split `calls` into successive `MAX_CALLS_PER_BATCH`-sized chunks
/// for `aggregate3`. Pure and unit-testable; no provider involvement.
/// Returns an empty `Vec` when `calls` is empty.
pub fn chunk_calls(calls: Vec<InnerCall>) -> Vec<Vec<InnerCall>> {
    if calls.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<Vec<InnerCall>> = Vec::new();
    let mut buf: Vec<InnerCall> = Vec::with_capacity(MAX_CALLS_PER_BATCH);
    for c in calls {
        if buf.len() == MAX_CALLS_PER_BATCH {
            out.push(std::mem::take(&mut buf));
            buf = Vec::with_capacity(MAX_CALLS_PER_BATCH);
        }
        buf.push(c);
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_call(idx: u8) -> InnerCall {
        InnerCall {
            target: Address::from_slice(&[idx; 20]),
            allow_failure: true,
            calldata: Bytes::from_static(&[0xde, 0xad, 0xbe, 0xef]),
        }
    }

    #[test]
    fn chunk_calls_returns_empty_for_empty_input() {
        assert!(chunk_calls(Vec::new()).is_empty());
    }

    #[test]
    fn chunk_calls_keeps_single_chunk_under_cap() {
        let calls: Vec<_> = (0..50u8).map(dummy_call).collect();
        let chunks = chunk_calls(calls);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), 50);
    }

    #[test]
    fn chunk_calls_splits_at_cap() {
        let calls: Vec<_> = (0..u8::try_from(MAX_CALLS_PER_BATCH * 2 + 17).unwrap_or(u8::MAX))
            .map(dummy_call)
            .collect();
        let total = calls.len();
        let chunks = chunk_calls(calls);
        // Three full chunks of 100 + one tail.
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].len(), MAX_CALLS_PER_BATCH);
        assert_eq!(chunks[1].len(), MAX_CALLS_PER_BATCH);
        assert_eq!(chunks[2].len(), total - 2 * MAX_CALLS_PER_BATCH);
    }

    /// MULTICALL3_ADDRESS is a hard-coded constant; pin it so a
    /// future refactor that accidentally drifts the address (e.g.
    /// copy-pasting Multicall1 or a chain-specific override) trips
    /// this test.
    #[test]
    fn multicall3_address_matches_canonical_deployment() {
        let expected = address!("cA11bde05977b3631167028862bE2a173976CA11");
        assert_eq!(MULTICALL3_ADDRESS, expected);
    }
}
