//! Charon scanner — chain listener, health-factor scanner, and price cache.

// Tests panic on bad fixtures by design; `unwrap_used = "deny"` at the
// workspace level is for production code.
#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod discovery;
pub mod listener;
pub mod mempool;
pub mod oracle;
pub mod provider;
pub mod scanner;
pub mod token_meta;

pub use discovery::{
    BORROW_TOPIC0, BorrowerInfo, BorrowerSet, DEFAULT_BACKFILL_BLOCKS, DISCOVERY_CHANNEL_CAPACITY,
    MAX_LOG_CHUNK_BLOCKS, backfill_borrowers, decode_borrow_borrower, run_discovery_live_once,
    run_discovery_live_with_reconnect,
};
pub use listener::{BlockListener, ChainEvent};
pub use mempool::{
    DEFAULT_MAX_PENDING_AGE, FIRST_TX_WATCHDOG, MempoolError, MempoolMonitor, OracleUpdate,
    PendingCache, PreSignedLiquidation, SimulationVerdict, UnverifiedPreSigned, default_selectors,
    legacy_selectors,
};
pub use oracle::{CachedPrice, DEFAULT_MAX_AGE, PriceCache};
pub use provider::{ChainProvider, ChainProviderT, MockChainProvider};
pub use scanner::{BucketCounts, BucketedPosition, HealthScanner, PositionBucket, ScanScheduler};
pub use token_meta::{TokenMeta, TokenMetaCache};
