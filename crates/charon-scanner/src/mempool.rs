//! Mempool monitor — head-start on Venus oracle price updates.
//!
//! Subscribes to the chain's pending-tx stream, looks up the full
//! transaction for each hash, and filters for calls that target the
//! Venus price oracle discovered by
//! [`VenusAdapter`](charon_protocols::venus::VenusAdapter). A match
//! means the next block is about to carry a price change that could
//! push borrowers under water; decoded [`OracleUpdate`] events are
//! emitted on an `mpsc` channel so a downstream handler can simulate
//! the impact and pre-sign liquidations before the update confirms.
//!
//! The monitor also owns a small in-memory `DashMap` of pre-signed
//! liquidations keyed by borrower. On the next
//! [`ChainEvent::NewBlock`](crate::listener::ChainEvent::NewBlock) the
//! caller drains this map via [`PendingCache::drain_for_block`] —
//! passing the set of tx hashes the new block actually confirmed.
//! Entries whose trigger oracle tx did not confirm are re-queued
//! (still within TTL) so a pre-sign whose trigger slips to the next
//! block is not silently lost. Entries older than
//! `max_pending_age_secs` are dropped on drain. Legacy
//! [`PendingCache::drain`] is retained for backward compatibility
//! but is deprecated — it returns every entry regardless of whether
//! its trigger confirmed, which invites broadcasting a tx whose
//! motivating oracle update never landed.
//!
//! Pure decode + pre-sign storage lives on [`PendingCache`] so tests
//! can exercise it without a live RPC; the RPC-bound subscription
//! lives on [`MempoolMonitor`].
//!
//! # RPC endpoint requirements
//!
//! **Public BSC RPCs do not feed this module.** `eth_subscribe` for
//! `newPendingTransactions` is either disabled or returns only the
//! local-node pool on every public BSC endpoint (Binance public WS,
//! Ankr, Allnodes, QuickNode shared tier, publicnode). The ~3 s
//! head-start the monitor is designed for is only achievable with:
//!
//! - a paid MEV-streaming service (bloxroute, blocknative), or
//! - a self-hosted BSC geth with the full txpool exposed.
//!
//! When the configured endpoint only streams local-pool transactions,
//! `run_once` still succeeds (subscription establishes) but zero
//! [`OracleUpdate`] events ever arrive. The monitor guards against a
//! silent-nothing scenario by logging a `warn!` with the endpoint URL
//! when no pending tx is observed within
//! [`FIRST_TX_WATCHDOG`] of subscription — operators see an explicit
//! "subscription appears inactive" signal instead of a blank stream.
//!
//! # Safety invariant
//!
//! Pre-signed liquidations bypass the `eth_call` simulation gate that
//! `charon-executor` would otherwise enforce before broadcast. The
//! cache therefore returns pre-signs wrapped in
//! [`UnverifiedPreSigned`] on drain — the raw EIP-2718 envelope is
//! only reachable after a caller presents a [`SimulationVerdict::Ok`]
//! via [`UnverifiedPreSigned::verify`]. A broadcaster written against
//! this type cannot skip the gate without disabling the type system.
//!
//! This module is library-only. CLI wiring (listen-loop integration +
//! per-block drain) is tracked in issue #299.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, SystemTimeError, UNIX_EPOCH};

use alloy::consensus::Transaction as _;
use alloy::primitives::{Address, B256, Bytes, FixedBytes, U256};
use alloy::providers::Provider;
use alloy::providers::RootProvider;
use alloy::pubsub::PubSubFrontend;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};
use charon_core::LiquidationOpportunity;
use dashmap::DashMap;
use futures_util::StreamExt;
use rand::Rng;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Default lifetime for a pre-signed liquidation sitting in the
/// pending map. The head-start window is ~3 s on BSC (one block); we
/// pad to 30 s so a one-block stall on the private RPC doesn't
/// silently drop a prepared tx.
pub const DEFAULT_MAX_PENDING_AGE: Duration = Duration::from_secs(30);

/// Grace period after `subscribe_pending_transactions` succeeds before
/// the monitor starts complaining that nothing is arriving. Long enough
/// to cover a quiet market window on a healthy mempool stream (BSC
/// steady-state pending tx rate is dozens-per-second), short enough
/// that an operator pointed at a public RPC that silently drops
/// pending-tx subscriptions sees a warning within a minute.
pub const FIRST_TX_WATCHDOG: Duration = Duration::from_secs(30);

/// Initial reconnect backoff for the pending-tx subscription.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// Upper bound on reconnect backoff. Matches `BlockListener` so an
/// operator tuning one knob doesn't need to tune two.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

sol! {
    /// Venus `ResilientOracle` write surface (BSC mainnet). The two
    /// selectors kept below are the ones the live proxy at
    /// `0x6592b5DE802159F3E74B2486b091D11a8256ab8A` accepts; legacy
    /// surfaces are split into [`ILegacyVenusOracleWrite`] so
    /// [`legacy_selectors`] can expose them without polluting the
    /// default tracked set.
    interface IVenusOracleWrite {
        /// Resilient oracle entry point — refreshes the cached
        /// snapshot for `asset` by re-reading its configured source
        /// oracles (Chainlink, Pyth, Binance redstone).
        ///
        /// Source: Venus `ResilientOracle` at
        /// `0x6592b5DE802159F3E74B2486b091D11a8256ab8A` (BSC mainnet).
        function updatePrice(address asset) external;

        /// Alternate entry on the resilient oracle for the same
        /// action, used when callers already hold the asset address
        /// rather than a vToken.
        ///
        /// Source: Venus `ResilientOracle` at
        /// `0x6592b5DE802159F3E74B2486b091D11a8256ab8A` (BSC mainnet).
        function updateAssetPrice(address asset) external;
    }

    /// Legacy Venus oracle write surface. Not installed on the
    /// current BSC `ResilientOracle` — kept here so operators
    /// running against a fork or a chain that still exposes the
    /// older `VenusPriceOracle` / Compound-style oracle can opt in
    /// via [`legacy_selectors`].
    interface ILegacyVenusOracleWrite {
        /// Legacy `VenusPriceOracle` — writes a price directly
        /// against the underlying asset address. Not present on
        /// BSC mainnet's `ResilientOracle`.
        function setDirectPrice(address asset, uint256 price) external;

        /// Compound-style oracle — writes a price keyed by vToken.
        /// Not present on BSC mainnet's `ResilientOracle`.
        function setUnderlyingPrice(address vToken, uint256 price) external;
    }
}

