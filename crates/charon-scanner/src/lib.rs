//! Charon scanner — chain listener, health-factor scanner, and price cache.

pub mod listener;
pub mod mempool;
pub mod oracle;
pub mod provider;
pub mod scanner;

pub use listener::{BlockListener, ChainEvent};
pub use mempool::{
    DEFAULT_MAX_PENDING_AGE, FIRST_TX_WATCHDOG, MempoolError, MempoolMonitor, OracleUpdate,
    PendingCache, PreSignedLiquidation, SimulationVerdict, UnverifiedPreSigned, default_selectors,
    legacy_selectors,
};
pub use oracle::{CachedPrice, DEFAULT_MAX_AGE, PriceCache};
pub use provider::ChainProvider;
pub use scanner::{BucketCounts, BucketedPosition, HealthScanner, PositionBucket};
