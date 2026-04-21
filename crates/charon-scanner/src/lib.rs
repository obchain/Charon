//! Charon scanner — chain listener, health-factor scanner, and price cache.

pub mod listener;
pub mod oracle;
pub mod provider;
pub mod scanner;

pub use listener::{BlockListener, ChainEvent};
pub use oracle::{CachedPrice, DEFAULT_MAX_AGE, PriceCache};
pub use provider::ChainProvider;
pub use scanner::{BucketCounts, BucketedPosition, HealthScanner, PositionBucket};
