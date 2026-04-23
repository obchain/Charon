//! Gas-aware profit calculator.
//!
//! Sits between the scanner (surfaces a liquidatable [`Position`]) and
//! the router (picks a flash-loan source): given a candidate liquidation
//! priced entirely in USD cents plus the configured
//! `min_profit_usd_1e6` threshold, decide whether it is worth building a
//! transaction for.
//!
//! # Unit discipline
//!
//! Everything inside the calculator is expressed in **USD cents (`u64`)**.
//! Integer math throughout so we do not accumulate float error across
//! the hot path. All wei-scale arithmetic happens exactly once inside
//! [`ProfitInputs::from_opportunity`], where on-chain token amounts are
//! folded into cents through a Chainlink-style 1e8 [`Price`].
//!
//! Cents are chosen (not micro-USD) because they fit comfortably in
//! `u64` up to roughly `1.8e17` USD — enough for any single liquidation.
//! The config threshold is stored in **micro-USD (`1e6`)** so the TOML
//! stays integer-only and round-trips through serde without float
//! surprises; conversion to cents is `micro / 10_000` and happens in the
//! executor when it calls [`calculate_profit`].
//!
//! # Profit formula
//!
//! ```text
//! gross_collateral_cents =
//!     expected_collateral_out_wei * collateral_price_1e8
//!         / 10^collateral_decimals / 1e6
//!
//! # Per-token USD is derived directly from Chainlink prices; the
//! # formula is *not* `repay * bonus / 10_000` — that form only holds
//! # when collateral and debt are the same asset (stable/stable).
//!
//! slippage_cents = expected_swap_output_cents * slippage_bps / 10_000
//! net_cents      = gross_collateral_cents
//!                 - flash_fee_cents - gas_cost_cents - slippage_cents
//! ```
//!
//! Slippage is charged against the **DEX swap output** (seized
//! collateral -> debt token), not the gross bonus — losing 0.5% on a
//! $10 000 swap is $50, not 0.5% of the $1 000 bonus.

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
    /// Total cost swallows the gross bonus — liquidation is unprofitable.
    #[error(
        "unprofitable: gross={gross_cents} cents <= total_cost={total_cost_cents} cents \
         (flash_fee={flash_fee_cents}, gas={gas_cost_cents}, slippage={slippage_cents})"
    )]
    Unprofitable {
        gross_cents: u64,
        total_cost_cents: u64,
        flash_fee_cents: u64,
        gas_cost_cents: u64,
        slippage_cents: u64,
    },
    /// Net profit is positive but below the configured threshold.
    #[error("below threshold: net={net_cents} cents < min={min_cents} cents")]
    BelowMinThreshold { net_cents: u64, min_cents: u64 },
}

/// Everything the calculator needs, already converted to USD cents.
///
/// Construct via [`ProfitInputs::from_opportunity`] whenever possible;
/// the direct literal form is kept only for tests and callers who have
/// already priced the opportunity themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProfitInputs {
    /// Gross USD value of the **collateral the bot will seize**, after
    /// applying `liquidation_bonus_bps`. In USD cents.
    ///
    /// Example: repaying $1 000 of debt against a 10% bonus on an
    /// equal-priced collateral => `gross_collateral_cents = 110_000`
    /// (i.e. $1 100).
    pub gross_collateral_cents: u64,
    /// Expected DEX output (collateral -> debt token) in USD cents.
    ///
    /// Slippage is charged against **this** value, not the gross
    /// collateral — the bot only loses slippage on the swap it
    /// actually performs. Usually very close to
    /// `gross_collateral_cents`, a hair lower because the DEX quote
    /// already reflects pool curvature.
    pub expected_swap_output_cents: u64,
    /// Absolute flash-loan fee in USD cents.
    ///
    /// Converted from the provider quote (`fee_wei * debt_price / 10^decimals`)
    /// inside [`ProfitInputs::from_opportunity`]. Aave V3 on BSC is
    /// `fee_bps = 5` (0.05%) — a $1 000 borrow costs 50 cents.
    pub flash_fee_cents: u64,
    /// Expected gas cost for the full liquidation tx in USD cents.
    ///
    /// Computed off-chain as
    /// `gas_units * effective_gas_price * native_price / 10^18 / 1e6`.
    pub gas_cost_cents: u64,
    /// DEX swap slippage to budget for, in basis points applied to
    /// `expected_swap_output_cents`. `50` = 0.5%.
    pub slippage_bps: u16,
}