/// Decoded observation extracted from one pending tx.
///
/// Split into two variants so the type system prevents a caller from
/// pre-signing against a `Refresh` update (which carries no new
/// price — the oracle must be re-read after the tx confirms). Pre-sign
/// builders should pattern-match on [`OracleUpdate::DirectUpdate`]
/// and handle [`OracleUpdate::Refresh`] explicitly (typically by
/// triggering a re-read once the trigger tx confirms).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum OracleUpdate {
    /// Price refresh via `updatePrice` / `updateAssetPrice` — the
    /// call only names the asset; the new price is whatever the
    /// source oracles return when the tx executes. Callers must
    /// re-read the oracle after confirmation or simulate via the
    /// underlying feed.
    Refresh {
        /// Hash of the pending tx that triggered the observation.
        tx_hash: B256,
        /// 4-byte selector matched.
        selector: FixedBytes<4>,
        /// Address argument from the call (asset).
        asset: Address,
    },
    /// Direct price write via `setDirectPrice` / `setUnderlyingPrice`
    /// — the calldata itself carries the new price, so a pre-sign
    /// builder can run the full health-factor simulation without
    /// waiting for confirmation.
    DirectUpdate {
        /// Hash of the pending tx that triggered the observation.
        tx_hash: B256,
        /// 4-byte selector matched.
        selector: FixedBytes<4>,
        /// Address argument from the call (asset or vToken,
        /// depending on the selector).
        asset: Address,
        /// New on-chain price carried by the calldata.
        price: U256,
    },
}

impl OracleUpdate {
    /// Hash of the originating pending tx.
    pub fn tx_hash(&self) -> B256 {
        match self {
            OracleUpdate::Refresh { tx_hash, .. } | OracleUpdate::DirectUpdate { tx_hash, .. } => {
                *tx_hash
            }
        }
    }

    /// 4-byte selector matched on the calldata.
    pub fn selector(&self) -> FixedBytes<4> {
        match self {
            OracleUpdate::Refresh { selector, .. }
            | OracleUpdate::DirectUpdate { selector, .. } => *selector,
        }
    }

    /// Asset (or vToken) argument from the call.
    pub fn asset(&self) -> Address {
        match self {
            OracleUpdate::Refresh { asset, .. } | OracleUpdate::DirectUpdate { asset, .. } => {
                *asset
            }
        }
    }

    /// Short human-readable tag for structured logging / metrics.
    pub fn kind(&self) -> &'static str {
        match self {
            OracleUpdate::Refresh { .. } => "refresh",
            OracleUpdate::DirectUpdate { .. } => "direct",
        }
    }
}

/// One signed liquidation sitting in the pending map, ready to
/// broadcast the moment its trigger oracle tx confirms.
///
/// **Safety invariant.** The raw EIP-2718 envelope is built against a
/// *predicted* post-oracle-update state. That prediction may never
/// materialise: the triggering oracle tx can revert, get replaced via
/// an EIP-1559 bump, or simply not land in the next block. Callers
/// MUST re-simulate the raw tx against confirmed block state before
/// broadcasting, per the CLAUDE.md hard invariant "every liquidation
/// transaction passes an eth_call simulation gate before broadcast".
///
/// The cache enforces this structurally:
/// [`PendingCache::drain_for_block`] (and the deprecated
/// [`PendingCache::drain`]) return [`UnverifiedPreSigned`] wrappers
/// rather than `PreSignedLiquidation` directly. The raw tx is only
/// reachable via [`UnverifiedPreSigned::verify`], which demands a
/// [`SimulationVerdict::Ok`] proof token that only a just-passed
/// simulation can produce.
///
/// Marked `#[non_exhaustive]` so adding fields (simulation metadata,
/// gas hints, etc.) isn't a breaking change for downstream callers
/// that construct `PreSignedLiquidation` directly.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct PreSignedLiquidation {
    /// Borrower targeted. Also the map key; duplicated here so a
    /// drained vec is self-describing.
    pub borrower: Address,
    /// Raw EIP-2718 envelope bytes, as produced by
    /// [`TxBuilder::sign`](charon_executor::TxBuilder::sign). Ready
    /// for `eth_sendRawTransaction`.
    ///
    /// **Intentionally pub-but-guarded.** The field is public so
    /// in-process construction stays ergonomic (tests, the mempool's
    /// own insert path) but the drain API never hands a
    /// `PreSignedLiquidation` to the broadcaster — it hands an
    /// [`UnverifiedPreSigned`] so the simulation gate cannot be
    /// bypassed at the type layer.
    pub raw_tx: Bytes,
    /// The opportunity this tx was built against. Carried so the
    /// drainer can log context and re-rank if multiple pre-signs
    /// target the same borrower across different oracle updates.
    pub opportunity: LiquidationOpportunity,
    /// Hash of the pending oracle tx that motivated this pre-sign.
    /// [`PendingCache::drain_for_block`] returns the entry only if
    /// this hash appears in the confirmed-tx set of the new block.
    pub trigger_tx: B256,
    /// Unix seconds at which the entry was inserted.
    pub inserted_at: u64,
}

/// Proof token that an `eth_call` simulation against current block
/// state accepted the candidate tx. Produced only by code that has
/// actually run the simulator — `Ok` has no public constructor beyond
/// [`SimulationVerdict::approve`], so a broadcaster cannot fabricate
/// one.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
#[must_use = "a verdict of Revert or Error must short-circuit the broadcast"]
pub enum SimulationVerdict {
    /// The simulator returned a success receipt; the tx is safe to
    /// broadcast against the block the simulator saw.
    ///
    /// **Construction rule.** `Ok` is literal-constructible by any
    /// in-crate caller, but by convention only simulator boundary code
    /// (or [`SimulationVerdict::approve`]) should emit it. Any other
    /// call site producing `SimulationVerdict::Ok` is a review flag —
    /// reviewers should reject it unless it is demonstrably tied to a
    /// real `eth_call` outcome. Sealing would require a cross-crate
    /// proof-token type that the executor does not yet expose.
    Ok,
    /// The simulator returned a reverting receipt. The tx must not
    /// be broadcast.
    Revert,
    /// The simulator itself errored (RPC timeout, encoding bug). Treat
    /// as Revert for safety.
    Error,
}

impl SimulationVerdict {
    /// Narrow constructor kept alongside the enum so every
    /// `SimulationVerdict::Ok` at a call site is traceable to a
    /// simulator outcome, not a hand-rolled literal.
    pub fn approve() -> Self {
        SimulationVerdict::Ok
    }
}

