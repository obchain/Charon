//! Charon core — shared types, traits, and config.

pub mod config;
pub mod flashloan;
pub mod profit;
pub mod queue;
pub mod traits;
pub mod types;

pub use config::{Config, MetricsConfig};
pub use flashloan::{FlashLoanProvider, FlashLoanQuote};
pub use profit::{NetProfit, ProfitInputs, calculate_profit};
pub use queue::{DEFAULT_TTL_BLOCKS, OpportunityQueue};
pub use traits::LendingProtocol;
pub use types::{
    FlashLoanSource, LiquidationOpportunity, LiquidationParams, Position, ProtocolId, SwapRoute,
};
