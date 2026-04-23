//! Per-chain RPC connection surface.
//!
//! Holds a WebSocket provider for a single chain. WebSocket is required for
//! `subscribe_blocks` / `subscribe_logs` — the scanner's hot path depends on
//! push events, not polling. One `ChainProvider` per configured chain;
//! multi-chain support is a config-driven fan-out at the call site.

use alloy::providers::{Provider, ProviderBuilder, RootProvider, WsConnect};
use alloy::pubsub::PubSubFrontend;
use anyhow::{Context, Result, bail};
use charon_core::config::ChainConfig;
use tracing::debug;

/// WebSocket RPC wrapper for one chain.
///
/// The `name` field matches the `[chain.<name>]` key from the config, so
/// logs and errors can be attributed to the chain by its short name
/// (e.g. `"bnb"`).
pub struct ChainProvider {
    /// Short name of the chain (matches the `[chain.<name>]` key).
    pub name: String,
    ws: RootProvider<PubSubFrontend>,
}

impl ChainProvider {
    /// Connect over WebSocket to the chain's RPC endpoint.
    ///
    /// Takes the chain's short name (for logging) and its [`ChainConfig`].
    /// Fails with a contextualized error if the WS handshake does not
    /// succeed — no panics, no silent fallbacks. After the handshake
    /// succeeds, the remote chain id is read via `eth_chainId` and
    /// compared against [`ChainConfig::chain_id`]; a mismatch aborts
    /// startup with a diagnostic naming both values. Without this
    /// check, a testnet profile accidentally paired with a mainnet
    /// RPC URL (or vice versa) would connect cleanly and then silently
    /// hit the wrong addresses with zero visible symptom (see #248).
    pub async fn connect(name: impl Into<String>, config: &ChainConfig) -> Result<Self> {
        let name = name.into();
        debug!(chain = %name, url = %config.ws_url, "connecting ws provider");

        let ws = WsConnect::new(&config.ws_url);
        let provider = ProviderBuilder::new().on_ws(ws).await.with_context(|| {
            format!(
                "chain '{name}': failed to connect over ws to {}",
                config.ws_url
            )
        })?;

        let rpc_chain_id = provider
            .get_chain_id()
            .await
            .with_context(|| format!("chain '{name}': eth_chainId read failed"))?;
        if rpc_chain_id != config.chain_id {
            bail!(
                "chain '{name}': chain_id mismatch — config declares {} but RPC {} reports {}. \
                 Check that [chain.{name}].chain_id and the RPC URL point at the same network.",
                config.chain_id,
                config.ws_url,
                rpc_chain_id
            );
        }

        Ok(Self { name, ws: provider })
    }

    /// Borrow the underlying pub-sub provider.
    ///
    /// Consumers (block listener, scanner, executor) use this to build
    /// subscriptions and make one-shot reads without re-establishing a
    /// connection.
    pub fn provider(&self) -> &RootProvider<PubSubFrontend> {
        &self.ws
    }

    /// Fetch the latest block number over WebSocket. Lightweight health check.
    pub async fn test_connection(&self) -> Result<u64> {
        self.ws
            .get_block_number()
            .await
            .with_context(|| format!("chain '{}': get_block_number failed", self.name))
    }
}
