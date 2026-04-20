//! Normalized types shared across the workspace.
//!
//! Every lending protocol is reduced to the same shape here so the scanner
//! and executor can be protocol-agnostic.

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

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

/// A profitable liquidation that has passed all off-chain gates and is
/// ready to be built into a transaction.
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
    pub net_profit_usd_cents: u64,
}