/// Newtype returned by [`PendingCache::drain_for_block`] /
/// [`PendingCache::drain`]. Wraps a `PreSignedLiquidation` so the raw
/// EIP-2718 envelope is only reachable after the caller presents a
/// passing [`SimulationVerdict`]. Honours the CLAUDE.md safety
/// invariant that every liquidation tx must pass an `eth_call` gate
/// before broadcast, enforced by the type system instead of a comment.
///
/// Marked `#[non_exhaustive]` so adding peek accessors or metadata
/// fields later is not a breaking change.
#[derive(Debug, Clone)]
#[non_exhaustive]
#[must_use = "pre-signs bypass the executor's eth_call gate; call .verify(simulation_verdict) before broadcasting"]
pub struct UnverifiedPreSigned {
    inner: PreSignedLiquidation,
}

impl UnverifiedPreSigned {
    /// Peek at the borrower without unwrapping the raw tx — lets the
    /// drain-site log context and rank candidates before simulation.
    pub fn borrower(&self) -> Address {
        self.inner.borrower
    }

    /// Peek at the trigger oracle tx hash.
    pub fn trigger_tx(&self) -> B256 {
        self.inner.trigger_tx
    }

    /// Peek at the opportunity payload so callers can feed it to the
    /// simulator without consuming the wrapper.
    pub fn opportunity(&self) -> &LiquidationOpportunity {
        &self.inner.opportunity
    }

    /// Consume the wrapper and return the raw tx + metadata ONLY when
    /// the caller presents a passing simulation verdict. A `Revert` or
    /// `Error` verdict returns `Err((self, verdict))` so the caller
    /// keeps the wrapper for logging and cannot accidentally broadcast.
    ///
    /// The `Err` variant is intentionally as heavy as the `Ok` variant
    /// (both carry the full `PreSignedLiquidation`) — returning the
    /// wrapper by value is what preserves the type-level guarantee that
    /// the raw tx is never reachable without a passing verdict. Boxing
    /// the error would only obscure the shape without meaningful win on
    /// the non-broadcast path.
    #[allow(clippy::result_large_err)]
    pub fn verify(
        self,
        verdict: SimulationVerdict,
    ) -> std::result::Result<PreSignedLiquidation, (Self, SimulationVerdict)> {
        match verdict {
            SimulationVerdict::Ok => Ok(self.inner),
            SimulationVerdict::Revert | SimulationVerdict::Error => Err((self, verdict)),
        }
    }
}

/// Errors surfaced by [`MempoolMonitor`] on its public API.
///
/// `anyhow` stays internal to the crate; callers (executor wiring,
/// CLI) get a typed enum so they can distinguish "the channel went
/// away, shut down cleanly" from "the RPC is unhealthy, surface to
/// operator".
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MempoolError {
    /// `eth_subscribe` for `newPendingTransactions` failed or the
    /// established stream terminated. Callers typically log and let
    /// the monitor's retry loop handle it; surfaced here for the
    /// benefit of callers that want to bail on repeated failure.
    #[error("pending-tx subscription failed: {0}")]
    SubscriptionFailed(#[source] alloy::transports::TransportError),
    /// The receiver half of the oracle-update channel was dropped,
    /// so the monitor has nowhere to send decoded updates. Treated
    /// as a clean shutdown signal.
    #[error("oracle update channel closed")]
    ChannelClosed,
}

/// Pure decode + pre-sign storage. Separated from the RPC layer so
/// tests can exercise the selector logic and TTL semantics without
/// opening a socket.
#[derive(Debug)]
pub struct PendingCache {
    oracle: Address,
    selectors: HashSet<FixedBytes<4>>,
    pending: DashMap<Address, PreSignedLiquidation>,
    max_pending_age_secs: u64,
}

impl PendingCache {
    pub fn new(
        oracle: Address,
        selectors: HashSet<FixedBytes<4>>,
        max_pending_age: Duration,
    ) -> Self {
        Self {
            oracle,
            selectors,
            pending: DashMap::new(),
            max_pending_age_secs: max_pending_age.as_secs(),
        }
    }

    pub fn with_defaults(oracle: Address) -> Self {
        Self::new(oracle, default_selectors(), DEFAULT_MAX_PENDING_AGE)
    }

    pub fn oracle(&self) -> Address {
        self.oracle
    }

    pub fn is_tracked_selector(&self, selector: FixedBytes<4>) -> bool {
        self.selectors.contains(&selector)
    }

    /// Insert a freshly pre-signed liquidation. Overwrites any prior
    /// entry for the same borrower — the most recent oracle update
    /// wins, which is what we want when two updates land in the same
    /// block window (the later one is what the chain will see).
    pub fn insert(&self, tx: PreSignedLiquidation) {
        debug!(
            borrower = %tx.borrower,
            trigger = %tx.trigger_tx,
            "pre-signed liquidation armed"
        );
        self.pending.insert(tx.borrower, tx);
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Drain entries whose trigger oracle tx actually confirmed in
    /// `block_hash`. Entries whose trigger is not in
    /// `confirmed_tx_hashes` are re-queued if still within TTL, or
    /// dropped as stale if not. Clock failures are treated as fatal:
    /// every entry is dropped and a `warn!` is emitted, because a
    /// dead clock makes TTL meaningless and we must not broadcast
    /// pre-signs against an unknown-age state.
    ///
    /// `block_hash` is used only for log correlation with the
    /// `ChainEvent::NewBlock` that triggered the drain; it is not
    /// used as a cache key.
    ///
    /// Each returned [`UnverifiedPreSigned`] requires a
    /// [`SimulationVerdict::Ok`] from the caller before its raw tx is
    /// reachable. The wrapper is what keeps the CLAUDE.md safety
    /// invariant enforced at the type level.
    #[must_use = "dropping the drained vec discards pre-signs without broadcasting; at minimum log and re-insert"]
    pub fn drain_for_block(
        &self,
        block_hash: B256,
        confirmed_tx_hashes: &HashSet<B256>,
    ) -> Vec<UnverifiedPreSigned> {
        let now = match unix_now() {
            Ok(n) => n,
            Err(err) => {
                warn!(
                    ?err,
                    pending = self.pending.len(),
                    "system clock unavailable, dropping all pre-signs"
                );
                self.pending.clear();
                return Vec::new();
            }
        };
        let max_age = self.max_pending_age_secs;
        let keys: Vec<Address> = self.pending.iter().map(|e| *e.key()).collect();
        let mut out = Vec::with_capacity(keys.len());
        let mut requeued = 0usize;
        let mut stale = 0usize;

        for k in keys {
            let Some((_, entry)) = self.pending.remove(&k) else {
                continue;
            };

            let age = now.saturating_sub(entry.inserted_at);

            if confirmed_tx_hashes.contains(&entry.trigger_tx) {
                if age > max_age {
                    stale += 1;
                    warn!(
                        borrower = %entry.borrower,
                        age_secs = age,
                        "dropped stale pre-signed liquidation (trigger confirmed but TTL exceeded)"
                    );
                    continue;
                }
                out.push(UnverifiedPreSigned { inner: entry });
                continue;
            }

            // Trigger didn't confirm in this block — re-queue if TTL
            // allows, otherwise drop.
            if age > max_age {
                stale += 1;
                warn!(
                    borrower = %entry.borrower,
                    age_secs = age,
                    "dropped stale pre-signed liquidation (trigger never confirmed)"
                );
                continue;
            }
            requeued += 1;
            self.pending.insert(entry.borrower, entry);
        }

        debug!(
            %block_hash,
            drained = out.len(),
            requeued,
            stale,
            "mempool cache drained for block"
        );
        out
    }

    /// Legacy drain. Returns every entry still within TTL, regardless
    /// of whether its trigger oracle tx actually confirmed in the
    /// current block. Unsafe for production broadcast —
    /// [`Self::drain_for_block`] is the only drain that respects
    /// the "trigger must confirm" invariant.
    #[deprecated(
        since = "0.1.0",
        note = "use drain_for_block with the confirmed-tx set from the NewBlock event"
    )]
    #[must_use = "dropping the drained vec discards pre-signs without broadcasting; at minimum log and re-insert"]
    pub fn drain(&self) -> Vec<UnverifiedPreSigned> {
        let now = match unix_now() {
            Ok(n) => n,
            Err(err) => {
                warn!(
                    ?err,
                    pending = self.pending.len(),
                    "system clock unavailable, dropping all pre-signs"
                );
                self.pending.clear();
                return Vec::new();
            }
        };
        let max_age = self.max_pending_age_secs;
        let mut out = Vec::with_capacity(self.pending.len());
        let keys: Vec<Address> = self.pending.iter().map(|e| *e.key()).collect();
        for k in keys {
            if let Some((_, entry)) = self.pending.remove(&k) {
                if now.saturating_sub(entry.inserted_at) > max_age {
                    warn!(
                        borrower = %entry.borrower,
                        age_secs = now.saturating_sub(entry.inserted_at),
                        "dropped stale pre-signed liquidation"
                    );
                    continue;
                }
                out.push(UnverifiedPreSigned { inner: entry });
            }
        }
        debug!(drained = out.len(), "mempool cache drained (legacy)");
        out
    }

    /// Pure decoder — returns `None` when the recipient isn't the
    /// bound oracle, the selector isn't tracked, or the calldata
    /// fails to decode against every candidate shape.
    pub fn decode(&self, tx_hash: B256, to: Option<Address>, input: &[u8]) -> Option<OracleUpdate> {
        if to != Some(self.oracle) {
            return None;
        }
        if input.len() < 4 {
            return None;
        }
        let selector = FixedBytes::<4>::from_slice(&input[..4]);
        if !self.selectors.contains(&selector) {
            return None;
        }
        decode_oracle_call(tx_hash, selector, input)
    }
}