/// Itemised profit breakdown returned on success.
///
/// All fields are USD cents (`u64`). [`NetProfit::net_usd`] /
/// [`NetProfit::gross_usd`] are floating-point convenience accessors
/// for logging only — do not feed them back into the profit path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct NetProfit {
    pub gross_usd_cents: u64,
    pub flash_fee_usd_cents: u64,
    pub gas_cost_usd_cents: u64,
    pub slippage_usd_cents: u64,
    pub net_usd_cents: u64,
}

impl NetProfit {
    /// Net profit as USD (display-only; precision is still cents).
    pub fn net_usd(&self) -> f64 {
        (self.net_usd_cents as f64) / 100.0
    }
    /// Gross collateral seized, as USD (display-only).
    pub fn gross_usd(&self) -> f64 {
        (self.gross_usd_cents as f64) / 100.0
    }
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
/// cents.
///
/// ```text
/// cents = amount_wei * usd_1e8 / 10^decimals / 10^6
/// ```
///
/// Performed in `U256` so an 18-decimal BEP-20 at trillion-dollar scale
/// still cannot overflow. The final cents value is range-checked to fit
/// `u64` — anything larger is a faulty input and returns
/// [`ProfitError::Overflow`].
fn wei_to_cents(amount_wei: U256, price: Price, decimals: u8) -> Result<u64, ProfitError> {
    if (decimals as usize) >= POW10.len() {
        return Err(ProfitError::UnsupportedDecimals(decimals));
    }
    // scale = 10^decimals * 10^6   (divide by 10^decimals to get whole
    //                               tokens; divide by 10^6 to move from
    //                               1e8-priced USD down to cents/1e2.)
    let pow_dec = U256::from(POW10[decimals as usize]);
    let pow_6 = U256::from(1_000_000u64);
    let scale = pow_dec.checked_mul(pow_6).ok_or(ProfitError::Overflow)?;
    let numerator = amount_wei
        .checked_mul(U256::from(price.usd_1e8))
        .ok_or(ProfitError::Overflow)?;
    // scale >= 1e6 (non-zero) so division cannot panic.
    let cents_u256 = numerator / scale;
    let cents: u64 = cents_u256.try_into().map_err(|_| ProfitError::Overflow)?;
    Ok(cents)
}

impl ProfitInputs {
    /// Construct [`ProfitInputs`] from a fully-priced
    /// [`LiquidationOpportunity`] plus live feed data.
    ///
    /// # Inputs
    ///
    /// - `opportunity` — the candidate, in native wei-scale units.
    /// - `collateral_price` / `debt_price` — Chainlink-style 1e8 prices.
    /// - `collateral_decimals` / `debt_decimals` — BEP-20 decimals
    ///   (must be `<= 18`).
    /// - `expected_swap_output_wei` — DEX router quote for the seized
    ///   collateral -> debt swap, in debt-token wei. Slippage is
    ///   applied to this.
    /// - `flash_fee_wei` — absolute flash-loan fee denominated in the
    ///   debt token's wei (matches `FlashLoanQuote::fee`).
    /// - `gas_cost_cents` — pre-computed gas budget for the whole tx.
    /// - `slippage_bps` — DEX slippage budget (applied to swap output).
    ///
    /// # Unit path
    ///
    /// 1. `collateral_wei * collateral_price -> gross_collateral_cents`
    /// 2. `swap_output_wei * debt_price      -> expected_swap_output_cents`
    /// 3. `flash_fee_wei   * debt_price      -> flash_fee_cents`
    ///
    /// All three conversions go through [`wei_to_cents`], which stays
    /// in `U256` until the very last `try_into::<u64>()`.
    #[allow(clippy::too_many_arguments)]
    pub fn from_opportunity(
        opportunity: &LiquidationOpportunity,
        collateral_price: Price,
        debt_price: Price,
        collateral_decimals: u8,
        debt_decimals: u8,
        expected_swap_output_wei: U256,
        flash_fee_wei: U256,
        gas_cost_cents: u64,
        slippage_bps: u16,
    ) -> Result<Self, ProfitError> {
        if slippage_bps > 10_000 {
            return Err(ProfitError::InvalidBps(slippage_bps));
        }
        if opportunity.position.liquidation_bonus_bps > 10_000 {
            return Err(ProfitError::InvalidBps(
                opportunity.position.liquidation_bonus_bps,
            ));
        }

        // Gross collateral seized = expected_collateral_out priced at
        // the collateral feed. The on-chain liquidation flow already
        // writes expected_collateral_out = debt_repaid * bonus /
        // collateral_price; we price it here directly.
        let gross_collateral_cents = wei_to_cents(
            opportunity.expected_collateral_out,
            collateral_price,
            collateral_decimals,
        )?;

        let expected_swap_output_cents =
            wei_to_cents(expected_swap_output_wei, debt_price, debt_decimals)?;

        let flash_fee_cents = wei_to_cents(flash_fee_wei, debt_price, debt_decimals)?;

        Ok(Self {
            gross_collateral_cents,
            expected_swap_output_cents,
            flash_fee_cents,
            gas_cost_cents,
            slippage_bps,
        })
    }
}

/// Compute net profit for a candidate liquidation.
///
/// Returns `Err` whenever the liquidation is unprofitable — either the
/// total cost (flash fee + gas + slippage) swallows the gross bonus, or
/// the net falls below `min_profit_usd_1e6`. The caller is expected to
/// drop the opportunity on `Err`; no partial state is ever emitted.
///
/// `min_profit_usd_1e6` is in **micro-USD** to match
/// [`crate::config::BotConfig::min_profit_usd_1e6`]. It is converted to
/// cents (`/ 10_000`) internally.
pub fn calculate_profit(
    inputs: &ProfitInputs,
    min_profit_usd_1e6: u64,
) -> Result<NetProfit, ProfitError> {
    if inputs.slippage_bps > 10_000 {
        return Err(ProfitError::InvalidBps(inputs.slippage_bps));
    }

    // Slippage is charged on the DEX swap output, not on gross seized
    // collateral — the bot only pays slippage on the swap it performs.
    let slippage_mul = inputs
        .expected_swap_output_cents
        .checked_mul(u64::from(inputs.slippage_bps))
        .ok_or(ProfitError::Overflow)?;
    // 10_000 is a non-zero constant so the division is infallible.
    let slippage_cents = slippage_mul / 10_000;

    let total_cost_cents = inputs
        .flash_fee_cents
        .checked_add(inputs.gas_cost_cents)
        .and_then(|v| v.checked_add(slippage_cents))
        .ok_or(ProfitError::Overflow)?;

    let gross_cents = inputs.gross_collateral_cents;

    if gross_cents <= total_cost_cents {
        return Err(ProfitError::Unprofitable {
            gross_cents,
            total_cost_cents,
            flash_fee_cents: inputs.flash_fee_cents,
            gas_cost_cents: inputs.gas_cost_cents,
            slippage_cents,
        });
    }

    // gross > total_cost (checked above); use checked_sub to keep the
    // invariant local to the subtraction and satisfy
    // arithmetic_side_effects lints.
    let net_cents = gross_cents
        .checked_sub(total_cost_cents)
        .ok_or(ProfitError::Overflow)?;

    // 1 USD = 1e6 micro-USD = 100 cents => cents = micro / 10_000.
    // Integer division rounds down, which biases the threshold *up*
    // slightly (stricter, never looser — correct direction).
    let min_cents = min_profit_usd_1e6 / 10_000;
    if net_cents < min_cents {
        return Err(ProfitError::BelowMinThreshold {
            net_cents,
            min_cents,
        });
    }

    Ok(NetProfit {
        gross_usd_cents: gross_cents,
        flash_fee_usd_cents: inputs.flash_fee_cents,
        gas_cost_usd_cents: inputs.gas_cost_cents,
        slippage_usd_cents: slippage_cents,
        net_usd_cents: net_cents,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FlashLoanSource, LiquidationOpportunity, Position, ProtocolId, SwapRoute};
    use alloy::primitives::{Address, U256, address};

    fn typical_inputs() -> ProfitInputs {
        // $1 000 debt, 10% bonus => $1 100 gross collateral.
        // Swap output ~= $1 090 (pool curvature); 50 bps = $5.45.
        ProfitInputs {
            gross_collateral_cents: 110_000,
            expected_swap_output_cents: 109_000,
            flash_fee_cents: 50, // $0.50 (Aave V3 0.05% on $1 000)
            gas_cost_cents: 200, // $2 gas
            slippage_bps: 50,    // 0.5% of swap output
        }
    }

    #[test]
    fn healthy_liquidation_is_profitable() {
        let np = calculate_profit(&typical_inputs(), 5_000_000).expect("profitable");
        assert_eq!(np.gross_usd_cents, 110_000);
        // slippage = 109_000 * 50 / 10_000 = 545
        assert_eq!(np.slippage_usd_cents, 545);
        // net = 110_000 - 50 - 200 - 545 = 109_205
        assert_eq!(np.net_usd_cents, 109_205);
    }

    #[test]
    fn below_threshold_is_rejected() {
        let err = calculate_profit(&typical_inputs(), 2_000_000_000).expect_err("should reject");
        assert!(matches!(err, ProfitError::BelowMinThreshold { .. }));
    }

    #[test]
    fn cost_greater_than_gross_is_rejected() {
        let inputs = ProfitInputs {
            gross_collateral_cents: 10, // $0.10 gross
            expected_swap_output_cents: 10,
            flash_fee_cents: 1,
            gas_cost_cents: 200,
            slippage_bps: 50,
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
        // 10_000 bps = 100% slippage — numerically valid input; it may
        // render the trade unprofitable, but the bps check itself must
        // pass.
        inputs.slippage_bps = 10_000;
        // 10_000 bps = 100% of expected_swap_output. With
        // gross=$1 100 and swap_output=$1 090 that still leaves
        // $1 100 - $10.90 slippage - $0.50 fee - $2 gas = $7.50 net,
        // so the calculator accepts it. The point of the boundary
        // test is that 10_000 is a *valid* bps input (no InvalidBps).
        let np = calculate_profit(&inputs, 0).expect("10_000 bps is valid input");
        assert_eq!(np.slippage_usd_cents, inputs.expected_swap_output_cents);

        inputs.slippage_bps = 10_001;
        assert!(matches!(
            calculate_profit(&inputs, 0),
            Err(ProfitError::InvalidBps(10_001))
        ));
    }

    #[test]
    fn min_profit_zero_accepts_any_positive_net() {
        let inputs = ProfitInputs {
            gross_collateral_cents: 1_000,
            expected_swap_output_cents: 1_000,
            flash_fee_cents: 30,
            gas_cost_cents: 15,
            slippage_bps: 50,
        };
        let np = calculate_profit(&inputs, 0).expect("profitable");
        assert!(np.net_usd_cents > 0);
    }

    #[test]
    fn u64_max_gross_does_not_overflow_slippage_path() {
        // The slippage path multiplies by up to 10_000 before dividing.
        // u64::MAX * 10_000 *would* wrap — so we expect Overflow, not a
        // silent truncation.
        let inputs = ProfitInputs {
            gross_collateral_cents: u64::MAX,
            expected_swap_output_cents: u64::MAX,
            flash_fee_cents: 0,
            gas_cost_cents: 0,
            slippage_bps: 50,
        };
        assert!(matches!(
            calculate_profit(&inputs, 0),
            Err(ProfitError::Overflow)
        ));
    }

    #[test]
    fn total_cost_addition_overflow_is_reported() {
        let inputs = ProfitInputs {
            gross_collateral_cents: u64::MAX,
            expected_swap_output_cents: 0, // skip the slippage branch
            flash_fee_cents: u64::MAX,
            gas_cost_cents: 1, // u64::MAX + 1 -> overflow
            slippage_bps: 0,
        };
        assert!(matches!(
            calculate_profit(&inputs, 0),
            Err(ProfitError::Overflow)
        ));
    }

    // ── from_opportunity / wei->cents path ──────────────────────────

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
                pool_fee: 0,
            },
            net_profit_usd_cents: 0,
        }
    }

