//! Multi-liquidation batcher.
//!
//! Groups [`LiquidationOpportunity`] records that land in the same
//! block window and encodes them into one `batchExecute(...)` call on
//! `CharonLiquidator.sol`. Saves one tx per extra opportunity — the
//! flash-loan fee on each borrow is still paid, but the base tx cost
//! (21000 gas + signature verify + calldata base) amortises across the
//! whole batch.
//!
//! The batcher is a **planner**, not an executor. It returns structured
//! [`LiquidationBatch`] values; a downstream caller (CLI pipeline, in a
//! later PR) builds the tx via [`TxBuilder`](crate::TxBuilder) with the
//! calldata produced here and broadcasts it through
//! [`Submitter`](crate::Submitter).
//!
//! v0.1 scope: Venus on BNB Chain only. The planner rejects any input
//! whose `chain_id` is not [`BSC_CHAIN_ID`]; cross-chain partitioning is
//! out of scope until the bot grows a second protocol target.
//!
//! Current heuristic:
//! - All opportunities must share `chain_id == 56` (BSC)
//! - Preserve profit-desc ordering supplied by the caller
//! - Cap per batch at [`MAX_BATCH_SIZE`] = 3 — PRD default; the on-chain
//!   cap is [`SOLIDITY_MAX_BATCH_SIZE`] = 10 and the planner enforces
//!   both ceilings
//! - Only emit a batch if it contains ≥ 2 opportunities; a 1-item
//!   "batch" is cheaper as a plain `executeLiquidation` call (skips the
//!   array length word + loop overhead)

use alloy::primitives::{Bytes, U256};
use alloy::providers::Provider;
use alloy::sol;
use alloy::sol_types::SolCall;
use charon_core::{LiquidationOpportunity, LiquidationParams};
use thiserror::Error;
use tracing::debug;

use crate::simulation::Simulator;

/// Matches `MAX_BATCH_SIZE` in `CharonLiquidator.sol`. The Solidity
/// ceiling is 10 ([`SOLIDITY_MAX_BATCH_SIZE`]); the Rust default is
/// smaller to keep gas estimates predictable and mirror the PRD's
/// suggested batch size.
pub const MAX_BATCH_SIZE: usize = 3;

/// Hard ceiling enforced on the Solidity side (`CharonLiquidator.sol`
/// `MAX_BATCH_SIZE`). Any batch whose length exceeds this constant
/// would be rejected on-chain by `require(n <= MAX_BATCH_SIZE, ...)`;
/// the encoder rejects it earlier so a compromised or misconfigured
/// caller cannot waste a tx or burn gas for guaranteed revert.
pub const SOLIDITY_MAX_BATCH_SIZE: usize = 10;

/// BNB Chain (v0.1 only). Cross-chain partitioning is deferred until a
/// second protocol target lands; until then any non-BSC opportunity is
/// a programming error and the planner surfaces it as such.
pub const BSC_CHAIN_ID: u64 = 56;

// On-chain struct + batch entrypoint. Must stay in lockstep with the
// Solidity source — the selector test pins the canonical keccak256 of
// the function signature, so any drift in the struct shape or the
// function name breaks the test before it reaches mainnet.
//
// Shape mirrors `CharonLiquidator.sol :: LiquidationParams` including the
// trailing `uint24 swapPoolFee` field added in the cold-wallet / vBNB
// port. The selector test below pins the canonical keccak256 so any
// further drift in field order or count reliably breaks CI before it
// reaches mainnet.
sol! {
    /// Solidity-side `LiquidationParams` — same shape as in
    /// [`crate::builder::CharonLiquidationParams`], redeclared here so
    /// the batch encoder is self-contained.
    #[derive(Debug)]
    struct BatchParams {
        uint8 protocolId;
        address borrower;
        address debtToken;
        address collateralToken;
        address debtVToken;
        address collateralVToken;
        uint256 repayAmount;
        uint256 minSwapOut;
        uint24 swapPoolFee;
    }

    /// `batchExecute(LiquidationParams[])` entry on `CharonLiquidator`.
    interface ICharonBatch {
        function batchExecute(BatchParams[] calldata items) external;
    }
}

