//! Normalized types shared across the workspace.
//!
//! Every lending protocol is reduced to the same shape here so the scanner
//! and executor can be protocol-agnostic.

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

use crate::profit::NetProfit;

/// Which lending protocol a position belongs to.
///
/// Only `Venus` for v1. Additional variants are added as adapters are
/// implemented (AaveV3, CompoundV3, Morpho, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProtocolId {
    Venus,
}

/// A single borrow position on a lending protocol, normalized across protocols.
///
/// `health_factor` and `liquidation_bonus_bps` are the two fields the scanner
/// uses to decide (a) whether the position can be liquidated and (b) how
/// profitable it would be.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub protocol: ProtocolId,
    pub chain_id: u64,
    pub borrower: Address,
    pub collateral_token: Address,
    pub debt_token: Address,
    pub collateral_amount: U256,
    pub debt_amount: U256,
    /// Health factor scaled by 1e18 (Aave-style fixed point).
    /// `health_factor < 1e18` means the position is liquidatable.
    pub health_factor: U256,
    /// Liquidation bonus in basis points (e.g. 500 = 5%).
    pub liquidation_bonus_bps: u16,
}

/// Where the flash loan capital comes from for a liquidation.
///
/// Router picks cheapest available: Balancer (0%) → Aave (0.05%) → Uniswap (pool fee).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlashLoanSource {
    /// Balancer V2 Vault — 0% fee.
    BalancerV2,
    /// Aave V3 Pool — 0.05% fee via `flashLoanSimple`.
    AaveV3,
    /// Uniswap V3 flash swap — pool fee tier applies.
    UniswapV3,
}

/// A planned swap: seized collateral → debt token, used to repay the flash loan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapRoute {
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    /// Slippage-protected minimum output. Tx reverts if DEX returns less.
    pub min_amount_out: U256,
    /// Uniswap V3 pool fee tier (500 / 3000 / 10000). 0 = not applicable.
    pub pool_fee: u32,
}

/// Protocol-specific parameters needed to build a liquidation call.
///
/// Every lending protocol has its own quirks (Aave allows partial liquidation,
/// Compound absorbs 100%, Venus uses vToken addresses, etc.). Each variant
/// captures exactly the fields its protocol needs — no shared bag of options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LiquidationParams {
    Venus {
        borrower: Address,
        /// vToken of the collateral asset (the token seized).
        collateral_vtoken: Address,
        /// vToken of the debt asset (the token repaid).
        debt_vtoken: Address,
        /// Amount of debt to repay, in underlying-debt-token units (not vToken units).
        repay_amount: U256,
    },
}

/// A profitable liquidation that has passed all off-chain gates and is
/// ready to be built into a transaction.
///
/// `net_profit_usd_cents` must always equal `NetProfit::net_usd_cents`
/// of the profit calculation that produced this opportunity — this is
/// the invariant [`LiquidationOpportunity::with_profit`] enforces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidationOpportunity {
    pub position: Position,
    /// How much of the debt to repay (Aave: up to 50%, Compound/Morpho: 100%).
    pub debt_to_repay: U256,
    /// Expected collateral seized after liquidation bonus.
    pub expected_collateral_out: U256,
    pub flash_source: FlashLoanSource,
    pub swap_route: SwapRoute,
    /// Estimated net profit in USD cents, after gas + flash fee + slippage.
    ///
    /// Use [`LiquidationOpportunity::with_profit`] to construct an
    /// opportunity from a [`NetProfit`] so this field is guaranteed to
    /// match the upstream calculator output.
    pub net_profit_usd_cents: u64,
}

impl LiquidationOpportunity {
    /// Build an opportunity, copying `net_profit.net_usd_cents` into
    /// `net_profit_usd_cents`.
    ///
    /// This is the **only** way to produce an opportunity that carries
    /// a profit figure consistent with the calculator — calling sites
    /// should use this instead of setting the field by hand.
    pub fn with_profit(
        position: Position,
        debt_to_repay: U256,
        expected_collateral_out: U256,
        flash_source: FlashLoanSource,
        swap_route: SwapRoute,
        net_profit: NetProfit,
    ) -> Self {
        Self {
            position,
            debt_to_repay,
            expected_collateral_out,
            flash_source,
            swap_route,
            net_profit_usd_cents: net_profit.net_usd_cents,
        }
    }
}
