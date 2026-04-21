//! EIP-1559 gas oracle.
//!
//! Three responsibilities:
//!
//! 1. **Live fee snapshot** — read `baseFeePerGas` from the latest
//!    block header, compute `maxFeePerGas` with a 25 % cushion, and
//!    bolt on the per-chain priority fee from config.
//! 2. **Ceiling enforcement** — refuse to emit gas params if the
//!    proposed `maxFeePerGas` exceeds `bot.max_gas_gwei`. Caller drops
//!    the opportunity rather than overpaying.
//! 3. **Cost estimation in USD cents** — converts `gas_units × maxFee`
//!    (wei) into integer USD cents using a Chainlink price reading
//!    for the chain's native asset (BNB on BSC). The result feeds
//!    [`charon_core::ProfitInputs`].

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::U256;
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use anyhow::{Context, Result};
use tracing::{debug, warn};

/// 25 % over base fee — enough to clear one block of normal congestion
/// without overshooting on a quiet chain. Same number the PRD uses.
const BASE_FEE_BUMP_PCT: u128 = 125;
const BPS_DIV: u128 = 100;
const ONE_GWEI: u128 = 1_000_000_000;

/// Resolved EIP-1559 gas parameters for one transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GasParams {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

/// Per-chain gas oracle. Construct once per chain at startup.
#[derive(Debug, Clone, Copy)]
pub struct GasOracle {
    /// Drop the tx if `max_fee_per_gas` exceeds this (gwei).
    max_gas_gwei: u64,
    /// EIP-1559 priority fee, gwei.
    priority_fee_gwei: u64,
}

impl GasOracle {
    pub fn new(max_gas_gwei: u64, priority_fee_gwei: u64) -> Self {
        Self {
            max_gas_gwei,
            priority_fee_gwei,
        }
    }

    /// Read the latest base fee, bump it 25 %, attach the priority fee.
    /// Returns `Ok(None)` when the resulting `max_fee_per_gas` exceeds
    /// the configured ceiling — caller should skip the opportunity.
    pub async fn fetch_params<P, T>(&self, provider: &P) -> Result<Option<GasParams>>
    where
        P: Provider<T>,
        T: alloy::transports::Transport + Clone,
    {
        let block = provider
            .get_block(
                BlockNumberOrTag::Latest.into(),
                alloy::rpc::types::BlockTransactionsKind::Hashes,
            )
            .await
            .context("gas oracle: get_block(latest) failed")?
            .context("gas oracle: latest block missing")?;

        let base_fee: u128 = block
            .header
            .base_fee_per_gas
            .context("gas oracle: header has no base_fee_per_gas (pre-EIP-1559 chain?)")?
            .into();

        let max_fee = base_fee
            .checked_mul(BASE_FEE_BUMP_PCT)
            .context("gas oracle: max-fee multiplication overflow")?
            / BPS_DIV;
        let max_fee_gwei = max_fee / ONE_GWEI;

        if max_fee_gwei > u128::from(self.max_gas_gwei) {
            warn!(
                max_fee_gwei,
                ceiling_gwei = self.max_gas_gwei,
                "gas exceeds configured ceiling — skipping tx"
            );
            return Ok(None);
        }

        let max_priority_fee_per_gas = u128::from(self.priority_fee_gwei) * ONE_GWEI;
        let params = GasParams {
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas,
        };
        debug!(
            base_fee_gwei = base_fee / ONE_GWEI,
            max_fee_gwei = params.max_fee_per_gas / ONE_GWEI,
            priority_fee_gwei = params.max_priority_fee_per_gas / ONE_GWEI,
            "gas params resolved"
        );
        Ok(Some(params))
    }

    /// Run `eth_estimateGas` on `tx` and return the unit count.
    pub async fn estimate_gas_units<P, T>(
        &self,
        provider: &P,
        tx: &TransactionRequest,
    ) -> Result<u64>
    where
        P: Provider<T>,
        T: alloy::transports::Transport + Clone,
    {
        provider
            .estimate_gas(tx)
            .await
            .context("gas oracle: eth_estimateGas failed")
    }
}

/// Convert `gas_units × max_fee_per_gas` (wei cost) into USD cents,
/// given a Chainlink reading for the chain's native asset.
///
/// `native_price` is the raw aggregator answer; `native_decimals` is
/// the feed's `decimals()` (typically 8). The native unit is assumed
/// to be 18-decimal (true on BNB / ETH / MATIC / AVAX).
pub fn gas_cost_usd_cents(
    gas_units: u64,
    max_fee_per_gas: u128,
    native_price: U256,
    native_decimals: u8,
) -> u64 {
    let wei_cost: u128 = (gas_units as u128).saturating_mul(max_fee_per_gas);
    // wei (1e18 = 1 native) × price (10^decimals = $1) → cents.
    // Divide by 10^(18 + decimals - 2) to land in cents.
    let exponent = 18u32 + u32::from(native_decimals) - 2;
    let divisor = U256::from(10u64).pow(U256::from(exponent));

    let numerator = U256::from(wei_cost).saturating_mul(native_price);
    let cents = numerator / divisor;
    u64::try_from(cents).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gas_cost_for_one_gwei_at_bnb_632_dollar() {
        // 200 000 gas × 1 gwei = 2e5 × 1e9 = 2e14 wei.
        // BNB at $632.85 → Chainlink price 63284968915 (8 decimals).
        // Expected: 2e14 × 6.3285e10 / 1e24 = ~12.66e0 = ~12.66 cents.
        let cents = gas_cost_usd_cents(200_000, ONE_GWEI, U256::from(63_284_968_915u128), 8);
        // Allow ±1 cent for integer-division rounding.
        assert!((12..=14).contains(&cents), "got {cents} cents");
    }

    #[test]
    fn gas_cost_zero_when_units_zero() {
        let cents = gas_cost_usd_cents(0, ONE_GWEI, U256::from(1u64), 8);
        assert_eq!(cents, 0);
    }

    #[test]
    fn gas_cost_saturates_on_huge_inputs() {
        // 1e9 gas × 1e30 wei/gas = 1e39 wei — clearly absurd, must
        // saturate to u64::MAX without panicking.
        let cents = gas_cost_usd_cents(
            1_000_000_000,
            10u128.pow(30),
            U256::from(63_284_968_915u128),
            8,
        );
        assert_eq!(cents, u64::MAX);
    }
}