const PROTOCOL_VENUS: u8 = 3;

/// Typed error surface for the public batcher API. Keeps `anyhow` out
/// of the library boundary so downstream callers can pattern-match on
/// failure modes and surface them as domain errors rather than opaque
/// `Result<_, anyhow::Error>` blobs.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BatcherError {
    /// Caller supplied a `params` slice whose length does not equal
    /// `batch.opportunities.len()`. Zipping proceeds pairwise, so any
    /// mismatch corrupts the mapping between opportunities and their
    /// protocol parameters.
    #[error("batcher: params/opportunities length mismatch (params={params}, opps={opps})")]
    ParamLengthMismatch {
        /// Length of the `params` slice supplied to `encode_calldata`.
        params: usize,
        /// Length of `batch.opportunities`.
        opps: usize,
    },

    /// Batch length exceeds the on-chain `MAX_BATCH_SIZE` in
    /// `CharonLiquidator.sol`. See [`SOLIDITY_MAX_BATCH_SIZE`].
    #[error("batcher: batch too large ({len} items, on-chain limit {limit})")]
    BatchTooLarge {
        /// Actual batch length.
        len: usize,
        /// Hard ceiling on the Solidity side.
        limit: usize,
    },

    /// Batch contains an opportunity whose `chain_id` does not match
    /// the v0.1 BSC-only scope. Collapses the previous
    /// cross-chain `HashMap` into a single explicit guard.
    #[error("batcher: unsupported chain_id {got}, only {expected} (BSC) is supported in v0.1")]
    UnsupportedChain {
        /// Chain id observed on the offending opportunity.
        got: u64,
        /// The only chain id accepted in v0.1.
        expected: u64,
    },

    /// The corresponding [`LiquidationParams`] variant is not handled
    /// by this batcher. Mirrors
    /// [`BuilderError::UnsupportedProtocol`](crate::BuilderError::UnsupportedProtocol)
    /// — `LiquidationParams` is `#[non_exhaustive]`, so a wildcard arm
    /// is required even though v0.1 only surfaces `Venus`. Payload is
    /// the `Debug` rendering so logs can identify which protocol
    /// adapter is still pending batcher support.
    #[error("batcher: unsupported liquidation protocol: {0}")]
    UnsupportedProtocol(String),

    /// The swap route attached to an opportunity lacks a `pool_fee`.
    /// The on-chain `CharonLiquidator.executeOperation` routes the
    /// swap through PancakeSwap V3 at a caller-supplied fee tier and
    /// reverts with `"!swapPoolFee"` if the tier is zero or missing;
    /// the encoder rejects the calldata earlier rather than burn gas
    /// for a guaranteed revert.
    #[error(
        "batcher: missing pool_fee on swap route for borrower {borrower:#x} \
         (fee-less routes are not supported by CharonLiquidator)"
    )]
    MissingPoolFee {
        /// Borrower address on the opportunity that lacked a pool fee.
        borrower: alloy::primitives::Address,
    },

    /// The supplied `pool_fee` does not fit in the on-chain `uint24`
    /// slot. The Solidity struct declares `swapPoolFee` as `uint24`
    /// (PancakeSwap V3's fee-tier domain maxes at 10_000), so any
    /// value greater than 2^24 - 1 is either a programming error in
    /// the router or a sign the off-chain type should be tightened.
    #[error(
        "batcher: pool_fee {got} does not fit in uint24 (on-chain limit {limit}) \
         for borrower {borrower:#x}"
    )]
    PoolFeeOutOfRange {
        /// Borrower address on the offending opportunity.
        borrower: alloy::primitives::Address,
        /// Fee that overflowed the `uint24` slot.
        got: u32,
        /// Largest value representable as `uint24` (2^24 - 1).
        limit: u32,
    },

    /// `SolCall::abi_encode` returned an error. Wrapped so the caller
    /// does not depend on `alloy`'s internal error types.
    #[error("batcher: ABI encoding failed: {0}")]
    AbiEncodeError(String),

    /// `eth_call` simulation of the batch reverted. Carries the
    /// underlying revert string from the node so the caller can
    /// log it and drop the batch. This is the failure path of the
    /// type-level simulate gate — see [`UnsimulatedBatchCalldata`]
    /// and [`Batcher::simulate`].
    #[error("batcher: batch simulation reverted: {0}")]
    SimulationFailed(String),
}

