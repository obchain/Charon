//! Venus Protocol adapter (BNB Chain).
//!
//! Venus is a Compound V2 fork running on BSC. Underwater accounts are
//! surfaced via `Comptroller.getAccountLiquidity(borrower)` which returns
//! a `(errorCode, liquidity, shortfall)` tuple; a non-zero `shortfall`
//! means the account is liquidatable. The adapter translates that shape
//! into the shared `Position` type and encodes liquidation calls through
//! `VToken.liquidateBorrow(borrower, repayAmount, vTokenCollateral)`.
//!
//! This file is a scaffold — ABIs, provider wiring, and the
//! [`LendingProtocol`](charon_core::LendingProtocol) implementation land
//! across the next commits in the #8 series.

use alloy::primitives::Address;

/// Venus adapter — see module docs.
///
/// Holds the Comptroller address for the chain it's running on. Further
/// fields (pub-sub provider, cached vToken list, price oracle address)
/// are added alongside the ABI bindings in the next commit.
#[derive(Debug, Clone)]
pub struct VenusAdapter {
    /// Address of the Venus Unitroller (main Comptroller proxy).
    pub comptroller: Address,
}

impl VenusAdapter {
    /// Build an adapter pointing at the given Venus Comptroller.
    ///
    /// This is intentionally minimal for now; the async constructor that
    /// also discovers vToken markets and the price oracle lands in the
    /// next commit.
    pub fn new(comptroller: Address) -> Self {
        Self { comptroller }
    }
}