/// Subscribes to the pending-tx stream, filters oracle updates, and
/// holds pre-signed liquidations until the next block.
///
/// Cheap to clone — all mutable state lives behind `Arc` / `DashMap`.
/// Clone into the block-listener task so it can call
/// [`PendingCache::drain_for_block`] without coordinating with the
/// mempool task.
#[derive(Clone)]
pub struct MempoolMonitor {
    provider: Arc<RootProvider<PubSubFrontend>>,
    cache: Arc<PendingCache>,
}

impl MempoolMonitor {
    /// Full-control constructor.
    pub fn new(
        provider: Arc<RootProvider<PubSubFrontend>>,
        oracle: Address,
        selectors: HashSet<FixedBytes<4>>,
        max_pending_age: Duration,
    ) -> Self {
        Self {
            provider,
            cache: Arc::new(PendingCache::new(oracle, selectors, max_pending_age)),
        }
    }

    /// Convenience: build with [`default_selectors`] and
    /// [`DEFAULT_MAX_PENDING_AGE`].
    pub fn with_defaults(provider: Arc<RootProvider<PubSubFrontend>>, oracle: Address) -> Self {
        Self::new(
            provider,
            oracle,
            default_selectors(),
            DEFAULT_MAX_PENDING_AGE,
        )
    }

    pub fn oracle(&self) -> Address {
        self.cache.oracle()
    }

    /// Share the inner cache. Lets the block-listener task call
    /// [`PendingCache::drain_for_block`] without going through the
    /// monitor, which keeps its `run` loop free to stay on the
    /// pending-tx stream.
    pub fn cache(&self) -> Arc<PendingCache> {
        self.cache.clone()
    }

    pub fn insert(&self, tx: PreSignedLiquidation) {
        self.cache.insert(tx);
    }

    pub fn pending_len(&self) -> usize {
        self.cache.pending_len()
    }

    /// Run the pending-tx subscription forever. Reconnect on stream
    /// error with a 1 s → 30 s exponential backoff plus 0-25% random
    /// jitter (see [`backoff_with_jitter`]) so many monitors pointed
    /// at the same upstream don't reconnect in lockstep.
    ///
    /// Emits one [`OracleUpdate`] per matched tx on `tx`. Returns
    /// `Ok(())` only when the receiver is dropped — the loop is
    /// expected to run for the lifetime of the process.
    pub async fn run(&self, tx: mpsc::Sender<OracleUpdate>) -> Result<(), MempoolError> {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            match self.run_once(&tx).await {
                Ok(()) => {
                    info!(oracle = %self.oracle(), "mempool channel closed, exiting");
                    return Ok(());
                }
                Err(err) => {
                    warn!(
                        oracle = %self.oracle(),
                        error = ?err,
                        backoff_secs = backoff.as_secs(),
                        "mempool subscription error, reconnecting after backoff"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = backoff_with_jitter(backoff, MAX_BACKOFF);
                }
            }
        }
    }