/// Calldata returned by [`Batcher::encode_calldata`].
///
/// Wraps the raw ABI-encoded bytes so they cannot reach a submitter
/// without first being promoted to [`SimulatedBatchCalldata`] via
/// [`Batcher::simulate`]. The wrapper is deliberately opaque: no
/// `Deref`, no `AsRef<Bytes>`, no public `.0`. The only paths into
/// this type are the encoder and its tests; the only path out is the
/// simulate gate. This makes the CLAUDE.md invariant "no broadcast
/// without a passing `eth_call`" a compile-time guarantee for the
/// batch path, mirroring the `UnverifiedPreSigned` guard on the
/// mempool pre-sign path (see `charon-scanner::mempool`).
#[derive(Debug, Clone)]
pub struct UnsimulatedBatchCalldata(Bytes);

impl UnsimulatedBatchCalldata {
    /// Borrow the inner bytes for simulation purposes **only**. The
    /// simulate gate inside the batcher is the one caller — external
    /// code must go through [`Batcher::simulate`].
    ///
    /// This is `pub(crate)` instead of `pub` so a broadcaster written
    /// against `charon-executor` cannot reach the raw calldata without
    /// passing through `Batcher::simulate` first. `#[cfg(test)]`
    /// tests in this module access it through the same accessor.
    pub(crate) fn as_bytes(&self) -> &Bytes {
        &self.0
    }

    /// Length of the inner calldata in bytes. Useful for telemetry
    /// (fee estimation, calldata-budget checks) at sites that do not
    /// need to read the bytes themselves.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True if the inner calldata is empty. Paired with
    /// [`Self::len`] so the opaque wrapper can still satisfy the
    /// standard length/empty contract without exposing the buffer.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Calldata that has passed the batcher's `eth_call` simulation gate.
///
/// Produced exclusively by [`Batcher::simulate`]. A downstream
/// submitter that accepts only `SimulatedBatchCalldata` cannot be
/// handed raw encoder output by mistake — the type system refuses.
/// Consumes the inner bytes on request via [`Self::into_bytes`] so
/// the broadcaster gets an owned `Bytes` for the final
/// `eth_sendRawTransaction` without paying a copy.
#[derive(Debug, Clone)]
pub struct SimulatedBatchCalldata(Bytes);

impl SimulatedBatchCalldata {
    /// Consume the wrapper and return the inner calldata. Intended
    /// for the broadcaster call site once batch submission is wired
    /// into the CLI pipeline.
    pub fn into_bytes(self) -> Bytes {
        self.0
    }

    /// Borrow the inner bytes without consuming the wrapper.
    pub fn as_bytes(&self) -> &Bytes {
        &self.0
    }

