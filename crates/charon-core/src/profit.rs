//! Gas-aware profit calculator.
//!
//! Sits between the scanner (surfaces a liquidatable [`Position`]) and
//! the router (picks a flash-loan source): given a candidate liquidation,
//! decide whether it clears the configured `min_profit_usd_1e6`
//! threshold.
//!
//! # Unit discipline
//!
//! The calculator is **native-wei first**: every cost is denominated in
//! the **debt token's base units (wei)**, matching
//! [`LiquidationOpportunity::net_profit_wei`]. Wei is the canonical
//! storage unit — it avoids f64 drift, survives chain-native precision,
//! and never depends on a USD oracle to express a profit figure.
//!
//! USD is a reporting / gating concern only. The final [`NetProfit`]
//! carries both the authoritative `net_profit_wei: U256` and a
//! derived `net_profit_usd_1e6: u64` convenience field for logging and
//! the `min_profit_usd_1e6` threshold compare.
//!
//! # Profit formula (all amounts in **debt-token wei**)
//!
//! ```text
//! gross_debt_wei      = expected_swap_output_wei
//! slippage_wei        = expected_swap_output_wei * slippage_bps / 10_000
//! total_cost_wei      = flash_fee_wei + gas_cost_debt_wei + slippage_wei
//! net_profit_wei      = gross_debt_wei - total_cost_wei   (saturating)
//! ```
//!
//! Slippage is charged against the DEX swap output (collateral ->
//! debt-token) because that is the trade whose execution price the bot
//! is exposed to — losing 0.5% on a $10 000 swap is $50, not 0.5% of
//! the $1 000 bonus.
//!
//! Gas is passed in already converted to debt-token wei
//! (`gas_cost_debt_wei`). The conversion `gas_units *
//! effective_gas_price * native_price / debt_price` is the caller's
//! responsibility — typically it goes through a PriceCache lookup
//! against Chainlink feeds for the native asset (BNB) and the debt
//! token.

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::types::LiquidationOpportunity;

/// Chainlink-style 1e8 price of one whole token, in USD.
///
/// BSC-native Chainlink aggregators report `int256` answers with 8
/// decimals. We normalise to `u64` here — any feed that returns a
/// negative answer is a feed fault and must be rejected upstream before
/// this type is constructed.
///
/// `usd_1e8 = 6 * 10^10` means 1 token = $600.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Price {
    /// USD value of 1 whole token, scaled by `1e8`.
    pub usd_1e8: u64,
}

impl Price {
    /// Construct a price; rejects zero/malformed feeds.
    pub fn new(usd_1e8: u64) -> Result<Self, ProfitError> {
        if usd_1e8 == 0 {
            return Err(ProfitError::InvalidPrice);
        }
        Ok(Self { usd_1e8 })
    }
}

/// Hard-typed errors from the profit calculator.
///
/// Every negative outcome the executor can plausibly react to is a
/// distinct variant — no `anyhow` `String` matching in the hot path.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ProfitError {
    /// A basis-points input exceeds `10_000` (100%).
    #[error("basis-points value {0} exceeds 10_000 (100%)")]
    InvalidBps(u16),
    /// Price feed produced zero or malformed output.
    #[error("price feed reported a zero or invalid value")]
    InvalidPrice,
    /// Unsigned arithmetic would have wrapped.
    #[error("arithmetic overflow while computing profit")]
    Overflow,
    /// Token decimals exceed the supported range (0..=18).
    #[error("unsupported token decimals {0} (must be <= 18)")]
    UnsupportedDecimals(u8),
    /// Total cost swallows the gross swap output — liquidation is unprofitable.
    #[error(
        "unprofitable: gross_wei={gross_wei} <= total_cost_wei={total_cost_wei} \
         (flash_fee_wei={flash_fee_wei}, gas_cost_wei={gas_cost_wei}, slippage_wei={slippage_wei})"
    )]
    Unprofitable {
        gross_wei: U256,
        total_cost_wei: U256,
        flash_fee_wei: U256,
        gas_cost_wei: U256,
        slippage_wei: U256,
    },
    /// Net profit is positive but below the configured threshold.
    #[error("below threshold: net_usd_1e6={net_usd_1e6} < min_usd_1e6={min_usd_1e6}")]
    BelowMinThreshold { net_usd_1e6: u64, min_usd_1e6: u64 },
}

