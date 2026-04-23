//! Charon core — shared types, traits, and config.

pub mod config;
pub mod flashloan;
pub mod profit;
pub mod queue;
pub mod traits;
pub mod types;

pub use config::Config;
pub use flashloan::{FlashLoanProvider, FlashLoanQuote};
pub use profit::{NetProfit, Price, ProfitError, ProfitInputs, calculate_profit};
pub use queue::{DEFAULT_TTL_BLOCKS, OpportunityQueue, QueueEntry};
pub use traits::LendingProtocol;
pub use types::{
    FlashLoanSource, LiquidationOpportunity, LiquidationParams, Position, ProtocolId, SwapRoute,
};
