//! Flash-loan provider abstraction.
//!
//! Every flash-loan source (Balancer V2, Aave V3, Uniswap V3, ...) plugs
//! in through the [`FlashLoanProvider`] trait. The router (in
//! `charon-flashloan`) walks a list of providers in fee-priority order
//! and picks the cheapest source with enough liquidity for the token +
//! amount it needs to borrow.
//!
//! # Fee-rate unit
//!
//! All fee rates in this module are expressed in **millionths (1e6)**,
//! the same convention Uniswap uses for pool-fee tiers. Examples:
//!
//! * Balancer V2 flash loan: `0` (fee-free)
//! * Aave V3 flash loan: `500` = 0.05%
//! * Uniswap V3 0.30% pool: `3_000`
//! * Uniswap V3 1.00% pool: `10_000`
//!
//! This intentionally differs from Aave's on-chain encoding, which
//! stores the premium as 4-decimal percent (e.g. `5` means 0.05%).
//! Adapters are expected to convert into millionths at construction.
//!
//! The trait is kept deliberately thin:
//!
//! * `available_liquidity` — can the source cover the requested amount?
//! * `fee_rate_millionths` — how expensive is borrowing from this source?
//! * `quote` — one-shot helper that rolls the two checks above into a
//!   ready-to-use [`FlashLoanQuote`], or `None` when the source cannot
//!   serve this borrow.
//! * `build_flashloan_calldata` — encode the outer call to the source
//!   (e.g. `Pool.flashLoanSimple`, `Vault.flashLoan`) that wraps the
//!   inner liquidation parameters the protocol adapter produced.

use alloy::primitives::{Address, U256};
use async_trait::async_trait;
use thiserror::Error;

use crate::types::FlashLoanSource;

/// Errors returned by [`FlashLoanProvider`] implementations.
///
/// The variants capture the failure modes the router needs to
/// distinguish (e.g. a paused reserve is not the same as a transient
/// RPC hiccup). `#[non_exhaustive]` so new sources can extend the
/// taxonomy without breaking downstream `match`es.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum FlashLoanError {
    /// The source does not hold enough liquidity of the requested
    /// token to cover the borrow.
    #[error("insufficient liquidity: have {have}, need {need}")]
    InsufficientLiquidity { have: U256, need: U256 },

    /// The reserve is paused or frozen on the source (Aave V3 pauses
    /// reserves during incidents, freezes them during deprecation).
    #[error("reserve paused or frozen for asset {asset}")]
    ReservePaused { asset: Address },

    /// The adapter was connected to the wrong chain.
    #[error("chain id mismatch: expected {expected}, got {actual}")]
    ChainIdMismatch { expected: u64, actual: u64 },

    /// An RPC call failed (timeout, provider error, decode failure, ...).
    #[error("rpc error: {0}")]
    Rpc(String),

    /// Any other unclassified failure.
    #[error("flash-loan provider error: {0}")]
    Other(String),
}

impl FlashLoanError {
    /// Convenience for wrapping an `anyhow::Error` into
    /// [`FlashLoanError::Rpc`] at call sites that still bubble up
    /// generic RPC failures.
    pub fn rpc<E: std::fmt::Display>(err: E) -> Self {
        Self::Rpc(err.to_string())
    }

    /// Convenience for unclassified failures.
    pub fn other<E: std::fmt::Display>(err: E) -> Self {
        Self::Other(err.to_string())
    }
}

/// Snapshot of a single flash-loan opportunity from one source.
///
/// The router produces these for the top-ranked liquidations; the tx
/// builder consumes them alongside the inner liquidation parameters to
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
    /// Fee rate in millionths (1e6). `500` = 0.05% (Aave V3). `0` =
    /// Balancer / Maker flash mint. See module docs for unit rationale.
    pub fee_rate_millionths: u32,
    /// Address to call to initiate the flash loan
    /// (Aave pool, Balancer vault, Uniswap pool, ...).
    pub pool_address: Address,
}

/// Flash-loan source adapter.
///
/// Implementations live in `charon-flashloan` (one per source) and are
/// consumed by the router as trait objects. The trait is `Send + Sync`
/// so a provider can be shared across the block listener, scanner, and
/// executor tasks without copying state.
///
/// All fees are returned in **millionths (1e6)** — see module docs.
#[async_trait]
pub trait FlashLoanProvider: Send + Sync {
    /// Which concrete source this provider wraps.
    fn source(&self) -> FlashLoanSource;

    /// Chain id the source is deployed on.
    fn chain_id(&self) -> u64;

    /// Current liquidity available for `token`, in its smallest unit.
    /// Returns `0` when the source does not support the token at all,
    /// or [`FlashLoanError::ReservePaused`] when the reserve is
    /// administratively disabled.
    async fn available_liquidity(&self, token: Address) -> Result<U256, FlashLoanError>;

    /// Fee rate in millionths (1e6). `500` = 0.05% (Aave V3). `0` =
    /// Balancer / Maker flash mint. See module docs.
    fn fee_rate_millionths(&self) -> u32;

    /// Effective fee in millionths for a *specific* `(token, amount,
    /// liquidity)` borrow — including utilisation-driven slippage on
    /// the source pool itself, not just the static fee tier.
    ///
    /// Default impl ignores `amount` / `liquidity` and returns
    /// [`fee_rate_millionths`] so existing adapters (and the
    /// single-provider Aave-only configuration) keep working
    /// unchanged. Adapters with a meaningful slippage penalty
    /// (Aave V3 utilisation curve, Balancer pool depth, …) override.
    ///
    /// Why a default: the trait is `#[non_exhaustive]`-friendly via
    /// the default method, so a future provider that does *not*
    /// model utilisation does not have to invent a fake penalty.
    fn effective_fee_millionths(&self, _token: Address, amount: U256, liquidity: U256) -> u32 {
        let _ = (amount, liquidity);
        self.fee_rate_millionths()
    }

    /// Roll `available_liquidity` + `fee_rate_millionths` into a ready
    /// quote for the requested borrow, or `None` when the source
    /// cannot cover the amount on this chain/token.
    async fn quote(
        &self,
        token: Address,
        amount: U256,
    ) -> Result<Option<FlashLoanQuote>, FlashLoanError>;

    /// Encode the outer flash-loan initiation call.
    ///
    /// `liquidation_params` is whatever the flash-loan recipient
    /// contract (`CharonLiquidator.sol`) needs inside its callback —
    /// typically the ABI-encoded protocol adapter liquidation
    /// parameters. It MUST be non-empty; every real
    /// `executeOperation` implementation decodes this payload and will
    /// revert on an empty buffer.
    fn build_flashloan_calldata(
        &self,
        quote: &FlashLoanQuote,
        liquidation_params: &[u8],
    ) -> Result<Vec<u8>, FlashLoanError>;
}