    /// Length of the inner calldata in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True if the inner calldata is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// One batch ready for `TxBuilder` to wrap into an EIP-1559 transaction.
#[derive(Debug, Clone)]
pub struct LiquidationBatch {
    /// Chain the opportunities share (always `BSC_CHAIN_ID` in v0.1).
    pub chain_id: u64,
    /// Opportunities in profit-desc order, length in `[2, MAX_BATCH_SIZE]`.
    pub opportunities: Vec<LiquidationOpportunity>,
    /// Sum of `net_profit_wei` across the batch — used by the caller
    /// to rank batches against single-opportunity txs. Kept in wei
    /// (the same domain as [`LiquidationOpportunity::net_profit_wei`])
    /// so ranking never crosses a USD-cent boundary where rounding
    /// could flip the comparison.
    pub total_net_profit_wei: U256,
}

/// Stateless planner — construct once per process.
#[derive(Debug, Clone, Copy)]
pub struct Batcher {
    max_batch_size: usize,
}

impl Batcher {
    pub fn new(max_batch_size: usize) -> Self {
        // Clamp to [1, SOLIDITY_MAX_BATCH_SIZE] so a misconfigured
        // caller cannot produce a batch that would be rejected on-chain
        // after the tx was built and signed.
        let clamped = max_batch_size.clamp(1, SOLIDITY_MAX_BATCH_SIZE);
        Self {
            max_batch_size: clamped,
        }
    }

    pub fn with_default_size() -> Self {
        Self::new(MAX_BATCH_SIZE)
    }

    /// Chunk `opportunities` into BSC-only batches.
    ///
    /// Returns [`BatcherError::UnsupportedChain`] if any input is not
    /// on BSC — v0.1 does not partition across chains. Input order is
    /// preserved (caller should supply profit-desc; the batcher does
    /// not re-rank). Single-opportunity chunks are omitted — they
    /// belong on the plain `executeLiquidation` path, not `batchExecute`.
    pub fn plan(
        &self,
        opportunities: Vec<LiquidationOpportunity>,
    ) -> Result<Vec<LiquidationBatch>, BatcherError> {
        // Single-chain guard: v0.1 is BSC-only. Rejecting here rather
        // than silently filtering forces the caller to surface the
        // misconfiguration before we burn RPC round-trips on
        // ineligible opportunities.
        for opp in &opportunities {
            if opp.position.chain_id != BSC_CHAIN_ID {
                return Err(BatcherError::UnsupportedChain {
                    got: opp.position.chain_id,
                    expected: BSC_CHAIN_ID,
                });
            }
        }

        let mut out = Vec::new();
        for chunk in opportunities.chunks(self.max_batch_size) {
            if chunk.len() < 2 {
                continue;
            }
            let total_net_profit_wei = chunk
                .iter()
                .map(|o| o.net_profit_wei)
                .fold(U256::ZERO, U256::saturating_add);
            out.push(LiquidationBatch {
                chain_id: BSC_CHAIN_ID,
                opportunities: chunk.to_vec(),
                total_net_profit_wei,
            });
        }
        debug!(batch_count = out.len(), "batcher planned");
        Ok(out)
    }

