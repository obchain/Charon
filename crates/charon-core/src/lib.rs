//! Charon core — shared types, traits, and config.

pub mod traits;
pub mod types;

pub use traits::LendingProtocol;
pub use types::{
    FlashLoanSource, LiquidationOpportunity, LiquidationParams, Position,
    ProtocolId, SwapRoute,
};