/// Everything the calculator needs, already expressed in debt-token wei.
///
/// Construct via [`ProfitInputs::from_opportunity`] whenever possible;
/// the direct literal form is kept only for tests and callers who have
/// already priced the opportunity themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProfitInputs {
    /// Expected DEX output (seized collateral -> debt token) in
    /// debt-token wei. This is the bot's realised revenue before fees
    /// and slippage, and is the value against which slippage is
    /// applied.
    pub expected_swap_output_wei: U256,
    /// Absolute flash-loan fee in debt-token wei (matches
    /// `FlashLoanQuote::fee` for an Aave-style loan denominated in the
    /// debt asset).
    pub flash_fee_wei: U256,
    /// Expected gas cost for the full liquidation tx, converted to
    /// **debt-token wei** by the caller (typical conversion:
    /// `gas_units * effective_gas_price * native_price / debt_price`).
    pub gas_cost_debt_wei: U256,
    /// DEX swap slippage to budget for, in basis points applied to
    /// `expected_swap_output_wei`. `50` = 0.5%.
    pub slippage_bps: u16,
    /// USD price of the debt token, Chainlink 1e8 scaled. Used to
    /// convert `net_profit_wei` into `net_profit_usd_1e6` for the
    /// threshold compare and for logging — never used inside the
    /// arithmetic path.
    pub debt_price: Price,
    /// Debt-token decimals (0..=18). Drives the final wei->USD_1e6
    /// conversion.
    pub debt_decimals: u8,
}

/// Itemised profit breakdown returned on success.
///
/// `net_profit_wei` is authoritative and is the value copied into
/// [`LiquidationOpportunity::net_profit_wei`] via
/// [`LiquidationOpportunity::with_profit`]. All `*_wei` fields are
/// denominated in **debt-token wei**. `net_profit_usd_1e6` is a
/// derived convenience figure for the threshold compare and logs; do
/// **not** feed it back into downstream wei arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct NetProfit {
    pub gross_wei: U256,
    pub flash_fee_wei: U256,
    pub gas_cost_wei: U256,
    pub slippage_wei: U256,
    pub net_profit_wei: U256,
    /// Derived USD value of `net_profit_wei`, scaled by 1e6.
    /// Convenience only — for logs and threshold display.
    pub net_profit_usd_1e6: u64,
}

/// 10^n for n in 0..=18 — pre-computed so the decimals path never
/// allocates and never panics on overflow.
const POW10: [u128; 19] = [
    1,
    10,
    100,
    1_000,
    10_000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
    10_000_000_000,
    100_000_000_000,
    1_000_000_000_000,
    10_000_000_000_000,
    100_000_000_000_000,
    1_000_000_000_000_000,
    10_000_000_000_000_000,
    100_000_000_000_000_000,
    1_000_000_000_000_000_000,
];

/// Convert `amount_wei` of a token with `decimals` at `price` into USD
/// micro-units (1e6).
///
/// ```text
/// usd_1e6 = amount_wei * usd_1e8 / 10^decimals / 10^2
/// ```
///
/// Performed in `U256` so an 18-decimal BEP-20 at trillion-dollar scale
/// still cannot overflow. The final USD_1e6 value is range-checked to
/// fit `u64` — anything larger is a faulty input and returns
/// [`ProfitError::Overflow`].
fn wei_to_usd_1e6(amount_wei: U256, price: Price, decimals: u8) -> Result<u64, ProfitError> {
    if (decimals as usize) >= POW10.len() {
        return Err(ProfitError::UnsupportedDecimals(decimals));
    }
    // scale = 10^decimals * 10^2
    // (divide by 10^decimals to get whole tokens;
    //  divide by 10^2       to move from 1e8-priced USD down to 1e6.)
    let pow_dec = U256::from(POW10[decimals as usize]);
    let pow_2 = U256::from(100u64);
    let scale = pow_dec.checked_mul(pow_2).ok_or(ProfitError::Overflow)?;
    let numerator = amount_wei
        .checked_mul(U256::from(price.usd_1e8))
        .ok_or(ProfitError::Overflow)?;
    // scale >= 100 (non-zero) so division cannot panic.
    let usd_u256 = numerator / scale;
    let usd: u64 = usd_u256.try_into().map_err(|_| ProfitError::Overflow)?;
    Ok(usd)
}