    /// ABI-encode a `batchExecute(LiquidationParams[])` call.
    ///
    /// Each opportunity needs its corresponding
    /// [`LiquidationParams`] (produced upstream by
    /// `LendingProtocol::get_liquidation_params`). The caller supplies
    /// them as a parallel slice so the batcher never has to know how a
    /// given protocol derives its vToken addresses.
    ///
    /// # Safety
    ///
    /// The returned [`UnsimulatedBatchCalldata`] is a compile-time
    /// guard enforcing the CLAUDE.md invariant that every
    /// liquidation tx passes an `eth_call` gate before broadcast.
    /// The wrapper cannot be unpacked into raw bytes by external
    /// code; the only promotion path is [`Batcher::simulate`], which
    /// runs the calldata through [`Simulator::simulate`] and returns
    /// [`SimulatedBatchCalldata`] on success. A broadcaster written
    /// against this crate that accepts only `SimulatedBatchCalldata`
    /// therefore cannot be handed raw encoder output by mistake.
    ///
    /// The simulator catches protocol-level reverts (insufficient
    /// collateral, stale oracle, closed market) that the planner
    /// cannot see from off-chain data alone. Skipping simulation is
    /// a bypass of the last line of defense and is never acceptable
    /// in production code paths — the type system now makes that
    /// bypass a compile error rather than a doc-comment aspiration.
    pub fn encode_calldata(
        &self,
        batch: &LiquidationBatch,
        params: &[LiquidationParams],
    ) -> Result<UnsimulatedBatchCalldata, BatcherError> {
        let opps = batch.opportunities.len();
        if params.len() != opps {
            return Err(BatcherError::ParamLengthMismatch {
                params: params.len(),
                opps,
            });
        }
        if opps > SOLIDITY_MAX_BATCH_SIZE {
            return Err(BatcherError::BatchTooLarge {
                len: opps,
                limit: SOLIDITY_MAX_BATCH_SIZE,
            });
        }

        // Largest value representable as `uint24`: 2^24 - 1. Any
        // larger fee lands outside the on-chain slot and would either
        // truncate silently (our Rust domain is `u32`) or revert on
        // ABI-encode. Keep the constant local so the error message
        // and the guard never drift.
        const UINT24_MAX: u32 = (1u32 << 24) - 1;

        let mut items = Vec::with_capacity(opps);
        for (opp, params) in batch.opportunities.iter().zip(params.iter()) {
            // Exhaustive match with a wildcard arm. `LiquidationParams`
            // is `#[non_exhaustive]` at the enum level, so a refutable
            // `let LiquidationParams::Venus { .. } = params;` outside
            // the defining crate would fail to compile. Mirrors the
            // same discipline as `TxBuilder::encode_calldata` so the
            // two encoders behave identically when a new variant
            // (AaveV3, Compound, Morpho…) lands in `charon-core` and
            // reaches the batcher before batch support has been
            // taught to emit its calldata.
            let (borrower, collateral_vtoken, debt_vtoken, repay_amount) = match params {
                LiquidationParams::Venus {
                    borrower,
                    collateral_vtoken,
                    debt_vtoken,
                    repay_amount,
                } => (borrower, collateral_vtoken, debt_vtoken, repay_amount),
                other => {
                    return Err(BatcherError::UnsupportedProtocol(format!("{other:?}")));
                }
            };

            // PancakeSwap V3 fee tier. `None` means a fee-less route
            // (Curve stable pool, Balancer V2, …) which the on-chain
            // `CharonLiquidator` does not support: it calls
            // `ISwapRouter.exactInputSingle` unconditionally and
            // requires `swapPoolFee > 0` inside `_initiateFlashLoan`.
            // Refuse the calldata here rather than emit a tx that
            // would revert with `"!swapPoolFee"` on-chain.
            let fee_u32 = opp.swap_route.pool_fee.ok_or(BatcherError::MissingPoolFee {
                borrower: *borrower,
            })?;
            if fee_u32 > UINT24_MAX {
                return Err(BatcherError::PoolFeeOutOfRange {
                    borrower: *borrower,
                    got: fee_u32,
                    limit: UINT24_MAX,
                });
            }
            // `alloy::primitives::aliases::U24` is the sol! target
            // type; it accepts `u32` via `from` on values that fit.
            let swap_pool_fee = alloy::primitives::aliases::U24::from(fee_u32);

            items.push(BatchParams {
                protocolId: PROTOCOL_VENUS,
                borrower: *borrower,
                debtToken: opp.position.debt_token,
                collateralToken: opp.position.collateral_token,
                debtVToken: *debt_vtoken,
                collateralVToken: *collateral_vtoken,
                repayAmount: *repay_amount,
                minSwapOut: opp.swap_route.min_amount_out,
                swapPoolFee: swap_pool_fee,
            });
        }

        let call = ICharonBatch::batchExecuteCall { items };
        let bytes: Bytes = call.abi_encode().into();
        debug!(
            items = opps,
            calldata_len = bytes.len(),
            chain_id = batch.chain_id,
            "batch calldata encoded"
        );
        Ok(UnsimulatedBatchCalldata(bytes))
    }

