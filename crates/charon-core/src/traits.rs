//! Traits that define the contracts between layers of the bot.
//!
//! `LendingProtocol` is the main boundary: each protocol (Venus, Aave, …)
//! implements it, and the scanner/executor consume it without caring which
//! protocol is behind the adapter.

use alloy::primitives::Address;
use async_trait::async_trait;

use crate::types::{LiquidationParams, Position, ProtocolId};

/// A lending protocol adapter.
///
/// - Scanner calls [`fetch_positions`](LendingProtocol::fetch_positions) on
///   each block (or on relevant events) to pick up health-factor changes.
/// - Executor calls [`get_liquidation_params`](LendingProtocol::get_liquidation_params)
///   and [`build_liquidation_calldata`](LendingProtocol::build_liquidation_calldata)
///   when a position crosses the liquidation threshold, to encode the
///   on-chain call to `CharonLiquidator.executeLiquidation(...)`.
#[async_trait]
pub trait LendingProtocol: Send + Sync {
    /// Stable identifier for this protocol.
    fn id(&self) -> ProtocolId;

    /// Fetch current position state for the given borrowers.
    ///
    /// The scanner is responsible for maintaining the list of tracked
    /// borrowers; this method is a pure query over protocol state.
    async fn fetch_positions(&self, borrowers: &[Address]) -> anyhow::Result<Vec<Position>>;

    /// Compute protocol-specific liquidation parameters for a position.
    ///
    /// Handles close-factor math (Aave's 50% cap, Compound's 100% absorb,
    /// etc.) and resolves any protocol-specific token addresses (e.g., Venus
    /// vToken addresses).
    fn get_liquidation_params(&self, position: &Position) -> anyhow::Result<LiquidationParams>;

    /// Encode the ABI calldata for `CharonLiquidator.executeLiquidation(...)`.
    fn build_liquidation_calldata(&self, params: &LiquidationParams) -> anyhow::Result<Vec<u8>>;
}
