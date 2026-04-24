//! Charon core — shared types, traits, and config.

pub mod config;
pub mod flashloan;
pub mod traits;
pub mod types;

pub use config::{Config, ConfigError};
pub use flashloan::{FlashLoanError, FlashLoanProvider, FlashLoanQuote};
pub use traits::{LendingProtocol, LendingProtocolError, Result as LendingResult};
pub use types::{
    FlashLoanSource, LiquidationOpportunity, LiquidationParams, Position, ProtocolId, SwapRoute,
};
