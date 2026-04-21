//! Flash-loan source adapters + router.
//!
//! One module per source. Each implements
//! [`charon_core::FlashLoanProvider`] so the router can treat them
//! uniformly. For v0.1 only the Aave V3 adapter on BNB Chain is wired
//! up; Balancer V2 and Uniswap V3 flash-swap adapters land alongside
//! multi-chain expansion.

pub mod aave;
pub mod router;

pub use aave::AaveFlashLoan;
pub use router::FlashLoanRouter;
