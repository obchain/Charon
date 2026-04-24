//! Block listener — the bot's heartbeat.
//!
//! Subscribes to `newHeads` over WebSocket and forwards each block into the
//! scanning pipeline via an `mpsc` channel. Wrapped in an outer loop so a
//! dropped WebSocket triggers a reconnect with jittered exponential backoff
//! instead of killing the bot. Reconnects backfill any blocks produced
//! during the disconnect window so the scanner never silently skips heads.

use std::time::Duration;

use alloy::primitives::B256;
use alloy::providers::Provider;
use anyhow::{Context, Result};
use charon_core::config::ChainConfig;
use futures_util::StreamExt;
use rand::Rng;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::provider::ChainProvider;

/// Maximum reconnect backoff. BSC blocks every ~3 s, so 30 s is ~10 missed
/// blocks — the ceiling for how many we are willing to backfill at once.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Upstream chain events produced by the listener.
///
/// Held in a dedicated enum so the pipeline can grow new event kinds
/// (`ProtocolLog`, `OraclePriceUpdate`, …) without changing channel types.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ChainEvent {
    /// A new head block arrived on the chain.
    NewBlock {
        /// Short chain name (matches `[chain.<name>]` key in config).
        chain: String,
        number: u64,
        /// Unix timestamp from the block header.
        timestamp: u64,
        /// Canonical block hash of the new head. Required by the
        /// mempool pre-sign drain so it can correlate its log with the
        /// block that triggered the drain and to let consumers fetch
        /// the block's confirmed tx-hash set in a follow-up call.
        block_hash: B256,
        /// `true` if the block was synthesised via reconnect backfill.
        backfill: bool,
    },
}

/// Listens to a single chain's `newHeads` stream and forwards events.
///
/// The listener owns a fresh `ChainProvider` for each connection attempt,
/// so a WebSocket drop recovers cleanly by reconnecting from scratch on
/// the next loop iteration.
pub struct BlockListener {
    name: String,
    config: ChainConfig,
    tx: mpsc::Sender<ChainEvent>,
    last_seen: Option<u64>,
}

impl BlockListener {
    /// Build a listener for a single chain.
    pub fn new(
        name: impl Into<String>,
        config: ChainConfig,
        tx: mpsc::Sender<ChainEvent>,
    ) -> Self {
        Self {
            name: name.into(),
            config,
            tx,
            last_seen: None,
        }
    }

    /// Run the listener forever. Reconnects with jittered exponential backoff
    /// on any connection or subscription error. Returns `Ok(())` only if the
    /// receiving side of the channel is dropped.
    pub async fn run(mut self) -> Result<()> {
        let mut backoff = Duration::from_secs(1);
        loop {
            metrics::counter!("charon_listener_connects_total", "chain" => self.name.clone())
                .increment(1);
            match self.run_once().await {
                Ok(()) => {
                    info!(chain = %self.name, "listener channel closed, exiting");
                    return Ok(());
                }
                Err(err) => {
                    metrics::counter!(
                        "charon_listener_disconnects_total",
                        "chain" => self.name.clone()
                    )
                    .increment(1);
                    let jitter_ms = rand::thread_rng()
                        .gen_range(0..=(backoff.as_millis() as u64).saturating_div(4));
                    let wait = backoff + Duration::from_millis(jitter_ms);
                    warn!(
                        chain = %self.name,
                        error = ?err,
                        wait_ms = wait.as_millis() as u64,
                        "listener error, reconnecting after jittered backoff"
                    );
                    tokio::time::sleep(wait).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    /// One connect → subscribe → drain cycle. Returns `Ok(())` if the
    /// stream ends cleanly (receiver dropped); returns `Err` on any
    /// connection or subscription failure so `run` can retry.
    async fn run_once(&mut self) -> Result<()> {
        let provider = ChainProvider::connect(&self.name, &self.config).await?;

        // Backfill blocks produced during any prior disconnect window.
        if let Some(last) = self.last_seen {
            let head = provider
                .provider()
                .get_block_number()
                .await
                .with_context(|| format!("chain '{}': get_block_number failed", self.name))?;
            if head > last + 1 {
                let gap = head - (last + 1);
                warn!(
                    chain = %self.name,
                    from = last + 1,
                    to = head - 1,
                    gap,
                    "reconnect gap detected — backfilling"
                );
                for number in (last + 1)..head {
                    let header = provider
                        .provider()
                        .get_block_by_number(number.into(), false.into())
                        .await
                        .with_context(|| {
                            format!("chain '{}': get_block_by_number({number}) failed", self.name)
                        })?;
                    let (ts, hash) = header
                        .map(|b| (b.header.timestamp, b.header.hash))
                        .unwrap_or_default();
                    self.publish(number, ts, hash, true);
                }
            }
        }

        let sub = provider
            .provider()
            .subscribe_blocks()
            .await
            .with_context(|| format!("chain '{}': subscribe_blocks failed", self.name))?;
        info!(chain = %self.name, "block subscription established");

        let mut stream = sub.into_stream();
        while let Some(header) = stream.next().await {
            self.publish(header.number, header.timestamp, header.hash, false);
        }

        anyhow::bail!("chain '{}': subscription stream ended", self.name)
    }

    /// Emit a `ChainEvent::NewBlock` into the channel. Non-blocking so a
    /// stalled consumer cannot stall the WebSocket drain loop; full channel
    /// drops the event with a warning (back-pressure visible to ops).
    fn publish(&mut self, number: u64, timestamp: u64, block_hash: B256, backfill: bool) {
        metrics::counter!("charon_blocks_received_total", "chain" => self.name.clone())
            .increment(1);
        debug!(
            chain = %self.name,
            block = number,
            timestamp,
            %block_hash,
            backfill,
            "new block"
        );
        self.last_seen = Some(match self.last_seen {
            Some(prev) => prev.max(number),
            None => number,
        });
        let event = ChainEvent::NewBlock {
            chain: self.name.clone(),
            number,
            timestamp,
            block_hash,
            backfill,
        };
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                metrics::counter!(
                    "charon_listener_dropped_events_total",
                    "chain" => self.name.clone()
                )
                .increment(1);
                warn!(
                    chain = %self.name,
                    block = number,
                    "channel full — block dropped"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                debug!(chain = %self.name, "receiver closed, stop publishing");
            }
        }
    }
}