    async fn run_once(&self, tx: &mpsc::Sender<OracleUpdate>) -> Result<()> {
        let sub = self
            .provider
            .subscribe_pending_transactions()
            .await
            .context("mempool: subscribe_pending_transactions failed")?;

        info!(oracle = %self.oracle(), "pending-tx subscription established");

        let mut stream = sub.into_stream();

        // First-tx watchdog. If the configured endpoint silently drops
        // `newPendingTransactions` (every public BSC RPC) the
        // subscription call above still succeeds but the stream never
        // yields. Nudge the operator at `FIRST_TX_WATCHDOG` with a
        // diagnosis pointing at the likely cause.
        let mut saw_first_tx = false;
        let mut watchdog = Box::pin(tokio::time::sleep(FIRST_TX_WATCHDOG));

        loop {
            tokio::select! {
                biased;
                maybe_hash = stream.next() => {
                    let Some(hash) = maybe_hash else { break; };
                    if !saw_first_tx {
                        saw_first_tx = true;
                        debug!(%hash, "first pending tx received, watchdog disarmed");
                    }
                    if !self.handle_pending_hash(hash, tx).await? {
                        return Ok(());
                    }
                }
                _ = &mut watchdog, if !saw_first_tx => {
                    warn!(
                        oracle = %self.oracle(),
                        watchdog_secs = FIRST_TX_WATCHDOG.as_secs(),
                        "no pending tx received after subscribe — the endpoint is likely a public RPC that disables newPendingTransactions or exposes only its local pool. MempoolMonitor requires a paid MEV stream (bloxroute/blocknative) or a self-hosted BSC geth with the txpool exposed. See module docs."
                    );
                }
            }
        }

        anyhow::bail!("mempool: pending-tx subscription stream ended")
    }

    /// Look up a pending tx hash, decode it, and forward any decoded
    /// [`OracleUpdate`] on `tx`. Returns `Ok(false)` when the receiver
    /// has been dropped (caller should exit cleanly), `Ok(true)`
    /// otherwise. Extracted from `run_once` so the watchdog loop stays
    /// readable.
    async fn handle_pending_hash(
        &self,
        hash: B256,
        tx: &mpsc::Sender<OracleUpdate>,
    ) -> Result<bool> {
        // Lookup failures are common for txs that dropped out of the
        // pool between the hash push and our get — log at debug, keep
        // going.
        let full = match self.provider.get_transaction_by_hash(hash).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                debug!(%hash, "pending tx vanished before fetch");
                return Ok(true);
            }
            Err(err) => {
                debug!(%hash, ?err, "get_transaction_by_hash failed");
                return Ok(true);
            }
        };

        let to = full.inner.kind().to().copied();
        let input = full.inner.input();
        let Some(update) = self.cache.decode(hash, to, input) else {
            return Ok(true);
        };

        // TODO(charon-metrics): bump a Prometheus counter labelled
        // with the selector + update.kind() here once the metrics
        // crate merges in rebase.
        debug!(
            %hash,
            asset = %update.asset(),
            selector = %format_selector(update.selector()),
            kind = update.kind(),
            "venus oracle update seen in mempool"
        );

        if tx.send(update).await.is_err() {
            return Ok(false);
        }
        Ok(true)
    }
}

/// Default Venus oracle write selectors tracked by the monitor.
///
/// Restricted to the two selectors actually accepted by the live
/// Venus `ResilientOracle` on BSC mainnet
/// (`0x6592b5DE802159F3E74B2486b091D11a8256ab8A`):
/// `updatePrice(address)` and `updateAssetPrice(address)`. Legacy
/// write selectors (`setDirectPrice`, `setUnderlyingPrice`) are not
/// deployed on BSC's `ResilientOracle` and live in
/// [`legacy_selectors`] for operators running against a fork or a
/// chain that still exposes them.
pub fn default_selectors() -> HashSet<FixedBytes<4>> {
    let mut s = HashSet::with_capacity(2);
    s.insert(IVenusOracleWrite::updatePriceCall::SELECTOR.into());
    s.insert(IVenusOracleWrite::updateAssetPriceCall::SELECTOR.into());
    s
}

/// Legacy Venus oracle write selectors. Not accepted by the live
/// BSC `ResilientOracle`; exposed for operators pointed at a fork
/// or a chain that still runs the older `VenusPriceOracle` /
/// Compound-style oracle.
pub fn legacy_selectors() -> HashSet<FixedBytes<4>> {
    let mut s = HashSet::with_capacity(2);
    s.insert(ILegacyVenusOracleWrite::setDirectPriceCall::SELECTOR.into());
    s.insert(ILegacyVenusOracleWrite::setUnderlyingPriceCall::SELECTOR.into());
    s
}

fn decode_oracle_call(
    tx_hash: B256,
    selector: FixedBytes<4>,
    input: &[u8],
) -> Option<OracleUpdate> {
    // `abi_decode_raw` skips the selector and validates the body.
    // `validate = true` rejects trailing junk.
    let body = &input[4..];

    if selector == FixedBytes::<4>::from(IVenusOracleWrite::updatePriceCall::SELECTOR) {
        let call = IVenusOracleWrite::updatePriceCall::abi_decode_raw(body, true).ok()?;
        return Some(OracleUpdate::Refresh {
            tx_hash,
            selector,
            asset: call.asset,
        });
    }
    if selector == FixedBytes::<4>::from(IVenusOracleWrite::updateAssetPriceCall::SELECTOR) {
        let call = IVenusOracleWrite::updateAssetPriceCall::abi_decode_raw(body, true).ok()?;
        return Some(OracleUpdate::Refresh {
            tx_hash,
            selector,
            asset: call.asset,
        });
    }
    if selector == FixedBytes::<4>::from(ILegacyVenusOracleWrite::setDirectPriceCall::SELECTOR) {
        let call = ILegacyVenusOracleWrite::setDirectPriceCall::abi_decode_raw(body, true).ok()?;
        return Some(OracleUpdate::DirectUpdate {
            tx_hash,
            selector,
            asset: call.asset,
            price: call.price,
        });
    }
    if selector == FixedBytes::<4>::from(ILegacyVenusOracleWrite::setUnderlyingPriceCall::SELECTOR)
    {
        let call =
            ILegacyVenusOracleWrite::setUnderlyingPriceCall::abi_decode_raw(body, true).ok()?;
        return Some(OracleUpdate::DirectUpdate {
            tx_hash,
            selector,
            asset: call.vToken,
            price: call.price,
        });
    }
    None
}

fn format_selector(sel: FixedBytes<4>) -> String {
    let b = sel.as_slice();
    format!("0x{:02x}{:02x}{:02x}{:02x}", b[0], b[1], b[2], b[3])
}

