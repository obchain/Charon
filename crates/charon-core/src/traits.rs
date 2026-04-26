//! Traits that define the contracts between layers of the bot.
//!
//! `LendingProtocol` is the main boundary: each protocol (Venus, Aave, …)
//! implements it, and the scanner/executor consume it without caring which
//! protocol is behind the adapter.

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::{Address, U256};
use async_trait::async_trait;

use crate::types::{LiquidationParams, Position, ProtocolId};

/// Structured error for lending-protocol adapter operations.
///
/// Callers match on the variant to decide retry vs skip vs abort:
/// - `Rpc` — transient transport failure, retry with backoff.
/// - `InvalidPosition` — borrower data is malformed or inconsistent, skip.
/// - `UnsupportedAsset` — asset not listed on this protocol, skip market.
/// - `ProtocolState` — an invariant broken (e.g. vToken ↔ underlying mismatch),
///   escalate / alert.
/// - `Abi` — calldata encode/decode failure, treat as bug.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LendingProtocolError {
    #[error("rpc error: {0}")]
    Rpc(String),
    #[error("invalid position: {0}")]
    InvalidPosition(String),
    #[error("unsupported asset: {0}")]
    UnsupportedAsset(Address),
    #[error("protocol state: {0}")]
    ProtocolState(String),
    #[error("abi: {0}")]
    Abi(String),
}

/// Shorthand `Result` for trait methods.
pub type Result<T> = std::result::Result<T, LendingProtocolError>;

/// A lending protocol adapter.
///
/// - Scanner calls [`fetch_positions`](LendingProtocol::fetch_positions) on
///   each block (or on relevant events) to pick up health-factor changes.
/// - Scanner also calls [`get_health_factor`](LendingProtocol::get_health_factor)
///   for single-borrower refreshes on mempool oracle updates.
/// - Executor calls [`get_liquidation_params`](LendingProtocol::get_liquidation_params)
///   and [`build_liquidation_calldata`](LendingProtocol::build_liquidation_calldata)
///   when a position crosses the liquidation threshold, to encode the
///   on-chain call to `CharonLiquidator.executeLiquidation(...)`.
///
/// Implementations must be `Send + Sync` so the scanner can hold them in
/// `Arc<dyn LendingProtocol>` and share across tokio tasks.
///
/// `#[async_trait]` is used for `dyn`-compatibility. Native AFIT (Rust 1.75+)
/// is not object-safe without `trait_variant` bounds, and the scanner stores
/// protocol adapters behind `Arc<dyn>`; the per-call boxing overhead is
/// negligible relative to the RPC round-trip dominating every method.
#[async_trait]
pub trait LendingProtocol: Send + Sync {
    /// Stable identifier for this protocol.
    fn id(&self) -> ProtocolId;

    /// Fetch current position state for the given borrowers, pinned to
    /// a specific block for snapshot consistency.
    ///
    /// Scanner passes `BlockNumberOrTag::Latest` for routine scans and a
    /// specific block number to reconcile state against an observed head.
    /// Mempool-driven paths pass `BlockNumberOrTag::Pending`.
    async fn fetch_positions(
        &self,
        borrowers: &[Address],
        block: BlockNumberOrTag,
    ) -> Result<Vec<Position>>;

    /// Return the 1e18-scaled health factor for a single borrower at `block`.
    ///
    /// Cheaper than `fetch_positions` for gating decisions (e.g. mempool
    /// monitor refresh) because only one aggregate call is needed.
    async fn get_health_factor(&self, borrower: Address, block: BlockNumberOrTag) -> Result<U256>;

    /// Close factor for a market, 1e18-scaled.
    ///
    /// Venus default is 0.5e18 (50% of debt per liquidation) but is
    /// per-market and governed. Aave V3 caps at 0.5e18 structurally.
    /// Required by the profit calculator to bound `repay_amount`.
    fn get_close_factor(&self, market: Address) -> Result<U256>;

    /// Liquidation incentive for the given collateral market, 1e18-scaled.
    ///
    /// Venus uses `liquidationIncentiveMantissa` (default 1.1e18 = 10%
    /// bonus) and it is per-market. Aave uses `LiquidationBonus` in bps.
    /// Profit calculator scales seized collateral by this value.
    async fn get_liquidation_incentive(&self, collateral_market: Address) -> Result<U256>;

    /// Compute protocol-specific liquidation parameters for a position.
    ///
    /// Handles close-factor math (Aave's 50% cap, Compound's 100% absorb,
    /// etc.) and resolves any protocol-specific token addresses (e.g., Venus
    /// vToken addresses).
    fn get_liquidation_params(&self, position: &Position) -> Result<LiquidationParams>;

    /// Encode the ABI calldata for `CharonLiquidator.executeLiquidation(...)`.
    fn build_liquidation_calldata(&self, params: &LiquidationParams) -> Result<Vec<u8>>;
}
