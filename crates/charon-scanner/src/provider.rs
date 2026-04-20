//! Per-chain RPC connection surface.
//!
//! For v0.1 we wrap a single chain's HTTP provider (one-shot reads such as
//! `get_block_number` and multicall). The WebSocket provider for block
//! subscriptions is added alongside the block listener in the next issue.

use alloy::providers::{Provider, ProviderBuilder, RootProvider};
use alloy::transports::BoxTransport;
use anyhow::{Context, Result};
use charon_core::config::ChainConfig;
use tracing::debug;

/// Wraps the RPC connections for a single chain.
///
/// The struct is intentionally owned by name so logs and errors can refer
/// to the chain by its config key (e.g. `"bnb"`).
pub struct ChainProvider {
    /// Short name of the chain (matches the `[chain.<name>]` key in config).
    pub name: String,
    /// HTTP provider — reliable for one-shot reads and multicall.
    ///
    /// `BoxTransport` erases the concrete transport type so we can swap
    /// http / https / ws / wss behind one field via `on_builtin`.
    http: RootProvider<BoxTransport>,
}

impl ChainProvider {
    /// Connect to the chain's HTTP RPC.
    ///
    /// Accepts URL schemes `http(s)://` and `ws(s)://` — alloy's
    /// `on_builtin` auto-selects the right transport.
    pub async fn connect(name: impl Into<String>, config: &ChainConfig) -> Result<Self> {
        let name = name.into();
        debug!(chain = %name, url = %config.http_url, "connecting http provider");

        let http = ProviderBuilder::new()
            .on_builtin(&config.http_url)
            .await
            .with_context(|| format!("chain '{name}': failed to connect to {}", config.http_url))?;

        Ok(Self { name, http })
    }

    /// Fetch the latest block number. Lightweight RPC health check.
    pub async fn test_connection(&self) -> Result<u64> {
        self.http
            .get_block_number()
            .await
            .with_context(|| format!("chain '{}': get_block_number failed", self.name))
    }
}
