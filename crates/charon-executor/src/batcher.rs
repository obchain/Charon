//! Multi-liquidation batcher.
//!
//! Groups per-chain [`LiquidationOpportunity`] records that land in the
//! same block window and encodes them into one `batchExecute(...)` call
//! on `CharonLiquidator.sol`. Saves one tx per extra opportunity — the
//! flash-loan fee on each borrow is still paid, but the base tx cost
//! (21000 gas + signature verify + calldata base) amortises across the
//! whole batch.
//!
//! The batcher is a **planner**, not an executor. It returns structured
//! [`LiquidationBatch`] values; a downstream caller (CLI pipeline, in a
//! later PR) builds the tx via [`TxBuilder`](crate::TxBuilder) with the
//! calldata produced here.
//!
//! Current heuristic:
//! - Group strictly by `chain_id` (can't batch across chains)
//! - Preserve profit-desc ordering within a group
//! - Cap at `MAX_BATCH_SIZE = 3` — PRD default; the on-chain cap is 10
//! - Only emit a batch if it contains ≥ 2 opportunities; a 1-item
//!   "batch" is cheaper as a plain `executeLiquidation` call (skips the
//!   array length word + loop overhead)

use std::collections::HashMap;

use alloy::primitives::Bytes;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::Result;
use charon_core::{LiquidationOpportunity, LiquidationParams};
use tracing::debug;

/// Matches `MAX_BATCH_SIZE` in `CharonLiquidator.sol`. The Solidity
/// ceiling is 10; keeping the default smaller (3) keeps gas estimates
/// predictable and mirrors the PRD's suggested batch size.
pub const MAX_BATCH_SIZE: usize = 3;

// On-chain struct + batch entrypoint. Must stay in lockstep with the
// Solidity source — the selector test on `ICharonLiquidator` catches
// drift on the single-item path; the batch path is pinned here.
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
    }

    /// `batchExecute(LiquidationParams[])` entry on `CharonLiquidator`.
    interface ICharonBatch {
        function batchExecute(BatchParams[] calldata items) external;
    }
}

const PROTOCOL_VENUS: u8 = 3;

/// One batch ready for `TxBuilder` to wrap into an EIP-1559 transaction.
#[derive(Debug, Clone)]
pub struct LiquidationBatch {
    /// Chain the opportunities share.
    pub chain_id: u64,
    /// Opportunities in profit-desc order, length in `[2, MAX_BATCH_SIZE]`.
    pub opportunities: Vec<LiquidationOpportunity>,
    /// Sum of `net_profit_usd_cents` across the batch — used by the
    /// caller to rank batches against single-opportunity txs.
    pub total_net_usd_cents: u64,
}

/// Stateless planner — construct once per process.
#[derive(Debug, Clone, Copy)]
pub struct Batcher {
    max_batch_size: usize,
}

impl Batcher {
    pub fn new(max_batch_size: usize) -> Self {
        Self {
            max_batch_size: max_batch_size.max(1),
        }
    }

    pub fn with_default_size() -> Self {
        Self::new(MAX_BATCH_SIZE)
    }

    /// Partition `opportunities` into per-chain batches.
    ///
    /// Input order is preserved within each chain group (caller should
    /// supply in profit-desc order; the batcher doesn't re-rank).
    /// Single-opportunity groups are omitted from the output — they
    /// belong on the plain `executeLiquidation` path, not `batchExecute`.
    pub fn plan(&self, opportunities: Vec<LiquidationOpportunity>) -> Vec<LiquidationBatch> {
        let mut by_chain: HashMap<u64, Vec<LiquidationOpportunity>> = HashMap::new();
        for opp in opportunities {
            by_chain.entry(opp.position.chain_id).or_default().push(opp);
        }

        let mut out = Vec::new();
        for (chain_id, group) in by_chain {
            for chunk in group.chunks(self.max_batch_size) {
                if chunk.len() < 2 {
                    continue;
                }
                let total_net_usd_cents = chunk
                    .iter()
                    .map(|o| o.net_profit_usd_cents)
                    .fold(0u64, u64::saturating_add);
                out.push(LiquidationBatch {
                    chain_id,
                    opportunities: chunk.to_vec(),
                    total_net_usd_cents,
                });
            }
        }
        debug!(batch_count = out.len(), "batcher planned");
        out
    }

