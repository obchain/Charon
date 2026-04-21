//! Gas-aware profit calculator.
//!
//! Sits between the scanner (surfaces a liquidatable `Position`) and the
//! router (picks a flash-loan source): given a candidate liquidation
//! priced entirely in USD cents — plus the configured `min_profit_usd`
//! threshold — decide whether it's worth building a transaction for.
//!
//! Everything is in USD cents (u64). Integer math throughout so we
//! don't accumulate float error across the hot path; conversion from
//! on-chain amounts happens once at the caller using the price cache.
//!
//! ```text
//! gross      = repay_usd × liquidation_bonus_bps / 10_000
//! slippage   = gross      × slippage_bps          / 10_000
//! net        = gross − flash_fee − gas − slippage
//! ```

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Everything the calculator needs, already converted to USD cents.
#[derive(Debug, Clone, Copy)]
pub struct ProfitInputs {
    /// Debt the bot will repay (and therefore also the flash-loan
    /// amount), expressed in USD cents after price-cache lookup.
    pub repay_amount_usd_cents: u64,
    /// Liquidation bonus paid on top of the seized collateral, in
    /// basis points. Venus is `1000` (10%) on most markets.
    pub liquidation_bonus_bps: u16,
    /// Absolute flash-loan fee in USD cents
    /// (`amount_usd × fee_bps / 10_000` at the call site).
    pub flash_fee_usd_cents: u64,
    /// Expected gas cost for the liquidation tx in USD cents.
    pub gas_cost_usd_cents: u64,
    /// DEX swap slippage to budget for, in basis points on the gross
    /// profit (`50` = 0.5% of gross).
    pub slippage_bps: u16,
}

/// Itemised profit breakdown returned on success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetProfit {
    pub gross_usd_cents: u64,
    pub flash_fee_usd_cents: u64,
    pub gas_cost_usd_cents: u64,
    pub slippage_usd_cents: u64,
    pub net_usd_cents: u64,
}

impl NetProfit {
    pub fn net_usd(&self) -> f64 {
        self.net_usd_cents as f64 / 100.0
    }
    pub fn gross_usd(&self) -> f64 {
        self.gross_usd_cents as f64 / 100.0
    }
}

/// Compute net profit for a candidate liquidation.
///
/// Returns `Err` whenever the liquidation is unprofitable — either the
/// total cost (flash fee + gas + slippage) swallows the gross bonus,
/// or the net falls below `min_profit_usd`. The caller is expected to
/// drop the opportunity on `Err`; no partial state is ever emitted.
pub fn calculate_profit(inputs: &ProfitInputs, min_profit_usd: f64) -> Result<NetProfit> {
    // Validate bps inputs up-front; out-of-range values would silently
    // distort the math without panicking.
    if inputs.liquidation_bonus_bps > 10_000 {
        anyhow::bail!(
            "liquidation_bonus_bps {} exceeds 100% (10_000 bps)",
            inputs.liquidation_bonus_bps
        );
    }
    if inputs.slippage_bps > 10_000 {
        anyhow::bail!(
            "slippage_bps {} exceeds 100% (10_000 bps)",
            inputs.slippage_bps
        );
    }

    // gross = repay × bonus_bps / 10_000
    let gross = inputs
        .repay_amount_usd_cents
        .checked_mul(inputs.liquidation_bonus_bps as u64)
        .ok_or_else(|| anyhow::anyhow!("profit: gross multiplication overflow"))?
        / 10_000;

    let slippage = gross
        .checked_mul(inputs.slippage_bps as u64)
        .ok_or_else(|| anyhow::anyhow!("profit: slippage multiplication overflow"))?
        / 10_000;

    let total_cost = inputs
        .flash_fee_usd_cents
        .checked_add(inputs.gas_cost_usd_cents)
        .and_then(|v| v.checked_add(slippage))
        .ok_or_else(|| anyhow::anyhow!("profit: total-cost addition overflow"))?;

    if gross <= total_cost {
        anyhow::bail!(
            "unprofitable: gross={:.2} ≤ total_cost={:.2} (flash_fee={:.2}, gas={:.2}, slippage={:.2})",
            cents_to_usd(gross),
            cents_to_usd(total_cost),
            cents_to_usd(inputs.flash_fee_usd_cents),
            cents_to_usd(inputs.gas_cost_usd_cents),
            cents_to_usd(slippage)
        );
    }

    let net = gross - total_cost;
    let min_cents = (min_profit_usd.max(0.0) * 100.0) as u64;
    if net < min_cents {
        anyhow::bail!(
            "below threshold: net={:.2} < min_profit_usd={:.2}",
            cents_to_usd(net),
            min_profit_usd
        );
    }

    Ok(NetProfit {
        gross_usd_cents: gross,
        flash_fee_usd_cents: inputs.flash_fee_usd_cents,
        gas_cost_usd_cents: inputs.gas_cost_usd_cents,
        slippage_usd_cents: slippage,
        net_usd_cents: net,
    })
}

fn cents_to_usd(cents: u64) -> f64 {
    cents as f64 / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typical_inputs() -> ProfitInputs {
        // $1000 debt, 10% bonus → $100 gross.
        ProfitInputs {
            repay_amount_usd_cents: 100_000,
            liquidation_bonus_bps: 1_000,
            flash_fee_usd_cents: 50, // $0.50 (Aave V3 0.05% on $1000)
            gas_cost_usd_cents: 200, // $2 gas
            slippage_bps: 50,        // 0.5% of gross = $0.50
        }
    }

    #[test]
    fn healthy_liquidation_is_profitable() {
        let np = calculate_profit(&typical_inputs(), 5.0).expect("profitable");
        assert_eq!(np.gross_usd_cents, 10_000); // $100
        assert_eq!(np.slippage_usd_cents, 50);
        // net = 10_000 − 50 (fee) − 200 (gas) − 50 (slippage) = 9_700
        assert_eq!(np.net_usd_cents, 9_700);
    }

    #[test]
    fn below_threshold_is_rejected() {
        let inputs = typical_inputs();
        let err = calculate_profit(&inputs, 200.0).expect_err("should reject");
        assert!(format!("{err:#}").contains("below threshold"));
    }

    #[test]
    fn cost_greater_than_gross_is_rejected() {
        let inputs = ProfitInputs {
            repay_amount_usd_cents: 1_000,
            liquidation_bonus_bps: 1_000, // $1 debt × 10% = $0.10 gross
            flash_fee_usd_cents: 1,
            gas_cost_usd_cents: 200, // $2 gas eats the whole thing
            slippage_bps: 50,
        };
        let err = calculate_profit(&inputs, 0.0).expect_err("unprofitable");
        assert!(format!("{err:#}").contains("unprofitable"));
    }

    #[test]
    fn bogus_bps_values_are_rejected() {
        let mut inputs = typical_inputs();
        inputs.liquidation_bonus_bps = 20_000;
        assert!(calculate_profit(&inputs, 0.0).is_err());

        inputs = typical_inputs();
        inputs.slippage_bps = 20_000;
        assert!(calculate_profit(&inputs, 0.0).is_err());
    }

    #[test]
    fn zero_threshold_accepts_any_positive_net() {
        // Gross = $1, costs = $0.50 total → net = $0.50 > $0 threshold
        let inputs = ProfitInputs {
            repay_amount_usd_cents: 1_000,
            liquidation_bonus_bps: 1_000,
            flash_fee_usd_cents: 30,
            gas_cost_usd_cents: 15,
            slippage_bps: 50,
        };
        let np = calculate_profit(&inputs, 0.0).expect("profitable");
        assert!(np.net_usd_cents > 0);
    }
}