    /// Run a batch calldata through the `eth_call` simulation gate
    /// and promote it to [`SimulatedBatchCalldata`].
    ///
    /// Consumes the [`UnsimulatedBatchCalldata`] so the same buffer
    /// cannot be simulated twice and reused without going through
    /// the gate again (a resubmission of stale calldata after
    /// intervening block state change is a silent profit regression
    /// the gate would otherwise miss). On simulation failure the
    /// revert string is surfaced via [`BatcherError::SimulationFailed`]
    /// and the caller drops the batch.
    ///
    /// The `simulator` argument carries the sender and liquidator
    /// addresses; pass a freshly constructed [`Simulator`] or one
    /// already built by the submitter wiring. The `provider` is the
    /// same alloy `Provider` used by the scanner/executor — no
    /// bespoke transport plumbing.
    ///
    /// `gas_limit` must match (or exceed) what the real broadcast
    /// will use. Main's [`Simulator::simulate`] takes this explicitly
    /// so the simulation cannot under-provision gas relative to the
    /// broadcast and pass here only to revert on-chain as
    /// out-of-gas. A batch call uses roughly
    /// `single_liq_gas * n + calldata_overhead`; the caller is
    /// expected to size it using [`GasOracle::estimate_gas_units`]
    /// on the same calldata.
    pub async fn simulate<P, T>(
        &self,
        provider: &P,
        simulator: &Simulator,
        calldata: UnsimulatedBatchCalldata,
        gas_limit: u64,
    ) -> Result<SimulatedBatchCalldata, BatcherError>
    where
        P: Provider<T>,
        T: alloy::transports::Transport + Clone,
    {
        simulator
            .simulate(provider, calldata.as_bytes().clone(), gas_limit)
            .await
            .map_err(|err| BatcherError::SimulationFailed(format!("{err:#}")))?;
        Ok(SimulatedBatchCalldata(calldata.0))
    }
}

impl Default for Batcher {
    fn default() -> Self {
        Self::with_default_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, U256, address, keccak256};
    use charon_core::{FlashLoanSource, Position, ProtocolId, SwapRoute};

    fn mk_opp(chain_id: u64, net_wei: u64, borrower_byte: u8) -> LiquidationOpportunity {
        let mut bytes = [0u8; 20];
        bytes[19] = borrower_byte;
        LiquidationOpportunity {
            position: Position {
                protocol: ProtocolId::Venus,
                chain_id,
                borrower: Address::from(bytes),
                collateral_token: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                debt_token: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                collateral_amount: U256::from(1_000u64),
                debt_amount: U256::from(500u64),
                health_factor: U256::ZERO,
                liquidation_bonus_bps: 1_000,
            },
            debt_to_repay: U256::from(250u64),
            expected_collateral_out: U256::from(275u64),
            flash_source: FlashLoanSource::AaveV3,
            swap_route: SwapRoute {
                token_in: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                token_out: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                amount_in: U256::from(275u64),
                min_amount_out: U256::from(260u64),
                pool_fee: Some(3_000),
            },
            net_profit_wei: U256::from(net_wei),
        }
    }

    fn mk_params(borrower_byte: u8) -> LiquidationParams {
        let mut bytes = [0u8; 20];
        bytes[19] = borrower_byte;
        LiquidationParams::Venus {
            borrower: Address::from(bytes),
            collateral_vtoken: address!("cccccccccccccccccccccccccccccccccccccccc"),
            debt_vtoken: address!("dddddddddddddddddddddddddddddddddddddddd"),
            repay_amount: U256::from(250u64),
        }
    }

    #[test]
    fn single_opportunity_does_not_become_a_batch() {
        let out = Batcher::with_default_size()
            .plan(vec![mk_opp(56, 100, 1)])
            .expect("plan");
        assert!(out.is_empty(), "1-item input should yield no batches");
    }

    #[test]
    fn same_chain_groups_into_one_batch() {
        let out = Batcher::with_default_size()
            .plan(vec![
                mk_opp(56, 300, 1),
                mk_opp(56, 200, 2),
                mk_opp(56, 100, 3),
            ])
            .expect("plan");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chain_id, 56);
        assert_eq!(out[0].opportunities.len(), 3);
        assert_eq!(out[0].total_net_profit_wei, U256::from(600u64));
    }

