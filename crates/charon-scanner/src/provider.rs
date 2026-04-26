//! Per-chain RPC connection surface.
//!
//! Holds a WebSocket provider for a single chain. WebSocket is required for
//! `subscribe_blocks` / `subscribe_logs` — the scanner's hot path depends on
//! push events, not polling. One `ChainProvider` per configured chain;
//! multi-chain support is a config-driven fan-out at the call site.

use std::sync::Arc;
use std::time::Duration;

use alloy::providers::{Provider, ProviderBuilder, RootProvider, WsConnect};
use alloy::pubsub::PubSubFrontend;
use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use charon_core::config::ChainConfig;
use tokio::time::timeout;
use tracing::debug;

/// Default deadline for the initial WebSocket handshake.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Trait abstraction over a chain RPC surface.
///
/// Exists so downstream scanner logic can be unit-tested against a
/// `MockChainProvider` without a live BSC node. The concrete
/// [`ChainProvider`] is the production impl.
#[async_trait]
pub trait ChainProviderT: Send + Sync {
    /// Short name of the chain (`[chain.<name>]` key).
    fn name(&self) -> &str;
    /// Latest block number over the underlying transport.
    async fn get_block_number(&self) -> Result<u64>;
}

/// WebSocket RPC wrapper for one chain.
///
/// The `name` field matches the `[chain.<name>]` key from the config, so
/// logs and errors can be attributed to the chain by its short name
/// (e.g. `"bnb"`). Returned from [`connect`] wrapped in `Arc` so the
/// provider can be cheaply shared across tokio tasks.
pub struct ChainProvider {
    name: String,
    ws: RootProvider<PubSubFrontend>,
}

impl ChainProvider {
    /// Connect over WebSocket, verify chain id matches config, return `Arc<Self>`.
    ///
    /// Fails with a contextualized, URL-redacted error if:
    /// - the WS handshake does not complete within [`DEFAULT_CONNECT_TIMEOUT`];
    /// - `eth_chainId` does not match `config.chain_id`.
    ///
    /// No panics, no silent fallbacks. Embedded API keys in the RPC URL are
    /// never printed — logs show only the URL's scheme + host portion.
    pub async fn connect(name: impl Into<String>, config: &ChainConfig) -> Result<Arc<Self>> {
        Self::connect_with_timeout(name, config, DEFAULT_CONNECT_TIMEOUT).await
    }

    /// As [`connect`] but with a caller-chosen deadline on the handshake.
    pub async fn connect_with_timeout(
        name: impl Into<String>,
        config: &ChainConfig,
        deadline: Duration,
    ) -> Result<Arc<Self>> {
        let name = name.into();
        let safe_url = redact_url(&config.ws_url);
        debug!(chain = %name, url = %safe_url, "connecting ws provider");

        let ws = WsConnect::new(&config.ws_url);
        let provider = timeout(deadline, ProviderBuilder::new().on_ws(ws))
            .await
            .map_err(|_| {
                anyhow!(
                    "chain '{name}': ws connect timed out after {}s to {safe_url}",
                    deadline.as_secs()
                )
            })?
            .with_context(|| format!("chain '{name}': failed to connect over ws to {safe_url}"))?;

        // Chain id verification — reject a misconfigured endpoint pointing at
        // the wrong network before any state-dependent call runs.
        let actual_chain_id = provider
            .get_chain_id()
            .await
            .with_context(|| format!("chain '{name}': eth_chainId probe failed"))?;
        if actual_chain_id != config.chain_id {
            bail!(
                "chain '{name}': expected chain id {}, got {actual_chain_id}",
                config.chain_id
            );
        }

        Ok(Arc::new(Self { name, ws: provider }))
    }

    /// Short name of the chain (matches the `[chain.<name>]` key).
    pub fn name(&self) -> &str {
        &self.name
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

#[async_trait]
impl ChainProviderT for ChainProvider {
    fn name(&self) -> &str {
        self.name()
    }
    async fn get_block_number(&self) -> Result<u64> {
        self.test_connection().await
    }
}

/// Compile-time assertion that `ChainProvider` is safe to share across
/// tokio tasks.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ChainProvider>();
};

/// Return an RPC URL with the final path segment (commonly the API-key slug)
/// replaced by `<redacted>`. Preserves scheme + host so logs stay useful.
fn redact_url(url: &str) -> String {
    let (scheme_end, rest) = match url.find("://") {
        Some(i) => (i + 3, &url[i + 3..]),
        None => return "<redacted>".to_string(),
    };
    let (host, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    if tail.is_empty() || tail == "/" {
        format!("{}{host}{tail}", &url[..scheme_end])
    } else {
        format!("{}{host}/<redacted>", &url[..scheme_end])
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;

    #[test]
    fn redact_url_strips_api_key_path() {
        assert_eq!(
            redact_url("wss://bsc-mainnet.nodereal.io/ws/v1/ABCDEFG"),
            "wss://bsc-mainnet.nodereal.io/<redacted>"
        );
    }

    #[test]
    fn redact_url_keeps_bare_host() {
        assert_eq!(
            redact_url("wss://bsc-rpc.publicnode.com"),
            "wss://bsc-rpc.publicnode.com"
        );
    }

    #[test]
    fn redact_url_handles_missing_scheme() {
        assert_eq!(redact_url("bsc-rpc.publicnode.com/key"), "<redacted>");
    }
}

/// In-memory [`ChainProviderT`] implementation for unit tests.
///
/// Feeds deterministic block numbers to downstream logic without touching
/// the network. `name` defaults to `"mock"`.
pub struct MockChainProvider {
    pub name: String,
    pub block_number: std::sync::atomic::AtomicU64,
}

impl MockChainProvider {
    pub fn new(block_number: u64) -> Arc<Self> {
        Arc::new(Self {
            name: "mock".into(),
            block_number: std::sync::atomic::AtomicU64::new(block_number),
        })
    }
}

#[async_trait]
impl ChainProviderT for MockChainProvider {
    fn name(&self) -> &str {
        &self.name
    }
    async fn get_block_number(&self) -> Result<u64> {
        Ok(self.block_number.load(std::sync::atomic::Ordering::Relaxed))
    }
}