    #[test]
    fn bsc_bnb_one_token_at_600_usd_prices_to_600_dollars() {
        // 1 BNB repay, matching-asset collateral, 10% bonus, $600 price
        let one_bnb = U256::from(1_000_000_000_000_000_000u128);
        let one_point_one_bnb = one_bnb * U256::from(11u64) / U256::from(10u64);
        let opp = mk_opp(one_point_one_bnb, one_bnb, 1_000);
        let price = Price::new(60_000_000_000).expect("valid"); // $600

        // Swap output ~= 1.1 BNB worth of debt; flash fee = 0.05% of
        // 1 BNB = 0.0005 BNB.
        let flash_fee_wei = one_bnb / U256::from(2_000u64);

        let inputs = ProfitInputs::from_opportunity(
            &opp,
            price,
            price,
            18,
            18,
            one_point_one_bnb,
            flash_fee_wei,
            200,
            50,
        )
        .expect("valid");

        assert_eq!(inputs.gross_collateral_cents, 66_000); // $660 = 1.1 * $600
        assert_eq!(inputs.expected_swap_output_cents, 66_000);
        assert_eq!(inputs.flash_fee_cents, 30); // 0.0005 BNB * $600 = $0.30

        let np = calculate_profit(&inputs, 0).expect("profitable");
        // slippage = 66_000 * 50 / 10_000 = 330
        // net = 66_000 - 30 - 200 - 330 = 65_440  (~ $654)
        assert_eq!(np.net_usd_cents, 65_440);
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
                price,
                price,
                19, // invalid
                18,
                U256::from(1u64),
                U256::from(0u64),
                0,
                0,
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
                price,
                price,
                18,
                18,
                U256::from(1u64),
                U256::from(0u64),
                0,
                0,
            ),
            Err(ProfitError::InvalidBps(10_001))
        ));
    }
}