    /// v0.1 is BSC-only. A non-BSC opportunity is a programming error,
    /// not a quiet partitioning condition, so the planner rejects it.
    #[test]
    fn plan_rejects_non_bsc_chain_id() {
        let err = Batcher::with_default_size()
            .plan(vec![mk_opp(56, 100, 1), mk_opp(1, 100, 2)])
            .expect_err("non-BSC must error");
        match err {
            BatcherError::UnsupportedChain { got, expected } => {
                assert_eq!(got, 1);
                assert_eq!(expected, 56);
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    /// All-BSC input of length > 1 planned normally — confirms the
    /// single-chain guard does not reject the happy path.
    #[test]
    fn assert_single_chain() {
        let out = Batcher::with_default_size()
            .plan(vec![mk_opp(56, 100, 1), mk_opp(56, 100, 2)])
            .expect("plan");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chain_id, 56);
    }

    #[test]
    fn batches_split_when_group_exceeds_max_size() {
        // Size 2 → 5 opps → chunks of [2, 2, 1]; the trailing 1 is
        // dropped because it's not a real batch.
        let b = Batcher::new(2);
        let out = b
            .plan((1..=5).map(|i| mk_opp(56, 100, i)).collect())
            .expect("plan");
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|b| b.opportunities.len() == 2));
    }

