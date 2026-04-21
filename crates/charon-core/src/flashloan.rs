//! Flash-loan provider abstraction.
//!
//! Every flash-loan source (Balancer V2, Aave V3, Uniswap V3, …) plugs
//! in through the [`FlashLoanProvider`] trait. The router (in
//! `charon-flashloan`) walks a list of providers in fee-priority order
//! and picks the cheapest source with enough liquidity for the token +
//! amount it needs to borrow.
//!
//! The trait is kept deliberately thin:
//!
//! * `available_liquidity` — can the source cover the requested amount?
//! * `fee_rate` — how expensive is borrowing from this source?
//! * `quote` — one-shot helper that rolls the two checks above into a
//!   ready-to-use [`FlashLoanQuote`], or `None` when the source cannot
//!   serve this borrow.
//! * `build_flashloan_calldata` — encode the outer call to the source
//!   (e.g. `Pool.flashLoanSimple`, `Vault.flashLoan`) that wraps the
//!   inner liquidation calldata the protocol adapter produced.

use alloy::primitives::{Address, U256};
use async_trait::async_trait;

use crate::types::FlashLoanSource;

/// Snapshot of a single flash-loan opportunity from one source.
///
/// The router produces these for the top-ranked liquidations; the tx
/// builder consumes them alongside the inner liquidation calldata to
/// encode the final on-chain call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlashLoanQuote {
    pub source: FlashLoanSource,
    /// Chain the source is deployed on.
    pub chain_id: u64,
    /// Token being borrowed (the debt token of the liquidation target).
    pub token: Address,
    /// Amount to borrow, in the token's smallest unit.
    pub amount: U256,
    /// Absolute fee to repay alongside `amount`, same units as `amount`.
    pub fee: U256,
    /// Fee rate in basis points (e.g. `5` = 0.05%). Balancer is `0`.
    pub fee_bps: u16,
    /// Address to call to initiate the flash loan
    /// (Aave pool, Balancer vault, Uniswap pool, …).
    pub pool_address: Address,
}

/// Flash-loan source adapter.
///
/// Implementations live in `charon-flashloan` (one per source) and are
/// consumed by the router as trait objects. The trait is `Send + Sync`
/// so a provider can be shared across the block listener, scanner, and
/// executor tasks without copying state.
#[async_trait]
pub trait FlashLoanProvider: Send + Sync {
    /// Which concrete source this provider wraps.
    fn source(&self) -> FlashLoanSource;

    /// Chain id the source is deployed on.
    fn chain_id(&self) -> u64;

    /// Current liquidity available for `token`, in its smallest unit.
    /// Returns `0` when the source does not support the token at all.
    async fn available_liquidity(&self, token: Address) -> anyhow::Result<U256>;

    /// Fee rate in basis points. `5` = 0.05% (Aave V3). `0` = Balancer /
    /// Maker flash mint.
    fn fee_rate_bps(&self) -> u16;

    /// Roll `available_liquidity` + `fee_rate_bps` into a ready quote
    /// for the requested borrow, or `None` when the source cannot cover
    /// the amount on this chain/token.
    async fn quote(&self, token: Address, amount: U256) -> anyhow::Result<Option<FlashLoanQuote>>;

    /// Encode the outer flash-loan initiation call. `inner_calldata` is
    /// whatever the flash-loan recipient contract (`CharonLiquidator.sol`)
    /// needs inside its callback — typically the protocol adapter's
    /// liquidation calldata.
    fn build_flashloan_calldata(
        &self,
        quote: &FlashLoanQuote,
        inner_calldata: &[u8],
    ) -> anyhow::Result<Vec<u8>>;
}