    /// ABI-encode a `batchExecute(LiquidationParams[])` call.
    ///
    /// Each opportunity needs its corresponding
    /// [`LiquidationParams`] (produced upstream by
    /// `LendingProtocol::get_liquidation_params`). The caller supplies
    /// them as a parallel slice so the batcher never has to know how a
    /// given protocol derives its vToken addresses.
    pub fn encode_calldata(
        &self,
        batch: &LiquidationBatch,
        params: &[LiquidationParams],
    ) -> Result<Bytes> {
        if params.len() != batch.opportunities.len() {
            anyhow::bail!(
                "batcher: params/opportunities length mismatch ({} vs {})",
                params.len(),
                batch.opportunities.len()
            );
        }

        let mut items = Vec::with_capacity(batch.opportunities.len());
        for (opp, params) in batch.opportunities.iter().zip(params.iter()) {
            let LiquidationParams::Venus {
                borrower,
                collateral_vtoken,
                debt_vtoken,
                repay_amount,
            } = params;
            items.push(BatchParams {
                protocolId: PROTOCOL_VENUS,
                borrower: *borrower,
                debtToken: opp.position.debt_token,
                collateralToken: opp.position.collateral_token,
                debtVToken: *debt_vtoken,
                collateralVToken: *collateral_vtoken,
                repayAmount: *repay_amount,
                minSwapOut: opp.swap_route.min_amount_out,
            });
        }

        let call = ICharonBatch::batchExecuteCall { items };
        let bytes: Bytes = call.abi_encode().into();
        debug!(
            items = batch.opportunities.len(),
            calldata_len = bytes.len(),
            chain_id = batch.chain_id,
            "batch calldata encoded"
        );
        Ok(bytes)
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
    use alloy::primitives::{Address, U256, address};
    use charon_core::{FlashLoanSource, Position, ProtocolId, SwapRoute};

    fn mk_opp(chain_id: u64, net_cents: u64, borrower_byte: u8) -> LiquidationOpportunity {
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
                pool_fee: 3_000,
            },
            net_profit_usd_cents: net_cents,
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
        let out = Batcher::with_default_size().plan(vec![mk_opp(56, 100, 1)]);
        assert!(out.is_empty(), "1-item input should yield no batches");
    }

    #[test]
    fn same_chain_groups_into_one_batch() {
        let out = Batcher::with_default_size().plan(vec![
            mk_opp(56, 300, 1),
            mk_opp(56, 200, 2),
            mk_opp(56, 100, 3),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chain_id, 56);
        assert_eq!(out[0].opportunities.len(), 3);
        assert_eq!(out[0].total_net_usd_cents, 600);
    }

    #[test]
    fn different_chains_produce_separate_batches() {
        let out = Batcher::with_default_size().plan(vec![
            mk_opp(56, 100, 1),
            mk_opp(56, 100, 2),
            mk_opp(1, 100, 3),
            mk_opp(1, 100, 4),
        ]);
        assert_eq!(out.len(), 2);
        let mut chains: Vec<u64> = out.iter().map(|b| b.chain_id).collect();
        chains.sort();
        assert_eq!(chains, vec![1, 56]);
    }

    #[test]
    fn batches_split_when_group_exceeds_max_size() {
        // Size 2 → 5 opps → chunks of [2, 2, 1]; the trailing 1 is
        // dropped because it's not a real batch.
        let b = Batcher::new(2);
        let out = b.plan((1..=5).map(|i| mk_opp(56, 100, i)).collect());
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|b| b.opportunities.len() == 2));
    }

    #[test]
    fn encode_calldata_has_batch_execute_selector() {
        let batch = LiquidationBatch {
            chain_id: 56,
            opportunities: vec![mk_opp(56, 100, 1), mk_opp(56, 200, 2)],
            total_net_usd_cents: 300,
        };
        let params = vec![mk_params(1), mk_params(2)];
        let bytes = Batcher::with_default_size()
            .encode_calldata(&batch, &params)
            .expect("encode");
        assert_eq!(
            &bytes[..4],
            &ICharonBatch::batchExecuteCall::SELECTOR,
            "calldata selector drifted from batchExecute"
        );
    }

    #[test]
    fn encode_calldata_rejects_mismatched_lengths() {
        let batch = LiquidationBatch {
            chain_id: 56,
            opportunities: vec![mk_opp(56, 100, 1), mk_opp(56, 200, 2)],
            total_net_usd_cents: 300,
        };
        let params = vec![mk_params(1)]; // only one
        assert!(
            Batcher::with_default_size()
                .encode_calldata(&batch, &params)
                .is_err()
        );
    }
}