impl ProfitInputs {
    /// Construct [`ProfitInputs`] from a [`LiquidationOpportunity`]
    /// plus live gas / fee quotes.
    ///
    /// # Inputs
    ///
    /// - `opportunity` — the candidate, in native wei-scale units.
    /// - `expected_swap_output_wei` — DEX router quote for the seized
    ///   collateral -> debt-token swap, in debt-token wei. Slippage is
    ///   applied to this.
    /// - `flash_fee_wei` — absolute flash-loan fee in debt-token wei.
    /// - `gas_cost_debt_wei` — gas budget converted to debt-token wei.
    /// - `slippage_bps` — DEX slippage budget.
    /// - `debt_price` / `debt_decimals` — debt-token Chainlink price
    ///   and BEP-20 decimals (must be `<= 18`), used downstream to
    ///   derive `net_profit_usd_1e6`.
    pub fn from_opportunity(
        opportunity: &LiquidationOpportunity,
        expected_swap_output_wei: U256,
        flash_fee_wei: U256,
        gas_cost_debt_wei: U256,
        slippage_bps: u16,
        debt_price: Price,
        debt_decimals: u8,
    ) -> Result<Self, ProfitError> {
        if slippage_bps > 10_000 {
            return Err(ProfitError::InvalidBps(slippage_bps));
        }
        if opportunity.position.liquidation_bonus_bps > 10_000 {
            return Err(ProfitError::InvalidBps(
                opportunity.position.liquidation_bonus_bps,
            ));
        }
        if (debt_decimals as usize) >= POW10.len() {
            return Err(ProfitError::UnsupportedDecimals(debt_decimals));
        }

        Ok(Self {
            expected_swap_output_wei,
            flash_fee_wei,
            gas_cost_debt_wei,
            slippage_bps,
            debt_price,
            debt_decimals,
        })
    }
}

