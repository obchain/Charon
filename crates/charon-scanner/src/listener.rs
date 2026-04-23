//! Block listener — the bot's heartbeat.
//!
//! Subscribes to `newHeads` over WebSocket and forwards each block into the
//! scanning pipeline via an `mpsc` channel. Wrapped in an outer loop so a
//! dropped WebSocket triggers a reconnect with exponential backoff instead
//! of killing the bot.

use std::time::Duration;

use alloy::providers::Provider;
use anyhow::{Context, Result};
use charon_core::config::ChainConfig;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::provider::ChainProvider;

/// Upstream chain events produced by the listener.
///
/// Held in a dedicated enum so the pipeline can grow new event kinds
/// (`ProtocolLog`, `OraclePriceUpdate`, …) without changing channel types.
#[derive(Debug, Clone)]
pub enum ChainEvent {
    /// A new head block arrived on the chain.
    NewBlock {
        /// Short chain name (matches `[chain.<name>]` key in config).
        chain: String,
        number: u64,
        /// Unix timestamp from the block header.
        timestamp: u64,
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
}

impl BlockListener {
    /// Build a listener for a single chain.
    pub fn new(name: impl Into<String>, config: ChainConfig, tx: mpsc::Sender<ChainEvent>) -> Self {
        Self {
            name: name.into(),
            config,
            tx,
        }
    }

    /// Run the listener forever. Reconnects with exponential backoff on
    /// any connection or subscription error. Returns `Ok(())` only if the
    /// receiving side of the channel is dropped.
    ///
    /// Increments
    /// [`charon_rpc_connection_reconnects_total`](charon_metrics::names::RPC_RECONNECTS_TOTAL)
    /// under `endpoint_kind="public"` on every reconnect attempt
    /// (issue #302) — the `newHeads` stream rides the chain's
    /// public pubsub endpoint.
    pub async fn run(self) -> Result<()> {
        let mut backoff = Duration::from_secs(1);
        loop {
            match self.run_once().await {
                Ok(()) => {
                    // Receiver dropped — no one is listening, exit quietly.
                    info!(chain = %self.name, "listener channel closed, exiting");
                    return Ok(());
                }
                Err(err) => {
                    charon_metrics::record_rpc_reconnect(
                        charon_metrics::endpoint_kind::PUBLIC,
                    );
                    warn!(
                        chain = %self.name,
                        error = ?err,
                        backoff_secs = backoff.as_secs(),
                        "listener error, reconnecting after backoff"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    /// One connect → subscribe → drain cycle. Returns `Ok(())` if the
    /// stream ends cleanly (receiver dropped); returns `Err` on any
    /// connection or subscription failure so `run` can retry.
    async fn run_once(&self) -> Result<()> {
        let provider = ChainProvider::connect(&self.name, &self.config).await?;

        let sub = provider
            .provider()
            .subscribe_blocks()
            .await
            .with_context(|| format!("chain '{}': subscribe_blocks failed", self.name))?;

        info!(chain = %self.name, "block subscription established");

        let mut stream = sub.into_stream();
        while let Some(header) = stream.next().await {
            let number = header.number;
            let timestamp = header.timestamp;

            info!(
                chain = %self.name,
                block = number,
                timestamp = timestamp,
                "new block"
            );

            let event = ChainEvent::NewBlock {
                chain: self.name.clone(),
                number,
                timestamp,
            };

            if self.tx.send(event).await.is_err() {
                // Receiver dropped; propagate clean shutdown up to `run`.
                return Ok(());
            }
        }

        // Stream terminated without error — the underlying ws likely closed.
        // Surface as an error so `run` reconnects instead of exiting.
        anyhow::bail!("chain '{}': subscription stream ended", self.name)
    }
}