    /// `new()` must clamp a caller-supplied size at the Solidity cap —
    /// otherwise `plan` could emit a batch that `batchExecute` rejects
    /// with "batch too large" after the tx is already signed.
    #[test]
    fn new_clamps_max_batch_size_to_solidity_cap() {
        let b = Batcher::new(99);
        let opps: Vec<_> = (1u8..=12).map(|i| mk_opp(56, 100, i)).collect();
        let out = b.plan(opps).expect("plan");
        // 12 opps chunked at 10 → [10, 2]; both ≥ 2 so both survive.
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].opportunities.len(), SOLIDITY_MAX_BATCH_SIZE);
        assert_eq!(out[1].opportunities.len(), 2);
    }

    /// Canonical keccak256 pin: external witness of the batchExecute
    /// selector. If either the function name or the struct shape drifts
    /// from `CharonLiquidator.sol`, this test fails before any tx is
    /// ever built. The signature must exactly mirror the Solidity
    /// declaration — nine tuple fields including the trailing
    /// `uint24 swapPoolFee` that backs the per-opportunity fee-tier
    /// routing in `executeOperation`.
    #[test]
    fn encode_calldata_has_batch_execute_selector() {
        const CANONICAL_SIG: &str = "batchExecute((uint8,address,address,address,address,address,uint256,uint256,uint24)[])";
        let digest = keccak256(CANONICAL_SIG.as_bytes());
        let expected: [u8; 4] = [digest[0], digest[1], digest[2], digest[3]];

        let batch = LiquidationBatch {
            chain_id: 56,
            opportunities: vec![mk_opp(56, 100, 1), mk_opp(56, 200, 2)],
            total_net_profit_wei: U256::from(300u64),
        };
        let params = vec![mk_params(1), mk_params(2)];
        let wrapped = Batcher::with_default_size()
            .encode_calldata(&batch, &params)
            .expect("encode");
        let bytes = wrapped.as_bytes();

        assert_eq!(
            &bytes[..4],
            &expected,
            "calldata selector drifted from canonical batchExecute signature"
        );
        // Belt-and-braces: confirm alloy's derived selector agrees with
        // the hand-computed keccak256, catching macro-side regressions.
        assert_eq!(&bytes[..4], &ICharonBatch::batchExecuteCall::SELECTOR);
    }

    #[test]
    fn encode_calldata_rejects_mismatched_lengths() {
        let batch = LiquidationBatch {
            chain_id: 56,
            opportunities: vec![mk_opp(56, 100, 1), mk_opp(56, 200, 2)],
            total_net_profit_wei: U256::from(300u64),
        };
        let params = vec![mk_params(1)]; // only one
        let err = Batcher::with_default_size()
            .encode_calldata(&batch, &params)
            .expect_err("mismatched lengths must error");
        match err {
            BatcherError::ParamLengthMismatch { params, opps } => {
                assert_eq!(params, 1);
                assert_eq!(opps, 2);
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    /// Direct guard test: a handcrafted oversize batch is rejected by
    /// `encode_calldata` before any abi encoding happens. Bypasses
    /// `plan`'s own clamping because the Solidity ceiling is the
    /// authoritative invariant.
    #[test]
    fn encode_calldata_rejects_oversize_batch() {
        // SOLIDITY_MAX_BATCH_SIZE is 10; the test needs 11 opps with
        // distinct borrower bytes. Use u8::try_from to keep the
        // workspace `cast_possible_truncation` lint satisfied.
        let limit_u8 = u8::try_from(SOLIDITY_MAX_BATCH_SIZE).expect("limit fits in u8");
        let over = limit_u8.checked_add(1).expect("limit + 1 fits in u8");
        let opps: Vec<_> = (1u8..=over).map(|i| mk_opp(56, 100, i)).collect();
        let params: Vec<_> = (1u8..=over).map(mk_params).collect();
        let batch = LiquidationBatch {
            chain_id: 56,
            total_net_profit_wei: U256::ZERO,
            opportunities: opps,
        };
        let err = Batcher::with_default_size()
            .encode_calldata(&batch, &params)
            .expect_err("oversize batch must error");
        match err {
            BatcherError::BatchTooLarge { len, limit } => {
                assert_eq!(len, SOLIDITY_MAX_BATCH_SIZE + 1);
                assert_eq!(limit, SOLIDITY_MAX_BATCH_SIZE);
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    /// A fee-less swap route is a programming error for the Venus /
    /// PancakeSwap V3 pipeline: the on-chain executor requires a
    /// non-zero `swapPoolFee`. Reject at encode time.
    #[test]
    fn encode_calldata_rejects_missing_pool_fee() {
        let mut opp1 = mk_opp(56, 100, 1);
        let opp2 = mk_opp(56, 200, 2);
        opp1.swap_route.pool_fee = None;
        let batch = LiquidationBatch {
            chain_id: 56,
            total_net_profit_wei: U256::from(300u64),
            opportunities: vec![opp1, opp2],
        };
        let params = vec![mk_params(1), mk_params(2)];
        let err = Batcher::with_default_size()
            .encode_calldata(&batch, &params)
            .expect_err("None pool_fee must error");
        match err {
            BatcherError::MissingPoolFee { borrower: _ } => {}
            other => panic!("wrong error variant: {other:?}"),
        }
    }

    /// `swapPoolFee` is `uint24` on-chain; a `u32` that overflows
    /// that slot must be caught here and not silently truncate.
    #[test]
    fn encode_calldata_rejects_pool_fee_out_of_range() {
        let mut opp1 = mk_opp(56, 100, 1);
        let opp2 = mk_opp(56, 200, 2);
        opp1.swap_route.pool_fee = Some(1u32 << 24); // 2^24, one past uint24 max
        let batch = LiquidationBatch {
            chain_id: 56,
            total_net_profit_wei: U256::from(300u64),
            opportunities: vec![opp1, opp2],
        };
        let params = vec![mk_params(1), mk_params(2)];
        let err = Batcher::with_default_size()
            .encode_calldata(&batch, &params)
            .expect_err("overflow pool_fee must error");
        match err {
            BatcherError::PoolFeeOutOfRange { got, limit, .. } => {
                assert_eq!(got, 1u32 << 24);
                assert_eq!(limit, (1u32 << 24) - 1);
            }
            other => panic!("wrong error variant: {other:?}"),
        }
    }
}