/// Compute net profit for a candidate liquidation.
///
/// Returns `Err` whenever the liquidation is unprofitable — either the
/// total cost (flash fee + gas + slippage) swallows the gross swap
/// output, or the net (converted to USD_1e6 via `inputs.debt_price`)
/// falls below `min_profit_usd_1e6`. The caller is expected to drop
/// the opportunity on `Err`; no partial state is ever emitted.
pub fn calculate_profit(
    inputs: &ProfitInputs,
    min_profit_usd_1e6: u64,
) -> Result<NetProfit, ProfitError> {
    if inputs.slippage_bps > 10_000 {
        return Err(ProfitError::InvalidBps(inputs.slippage_bps));
    }

    // Slippage is charged on the DEX swap output — the bot only pays
    // slippage on the swap it performs.
    let slippage_mul = inputs
        .expected_swap_output_wei
        .checked_mul(U256::from(inputs.slippage_bps))
        .ok_or(ProfitError::Overflow)?;
    // 10_000 is a non-zero constant so the division is infallible.
    let slippage_wei = slippage_mul / U256::from(10_000u64);

    let total_cost_wei = inputs
        .flash_fee_wei
        .checked_add(inputs.gas_cost_debt_wei)
        .and_then(|v| v.checked_add(slippage_wei))
        .ok_or(ProfitError::Overflow)?;

    let gross_wei = inputs.expected_swap_output_wei;

    if gross_wei <= total_cost_wei {
        return Err(ProfitError::Unprofitable {
            gross_wei,
            total_cost_wei,
            flash_fee_wei: inputs.flash_fee_wei,
            gas_cost_wei: inputs.gas_cost_debt_wei,
            slippage_wei,
        });
    }

    // gross > total_cost (checked above).
    let net_profit_wei = gross_wei
        .checked_sub(total_cost_wei)
        .ok_or(ProfitError::Overflow)?;

    // Convert net_profit_wei to USD_1e6 for threshold compare + logs.
    let net_profit_usd_1e6 =
        wei_to_usd_1e6(net_profit_wei, inputs.debt_price, inputs.debt_decimals)?;

    if net_profit_usd_1e6 < min_profit_usd_1e6 {
        return Err(ProfitError::BelowMinThreshold {
            net_usd_1e6: net_profit_usd_1e6,
            min_usd_1e6: min_profit_usd_1e6,
        });
    }

    Ok(NetProfit {
        gross_wei,
        flash_fee_wei: inputs.flash_fee_wei,
        gas_cost_wei: inputs.gas_cost_debt_wei,
        slippage_wei,
        net_profit_wei,
        net_profit_usd_1e6,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FlashLoanSource, LiquidationOpportunity, Position, ProtocolId, SwapRoute};
    use alloy::primitives::{Address, U256, address};

    /// 1 BNB = 1e18 wei.
    const ONE_BNB: u128 = 1_000_000_000_000_000_000;

    fn typical_inputs() -> ProfitInputs {
        // $1 000 debt at $600/BNB ~= 1.667 BNB. Keep everything in BNB
        // wei for readability of the arithmetic.
        //
        // Swap output = 1.1 BNB, flash fee = 0.05% of 1 BNB = 0.0005 BNB,
        // gas = 0.001 BNB, 0.5% slippage budget.
        ProfitInputs {
            expected_swap_output_wei: U256::from(ONE_BNB)
                .checked_mul(U256::from(11u64))
                .expect("const mul")
                / U256::from(10u64),
            flash_fee_wei: U256::from(ONE_BNB / 2_000),
            gas_cost_debt_wei: U256::from(ONE_BNB / 1_000),
            slippage_bps: 50,
            debt_price: Price::new(60_000_000_000).expect("valid"), // $600
            debt_decimals: 18,
        }
    }

    #[test]
    fn healthy_liquidation_is_profitable() {
        let inputs = typical_inputs();
        // min = $5.00 (1e6 scale)
        let np = calculate_profit(&inputs, 5_000_000).expect("profitable");

        // slippage = 1.1 BNB * 50 / 10_000 = 0.0055 BNB
        let expected_slippage = inputs.expected_swap_output_wei * U256::from(50u64)
            / U256::from(10_000u64);
        assert_eq!(np.slippage_wei, expected_slippage);

        // net = 1.1 - 0.0005 - 0.001 - 0.0055 = 1.0930 BNB
        let expected_net = inputs.expected_swap_output_wei
            - inputs.flash_fee_wei
            - inputs.gas_cost_debt_wei
            - expected_slippage;
        assert_eq!(np.net_profit_wei, expected_net);

        // 1.0930 BNB * $600 ~= $655.80 -> 655_800_000 in 1e6 scale.
        // Allow last-digit rounding (floor division).
        assert!(np.net_profit_usd_1e6 >= 655_000_000);
        assert!(np.net_profit_usd_1e6 <= 656_000_000);
    }

    #[test]
    fn below_threshold_is_rejected() {
        // Threshold of $10 000 -> $1 000 000 000 in 1e6 scale. Typical
        // inputs yield ~$650, nowhere near the bar.
        let err = calculate_profit(&typical_inputs(), 1_000_000_000_000)
            .expect_err("should reject below threshold");
        assert!(matches!(err, ProfitError::BelowMinThreshold { .. }));
    }

    #[test]
    fn cost_greater_than_gross_is_rejected() {
        let inputs = ProfitInputs {
            expected_swap_output_wei: U256::from(ONE_BNB / 1_000), // tiny trade
            flash_fee_wei: U256::from(ONE_BNB / 500),
            gas_cost_debt_wei: U256::from(ONE_BNB / 500),
            slippage_bps: 50,
            debt_price: Price::new(60_000_000_000).expect("valid"),
            debt_decimals: 18,
        };
        let err = calculate_profit(&inputs, 0).expect_err("unprofitable");
        assert!(matches!(err, ProfitError::Unprofitable { .. }));
    }

    #[test]
    fn bogus_slippage_bps_is_rejected() {
        let mut inputs = typical_inputs();
        inputs.slippage_bps = 20_000;
        assert!(matches!(
            calculate_profit(&inputs, 0),
            Err(ProfitError::InvalidBps(20_000))
        ));
    }

    #[test]
    fn slippage_bps_boundary_10_000_is_accepted_and_10_001_rejected() {
        let mut inputs = typical_inputs();
        // 100% slippage consumes the full swap output -> unprofitable,
        // but the bps check itself must *pass*.
        inputs.slippage_bps = 10_000;
        let err = calculate_profit(&inputs, 0)
            .expect_err("100% slippage eats the whole swap");
        assert!(matches!(err, ProfitError::Unprofitable { .. }));

        inputs.slippage_bps = 10_001;
        assert!(matches!(
            calculate_profit(&inputs, 0),
            Err(ProfitError::InvalidBps(10_001))
        ));
    }

    #[test]
    fn min_profit_zero_accepts_any_positive_net() {
        let inputs = ProfitInputs {
            expected_swap_output_wei: U256::from(ONE_BNB / 100),
            flash_fee_wei: U256::from(ONE_BNB / 100_000),
            gas_cost_debt_wei: U256::from(ONE_BNB / 100_000),
            slippage_bps: 50,
            debt_price: Price::new(60_000_000_000).expect("valid"),
            debt_decimals: 18,
        };
        let np = calculate_profit(&inputs, 0).expect("profitable");
        assert!(np.net_profit_wei > U256::ZERO);
    }

    #[test]
    fn total_cost_addition_overflow_is_reported() {
        // Force checked_add(flash_fee_wei, gas_cost_debt_wei) to wrap.
        let inputs = ProfitInputs {
            expected_swap_output_wei: U256::ZERO,
            flash_fee_wei: U256::MAX,
            gas_cost_debt_wei: U256::from(1u64),
            slippage_bps: 0,
            debt_price: Price::new(60_000_000_000).expect("valid"),
            debt_decimals: 18,
        };
        assert!(matches!(
            calculate_profit(&inputs, 0),
            Err(ProfitError::Overflow)
        ));
    }

    // ── from_opportunity / wei->usd_1e6 path ────────────────────────

    fn mk_opp(
        collateral_amount: U256,
        debt_amount: U256,
        bonus_bps: u16,
    ) -> LiquidationOpportunity {
        LiquidationOpportunity {
            position: Position {
                protocol: ProtocolId::Venus,
                chain_id: 56,
                borrower: address!("1111111111111111111111111111111111111111"),
                collateral_token: Address::ZERO,
                debt_token: Address::ZERO,
                collateral_amount,
                debt_amount,
                health_factor: U256::ZERO,
                liquidation_bonus_bps: bonus_bps,
            },
            debt_to_repay: debt_amount,
            expected_collateral_out: collateral_amount,
            flash_source: FlashLoanSource::AaveV3,
            swap_route: SwapRoute {
                token_in: Address::ZERO,
                token_out: Address::ZERO,
                amount_in: collateral_amount,
                min_amount_out: debt_amount,
                pool_fee: None,
            },
            net_profit_wei: U256::ZERO,
        }
    }

    #[test]
    fn bsc_bnb_one_token_at_600_usd_prices_to_600_dollars() {
        // 1 BNB repay, matching-asset collateral, 10% bonus, $600 price
        let one_bnb = U256::from(ONE_BNB);
        let one_point_one_bnb = one_bnb * U256::from(11u64) / U256::from(10u64);
        let opp = mk_opp(one_point_one_bnb, one_bnb, 1_000);
        let price = Price::new(60_000_000_000).expect("valid"); // $600

        // Swap output ~= 1.1 BNB; flash fee = 0.05% of 1 BNB = 0.0005 BNB.
        let flash_fee_wei = one_bnb / U256::from(2_000u64);
        let gas_cost_debt_wei = one_bnb / U256::from(1_000u64);

        let inputs = ProfitInputs::from_opportunity(
            &opp,
            one_point_one_bnb,
            flash_fee_wei,
            gas_cost_debt_wei,
            50,
            price,
            18,
        )
        .expect("valid");

        assert_eq!(inputs.expected_swap_output_wei, one_point_one_bnb);
        assert_eq!(inputs.flash_fee_wei, flash_fee_wei);

        let np = calculate_profit(&inputs, 0).expect("profitable");
        // net = 1.1 - 0.0005 - 0.001 - (1.1*0.005) = 1.0930 BNB ~= $655.80
        assert!(np.net_profit_usd_1e6 >= 655_000_000);
        assert!(np.net_profit_usd_1e6 <= 656_000_000);
    }

    #[test]
    fn zero_price_is_rejected() {
        assert!(matches!(Price::new(0), Err(ProfitError::InvalidPrice)));
    }

    #[test]
    fn decimals_above_18_are_rejected() {
        let opp = mk_opp(U256::from(1u64), U256::from(1u64), 1_000);
        let price = Price::new(60_000_000_000).expect("valid");
        assert!(matches!(
            ProfitInputs::from_opportunity(
                &opp,
                U256::from(1u64),
                U256::from(0u64),
                U256::from(0u64),
                0,
                price,
                19, // invalid
            ),
            Err(ProfitError::UnsupportedDecimals(19))
        ));
    }

    #[test]
    fn position_bonus_bps_above_10_000_is_rejected_in_constructor() {
        let opp = mk_opp(U256::from(1u64), U256::from(1u64), 10_001);
        let price = Price::new(60_000_000_000).expect("valid");
        assert!(matches!(
            ProfitInputs::from_opportunity(
                &opp,
                U256::from(1u64),
                U256::from(0u64),
                U256::from(0u64),
                0,
                price,
                18,
            ),
            Err(ProfitError::InvalidBps(10_001))
        ));
    }

    /// `LiquidationOpportunity::net_profit_wei` must equal
    /// `NetProfit::net_profit_wei` — this is the invariant that
    /// [`LiquidationOpportunity::with_profit`] enforces.
    #[test]
    fn with_profit_copies_net_profit_wei_into_opportunity() {
        let inputs = typical_inputs();
        let np = calculate_profit(&inputs, 0).expect("profitable");
        let opp = mk_opp(U256::from(ONE_BNB), U256::from(ONE_BNB), 1_000);
        let out = LiquidationOpportunity::with_profit(
            opp.position.clone(),
            opp.debt_to_repay,
            opp.expected_collateral_out,
            opp.flash_source,
            opp.swap_route.clone(),
            np,
        );
        assert_eq!(out.net_profit_wei, np.net_profit_wei);
    }
}
