//! Normalized types shared across the workspace.
//!
//! Every lending protocol is reduced to the same shape here so the scanner
//! and executor can be protocol-agnostic.

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// Which lending protocol a position belongs to.
///
/// Only `Venus` for v1. Additional variants are added as adapters are
/// implemented (AaveV3, CompoundV3, Morpho, …). Marked `#[non_exhaustive]`
/// so adding variants in future is not a semver-breaking change for
/// downstream exhaustive matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
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
    /// Which lending protocol produced this snapshot.
    pub protocol: ProtocolId,
    /// EVM chain id (e.g. BSC mainnet = 56).
    pub chain_id: u64,
    /// Borrower account whose health is being tracked.
    pub borrower: Address,
    /// Collateral asset address in the underlying-token space (not vToken/aToken).
    pub collateral_token: Address,
    /// Debt asset address in the underlying-token space.
    pub debt_token: Address,
    /// Collateral balance in the underlying token's base units (wei-equivalent).
    pub collateral_amount: U256,
    /// Outstanding debt in the debt token's base units (wei-equivalent).
    pub debt_amount: U256,
    /// Health factor scaled by 1e18 (Aave-style fixed point).
    /// `health_factor < 1e18` means the position is liquidatable.
    pub health_factor: U256,
    /// Liquidation bonus in basis points (e.g. 500 = 5%).
    /// Sourced per-market from the protocol (Venus `liquidationIncentiveMantissa`,
    /// Aave `LiquidationBonus`, etc.). Never hardcoded.
    pub liquidation_bonus_bps: u16,
}

/// Where the flash loan capital comes from for a liquidation.
///
/// Router picks cheapest available. BSC sources only for v0.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum FlashLoanSource {
    /// Aave V3 Pool on BSC — 0.05% fee via `flashLoanSimple`.
    AaveV3,
    /// PancakeSwap V3 flash-swap — pool fee tier applies (100 / 500 / 2500 / 10000 bps).
    PancakeSwapV3,
}

/// A planned swap: seized collateral → debt token, used to repay the flash loan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapRoute {
    /// Token being sold (typically seized collateral).
    pub token_in: Address,
    /// Token being bought (typically the flash-loaned debt token).
    pub token_out: Address,
    /// Amount of `token_in` swapped, in its base units.
    pub amount_in: U256,
    /// Slippage-protected minimum output. Tx reverts if the DEX returns less.
    pub min_amount_out: U256,
    /// PancakeSwap V3 pool fee tier in hundredths of a bip (100 / 500 / 2500 / 10000).
    /// `None` for fee-less routes (e.g. Balancer V2, Curve stable pool).
    pub pool_fee: Option<u32>,
}

/// Protocol-specific parameters needed to build a liquidation call.
///
/// Every lending protocol has its own quirks (Aave allows partial liquidation,
/// Compound absorbs 100%, Venus uses vToken addresses, etc.). Each variant
/// captures exactly the fields its protocol needs — no shared bag of options.
///
/// Marked `#[non_exhaustive]` at both enum and variant level so adding new
/// variants (AaveV3, Compound, …) or new fields on existing variants is not
/// a semver-breaking change for downstream exhaustive matches.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum LiquidationParams {
    // NOTE: this variant is intentionally *not* `#[non_exhaustive]`.
    // The `LendingProtocol::get_liquidation_params` trait method is
    // implemented outside `charon-core` (each adapter crate returns a
    // protocol-specific variant), which requires each variant to be
    // constructible via a struct expression from downstream crates.
    // Enum-level `#[non_exhaustive]` still prevents breakage from adding
    // new variants; that is the only forward-compat guarantee we need
    // here. Adding or renaming a field on `Venus` is a semver break by
    // design — every adapter call site must be audited when liquidation
    // mechanics change.
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[must_use]
pub struct LiquidationOpportunity {
    /// Underlying position being liquidated.
    pub position: Position,
    /// How much of the debt to repay in debt-token base units
    /// (Aave: up to 50%, Compound/Venus: up to close_factor × debt).
    pub debt_to_repay: U256,
    /// Expected collateral seized after liquidation bonus, in collateral-token base units.
    pub expected_collateral_out: U256,
    /// Flash-loan source selected by the router.
    pub flash_source: FlashLoanSource,
    /// Pre-computed swap route for collateral → debt token.
    pub swap_route: SwapRoute,
    /// Estimated net profit in debt-token base units (wei), after gas, flash fee,
    /// and slippage. Token-native to avoid f64 / USD-cent precision loss; USD
    /// display is a reporting-layer concern only.
    pub net_profit_wei: U256,
}

impl Ord for LiquidationOpportunity {
    /// Ranks opportunities by `net_profit_wei` ascending so that a
    /// `BinaryHeap<LiquidationOpportunity>` pops the highest-profit entry first.
    fn cmp(&self, other: &Self) -> Ordering {
        self.net_profit_wei.cmp(&other.net_profit_wei)
    }
}

impl PartialOrd for LiquidationOpportunity {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for LiquidationOpportunity {
    fn eq(&self, other: &Self) -> bool {
        self.net_profit_wei == other.net_profit_wei
            && self.position.borrower == other.position.borrower
            && self.position.chain_id == other.position.chain_id
    }
}

impl Eq for LiquidationOpportunity {}