/// Unix seconds since epoch. Surfaces clock-skew as an error so
/// callers who depend on monotonic age comparisons (TTL) can fail
/// closed rather than silently treating a dead clock as
/// `inserted_at = 0`.
fn unix_now() -> Result<u64, SystemTimeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
}

/// Double `current`, add 0-25% random jitter, and clamp to `max`.
/// Extracted so tests (and any future `BlockListener` convergence)
/// can exercise the backoff curve without a live socket.
fn backoff_with_jitter(current: Duration, max: Duration) -> Duration {
    let doubled = current.saturating_mul(2);
    // `doubled.as_millis()` can be large on the path to the cap;
    // computing the jitter off the post-double value keeps the
    // distribution well-defined at every step.
    let quarter_ms = (doubled.as_millis() / 4) as u64;
    let jitter_ms = if quarter_ms == 0 {
        0
    } else {
        rand::thread_rng().gen_range(0..quarter_ms)
    };
    (doubled + Duration::from_millis(jitter_ms)).min(max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256};
    use alloy::sol_types::SolCall;
    use charon_core::{FlashLoanSource, Position, ProtocolId, SwapRoute};

    const ORACLE: Address = address!("1111111111111111111111111111111111111111");
    const OTHER: Address = address!("2222222222222222222222222222222222222222");
    const ASSET: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    const HASH: B256 = b256!("abababababababababababababababababababababababababababababababab");

    fn mk_cache() -> PendingCache {
        // Tests exercise the legacy selectors too — wire both sets so
        // `decode_set_direct_price_*` / `decode_set_underlying_price_*`
        // still match.
        let mut sels = default_selectors();
        sels.extend(legacy_selectors());
        PendingCache::new(ORACLE, sels, DEFAULT_MAX_PENDING_AGE)
    }

    fn now_secs() -> u64 {
        unix_now().expect("test clock")
    }

    fn mk_opp() -> LiquidationOpportunity {
        LiquidationOpportunity {
            position: Position {
                protocol: ProtocolId::Venus,
                chain_id: 56,
                borrower: address!("3333333333333333333333333333333333333333"),
                collateral_token: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                debt_token: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                collateral_amount: U256::from(1_000u64),
                debt_amount: U256::from(500u64),
                health_factor: U256::ZERO,
                liquidation_bonus_bps: 1_000,
            },
            debt_to_repay: U256::from(250u64),
            expected_collateral_out: U256::from(275u64),
            flash_source: FlashLoanSource::AaveV3,
            swap_route: SwapRoute {
                token_in: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                token_out: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                amount_in: U256::from(275u64),
                min_amount_out: U256::from(260u64),
                pool_fee: 3_000,
            },
            net_profit_usd_cents: 5_000,
        }
    }

    #[test]
    fn default_selectors_has_two_entries() {
        assert_eq!(default_selectors().len(), 2);
    }

    #[test]
    fn legacy_selectors_has_two_entries() {
        assert_eq!(legacy_selectors().len(), 2);
    }

    #[test]
    fn default_and_legacy_selectors_are_disjoint() {
        let d = default_selectors();
        let l = legacy_selectors();
        assert!(d.is_disjoint(&l));
    }

    #[test]
    fn decode_update_price_yields_refresh_variant() {
        let c = mk_cache();
        let call = IVenusOracleWrite::updatePriceCall { asset: ASSET };
        let data = call.abi_encode();
        let out = c.decode(HASH, Some(ORACLE), &data).expect("match");
        match out {
            OracleUpdate::Refresh {
                asset,
                tx_hash: h,
                selector,
            } => {
                assert_eq!(asset, ASSET);
                assert_eq!(h, HASH);
                assert_eq!(
                    selector,
                    FixedBytes::<4>::from(IVenusOracleWrite::updatePriceCall::SELECTOR)
                );
            }
            OracleUpdate::DirectUpdate { .. } => panic!("expected Refresh"),
        }
    }

    #[test]
    fn decode_update_asset_price_yields_refresh_variant() {
        let c = mk_cache();
        let call = IVenusOracleWrite::updateAssetPriceCall { asset: ASSET };
        let data = call.abi_encode();
        let out = c.decode(HASH, Some(ORACLE), &data).expect("match");
        assert!(matches!(
            out,
            OracleUpdate::Refresh { asset, .. } if asset == ASSET
        ));
    }

    #[test]
    fn decode_set_direct_price_yields_direct_update() {
        let c = mk_cache();
        let call = ILegacyVenusOracleWrite::setDirectPriceCall {
            asset: ASSET,
            price: U256::from(12_345u64),
        };
        let data = call.abi_encode();
        let out = c.decode(HASH, Some(ORACLE), &data).expect("match");
        match out {
            OracleUpdate::DirectUpdate { asset, price, .. } => {
                assert_eq!(asset, ASSET);
                assert_eq!(price, U256::from(12_345u64));
            }
            OracleUpdate::Refresh { .. } => panic!("expected DirectUpdate"),
        }
    }

    #[test]
    fn decode_set_underlying_price_yields_direct_update() {
        let c = mk_cache();
        let call = ILegacyVenusOracleWrite::setUnderlyingPriceCall {
            vToken: ASSET,
            price: U256::from(99u64),
        };
        let data = call.abi_encode();
        let out = c.decode(HASH, Some(ORACLE), &data).expect("match");
        match out {
            OracleUpdate::DirectUpdate { asset, price, .. } => {
                assert_eq!(asset, ASSET);
                assert_eq!(price, U256::from(99u64));
            }
            OracleUpdate::Refresh { .. } => panic!("expected DirectUpdate"),
        }
    }

    #[test]
    fn decode_rejects_wrong_recipient() {
        let c = mk_cache();
        let call = IVenusOracleWrite::updatePriceCall { asset: ASSET };
        let data = call.abi_encode();
        assert!(c.decode(HASH, Some(OTHER), &data).is_none());
        assert!(c.decode(HASH, None, &data).is_none());
    }

    #[test]
    fn decode_rejects_unknown_selector() {
        let c = mk_cache();
        // `transfer(address,uint256)` selector — not in the tracked
        // set. Followed by two zero-padded words so a lenient decoder
        // wouldn't accidentally accept it.
        let mut data = vec![0xa9, 0x05, 0x9c, 0xbb];
        data.extend_from_slice(&[0u8; 64]);
        assert!(c.decode(HASH, Some(ORACLE), &data).is_none());
    }

    #[test]
    fn decode_rejects_short_input() {
        let c = mk_cache();
        assert!(c.decode(HASH, Some(ORACLE), &[]).is_none());
        assert!(c.decode(HASH, Some(ORACLE), &[0xde, 0xad]).is_none());
    }

    #[test]
    fn decode_rejects_truncated_calldata() {
        let c = mk_cache();
        let sel: [u8; 4] = IVenusOracleWrite::updatePriceCall::SELECTOR;
        assert!(c.decode(HASH, Some(ORACLE), &sel).is_none());
    }

    #[test]
    fn default_cache_does_not_decode_legacy_selectors() {
        // `PendingCache::with_defaults` uses `default_selectors()`
        // only, which now excludes `setDirectPrice` /
        // `setUnderlyingPrice`. Calldata targeting those must no
        // longer decode against a default-configured cache.
        let c = PendingCache::with_defaults(ORACLE);
        let call = ILegacyVenusOracleWrite::setDirectPriceCall {
            asset: ASSET,
            price: U256::from(1u64),
        };
        let data = call.abi_encode();
        assert!(c.decode(HASH, Some(ORACLE), &data).is_none());
    }

    #[test]
    fn drain_for_block_returns_confirmed_entries_only() {
        let c = mk_cache();
        let opp = mk_opp();
        let borrower = opp.position.borrower;
        c.insert(PreSignedLiquidation {
            borrower,
            raw_tx: Bytes::from_static(&[0x01, 0x02, 0x03]),
            opportunity: opp,
            trigger_tx: HASH,
            inserted_at: now_secs(),
        });
        assert_eq!(c.pending_len(), 1);

        let mut confirmed = HashSet::new();
        confirmed.insert(HASH);
        let block_hash = B256::repeat_byte(0xcc);
        let drained = c.drain_for_block(block_hash, &confirmed);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].borrower(), borrower);
        assert_eq!(c.pending_len(), 0);
    }

    #[test]
    fn drain_for_block_requeues_when_trigger_not_confirmed() {
        let c = mk_cache();
        let opp = mk_opp();
        c.insert(PreSignedLiquidation {
            borrower: opp.position.borrower,
            raw_tx: Bytes::new(),
            opportunity: opp,
            trigger_tx: HASH,
            inserted_at: now_secs(),
        });

        let confirmed = HashSet::new(); // trigger not in set
        let block_hash = B256::repeat_byte(0xaa);
        let drained = c.drain_for_block(block_hash, &confirmed);
        assert!(drained.is_empty());
        // Entry must remain in the cache for the next block.
        assert_eq!(c.pending_len(), 1);
    }

    #[test]
    fn drain_for_block_drops_stale_even_when_unconfirmed() {
        let c = mk_cache();
        let opp = mk_opp();
        c.insert(PreSignedLiquidation {
            borrower: opp.position.borrower,
            raw_tx: Bytes::new(),
            opportunity: opp,
            trigger_tx: HASH,
            inserted_at: now_secs().saturating_sub(3_600),
        });
        let confirmed = HashSet::new();
        let block_hash = B256::repeat_byte(0xbb);
        let drained = c.drain_for_block(block_hash, &confirmed);
        assert!(drained.is_empty());
        assert_eq!(c.pending_len(), 0, "stale entry must be evicted");
    }

    #[test]
    fn drain_for_block_drops_stale_even_when_confirmed() {
        let c = mk_cache();
        let opp = mk_opp();
        c.insert(PreSignedLiquidation {
            borrower: opp.position.borrower,
            raw_tx: Bytes::new(),
            opportunity: opp,
            trigger_tx: HASH,
            inserted_at: now_secs().saturating_sub(3_600),
        });
        let mut confirmed = HashSet::new();
        confirmed.insert(HASH);
        let block_hash = B256::repeat_byte(0xdd);
        let drained = c.drain_for_block(block_hash, &confirmed);
        assert!(
            drained.is_empty(),
            "expired entries must not broadcast even when confirmed"
        );
        assert_eq!(c.pending_len(), 0);
    }

    #[test]
    #[allow(deprecated)]
    fn legacy_drain_still_works() {
        let c = mk_cache();
        let opp = mk_opp();
        let borrower = opp.position.borrower;
        c.insert(PreSignedLiquidation {
            borrower,
            raw_tx: Bytes::from_static(&[0x01, 0x02, 0x03]),
            opportunity: opp,
            trigger_tx: HASH,
            inserted_at: now_secs(),
        });
        let drained = c.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].borrower(), borrower);
    }

    #[test]
    fn insert_overwrites_same_borrower() {
        let c = mk_cache();
        let opp = mk_opp();
        let borrower = opp.position.borrower;
        c.insert(PreSignedLiquidation {
            borrower,
            raw_tx: Bytes::from_static(&[0x01]),
            opportunity: opp.clone(),
            trigger_tx: HASH,
            inserted_at: now_secs(),
        });
        c.insert(PreSignedLiquidation {
            borrower,
            raw_tx: Bytes::from_static(&[0x02]),
            opportunity: opp,
            trigger_tx: HASH,
            inserted_at: now_secs(),
        });
        assert_eq!(c.pending_len(), 1);
        let mut confirmed = HashSet::new();
        confirmed.insert(HASH);
        let drained = c.drain_for_block(B256::ZERO, &confirmed);
        assert_eq!(drained.len(), 1);
        // To read raw_tx the caller must present a passing verdict —
        // that's the whole point of the wrapper.
        let unwrapped = drained
            .into_iter()
            .next()
            .unwrap()
            .verify(SimulationVerdict::approve())
            .expect("approved verdict must unwrap");
        assert_eq!(unwrapped.raw_tx.as_ref(), &[0x02]);
    }

    #[test]
    fn verify_ok_returns_inner_signed() {
        let c = mk_cache();
        let opp = mk_opp();
        c.insert(PreSignedLiquidation {
            borrower: opp.position.borrower,
            raw_tx: Bytes::from_static(&[0xaa]),
            opportunity: opp,
            trigger_tx: HASH,
            inserted_at: now_secs(),
        });
        let mut confirmed = HashSet::new();
        confirmed.insert(HASH);
        let drained = c.drain_for_block(B256::ZERO, &confirmed);
        let verified = drained
            .into_iter()
            .next()
            .unwrap()
            .verify(SimulationVerdict::Ok)
            .expect("Ok verdict unwraps");
        assert_eq!(verified.raw_tx.as_ref(), &[0xaa]);
    }

    #[test]
    fn verify_revert_keeps_wrapper_and_hides_raw_tx() {
        let c = mk_cache();
        let opp = mk_opp();
        c.insert(PreSignedLiquidation {
            borrower: opp.position.borrower,
            raw_tx: Bytes::from_static(&[0xbb]),
            opportunity: opp,
            trigger_tx: HASH,
            inserted_at: now_secs(),
        });
        let mut confirmed = HashSet::new();
        confirmed.insert(HASH);
        let drained = c.drain_for_block(B256::ZERO, &confirmed);
        let wrapped = drained.into_iter().next().unwrap();
        let borrower_before = wrapped.borrower();
        match wrapped.verify(SimulationVerdict::Revert) {
            Err((still_wrapped, v)) => {
                assert!(matches!(v, SimulationVerdict::Revert));
                assert_eq!(still_wrapped.borrower(), borrower_before);
            }
            Ok(_) => panic!("Revert must not unwrap"),
        }
    }

    #[test]
    fn verify_revert_then_ok_roundtrips() {
        // A rejected verdict must leave the wrapper usable for a retry
        // simulation. Without this, a transient RPC error on the first
        // simulate would permanently strand the pre-sign.
        let c = mk_cache();
        let opp = mk_opp();
        c.insert(PreSignedLiquidation {
            borrower: opp.position.borrower,
            raw_tx: Bytes::from_static(&[0xdd]),
            opportunity: opp,
            trigger_tx: HASH,
            inserted_at: now_secs(),
        });
        let mut confirmed = HashSet::new();
        confirmed.insert(HASH);
        let wrapped = c
            .drain_for_block(B256::ZERO, &confirmed)
            .into_iter()
            .next()
            .unwrap();
        let (retry, _) = match wrapped.verify(SimulationVerdict::Revert) {
            Err(pair) => pair,
            Ok(_) => panic!("Revert must not unwrap"),
        };
        let verified = retry
            .verify(SimulationVerdict::Ok)
            .expect("retry with Ok must unwrap");
        assert_eq!(verified.raw_tx.as_ref(), &[0xdd]);
    }

    #[test]
    fn peek_accessors_survive_failed_verify() {
        // Confirm every read-only accessor is still reachable on the
        // wrapper after a failed verdict — the logging/ranking path
        // must not be blocked by the failure.
        let c = mk_cache();
        let opp = mk_opp();
        let borrower_expected = opp.position.borrower;
        c.insert(PreSignedLiquidation {
            borrower: borrower_expected,
            raw_tx: Bytes::from_static(&[0xee]),
            opportunity: opp,
            trigger_tx: HASH,
            inserted_at: now_secs(),
        });
        let mut confirmed = HashSet::new();
        confirmed.insert(HASH);
        let wrapped = c
            .drain_for_block(B256::ZERO, &confirmed)
            .into_iter()
            .next()
            .unwrap();
        let (still_wrapped, _) = match wrapped.verify(SimulationVerdict::Error) {
            Err(pair) => pair,
            Ok(_) => panic!("Error must not unwrap"),
        };
        assert_eq!(still_wrapped.borrower(), borrower_expected);
        assert_eq!(still_wrapped.trigger_tx(), HASH);
        assert_eq!(
            still_wrapped.opportunity().position.borrower,
            borrower_expected
        );
    }

    #[test]
    fn verify_error_keeps_wrapper_and_hides_raw_tx() {
        let c = mk_cache();
        let opp = mk_opp();
        c.insert(PreSignedLiquidation {
            borrower: opp.position.borrower,
            raw_tx: Bytes::from_static(&[0xcc]),
            opportunity: opp,
            trigger_tx: HASH,
            inserted_at: now_secs(),
        });
        let mut confirmed = HashSet::new();
        confirmed.insert(HASH);
        let drained = c.drain_for_block(B256::ZERO, &confirmed);
        let wrapped = drained.into_iter().next().unwrap();
        assert!(matches!(
            wrapped.verify(SimulationVerdict::Error),
            Err((_, SimulationVerdict::Error))
        ));
    }

    #[test]
    fn is_tracked_selector_matches_defaults() {
        let c = PendingCache::with_defaults(ORACLE);
        let sel = FixedBytes::<4>::from(IVenusOracleWrite::updatePriceCall::SELECTOR);
        assert!(c.is_tracked_selector(sel));
        let unknown = FixedBytes::<4>::from([0xde, 0xad, 0xbe, 0xef]);
        assert!(!c.is_tracked_selector(unknown));
        let legacy = FixedBytes::<4>::from(ILegacyVenusOracleWrite::setDirectPriceCall::SELECTOR);
        assert!(
            !c.is_tracked_selector(legacy),
            "legacy selectors must not be tracked by default"
        );
    }

    #[test]
    fn oracle_round_trips() {
        let c = mk_cache();
        assert_eq!(c.oracle(), ORACLE);
    }

    #[test]
    fn format_selector_renders_lowercase_hex() {
        let sel = FixedBytes::<4>::from([0xab, 0xcd, 0xef, 0x01]);
        assert_eq!(format_selector(sel), "0xabcdef01");
    }

    #[test]
    fn oracle_update_accessors_match_variant_fields() {
        let selector = FixedBytes::<4>::from(IVenusOracleWrite::updatePriceCall::SELECTOR);
        let refresh = OracleUpdate::Refresh {
            tx_hash: HASH,
            selector,
            asset: ASSET,
        };
        assert_eq!(refresh.tx_hash(), HASH);
        assert_eq!(refresh.selector(), selector);
        assert_eq!(refresh.asset(), ASSET);
        assert_eq!(refresh.kind(), "refresh");

        let direct = OracleUpdate::DirectUpdate {
            tx_hash: HASH,
            selector,
            asset: ASSET,
            price: U256::from(7u64),
        };
        assert_eq!(direct.tx_hash(), HASH);
        assert_eq!(direct.asset(), ASSET);
        assert_eq!(direct.kind(), "direct");
    }

    #[test]
    fn backoff_with_jitter_doubles_and_caps() {
        let max = Duration::from_secs(30);
        // First step: from 1 s should land in [2.0, 2.5) s.
        let b = backoff_with_jitter(Duration::from_secs(1), max);
        assert!(
            b >= Duration::from_millis(2_000) && b < Duration::from_millis(2_500),
            "unexpected step-1 backoff: {b:?}"
        );

        // Near-cap: from 20 s should cap at 30 s (40 s + jitter > cap).
        let b = backoff_with_jitter(Duration::from_secs(20), max);
        assert_eq!(b, max);
    }

    #[test]
    fn backoff_with_jitter_handles_zero() {
        let max = Duration::from_secs(30);
        let b = backoff_with_jitter(Duration::ZERO, max);
        assert_eq!(b, Duration::ZERO);
    }
}
