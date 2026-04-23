//! Private-RPC transaction submitter.
//!
//! Thin wrapper around `eth_sendRawTransaction`. Primary job: post the
//! raw signed bytes produced by [`crate::builder::TxBuilder::sign`] to
//! a private-RPC endpoint (bloxroute / blocknative on BSC, sequencer
//! URLs on L2s) so pending transactions never hit the public mempool.
//!
//! Retries once on timeout — BSC blocks are ~3 s, so a 6 s ceiling
//! tolerates one network hiccup without letting a stuck submission
//! silently burn an opportunity.

use std::time::Duration;

use alloy::primitives::{Bytes, TxHash};
use alloy::providers::{Provider, ProviderBuilder, RootProvider};
use alloy::transports::BoxTransport;
use anyhow::{Context, Result};
use charon_metrics::{endpoint_kind, record_rpc_error, rpc_error, rpc_method, time_rpc};
use tracing::{info, warn};

/// Default submission timeout per attempt (6 s ≈ 8 BSC blocks).
pub const DEFAULT_SUBMIT_TIMEOUT: Duration = Duration::from_secs(6);

/// Number of attempts before giving up. One retry catches a single
/// transient timeout; more than that usually means a real outage.
const MAX_ATTEMPTS: u32 = 2;

/// Transaction submitter bound to one RPC endpoint.
///
/// Holds an owned provider so each submission reuses the underlying
/// HTTP client / connection pool. Cheap to clone — the provider is
/// reference-counted internally.
#[derive(Debug)]
pub struct Submitter {
    provider: RootProvider<BoxTransport>,
    endpoint: String,
    timeout: Duration,
}

impl Submitter {
    /// Connect to the submission endpoint. Accepts any URL scheme
    /// `on_builtin` supports (`https://`, `http://`, `wss://`, `ws://`).
    pub async fn connect(url: impl Into<String>, timeout: Duration) -> Result<Self> {
        let endpoint = url.into();
        let provider = ProviderBuilder::new()
            .on_builtin(&endpoint)
            .await
            .with_context(|| format!("submitter: failed to connect to {endpoint}"))?;
        info!(endpoint = %endpoint, timeout_secs = timeout.as_secs(), "submitter ready");
        Ok(Self {
            provider,
            endpoint,
            timeout,
        })
    }

    /// Endpoint this submitter posts to — useful for logging.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Submit raw signed transaction bytes. Retries once on timeout.
    /// Non-timeout RPC errors (revert, bad nonce, bad signature) fail
    /// immediately — no point retrying a deterministic rejection.
    ///
    /// Each attempt is instrumented via
    /// [`charon_metrics::time_rpc`] under
    /// [`rpc_method::ETH_SEND_RAW_TRANSACTION`] /
    /// [`endpoint_kind::PRIVATE`]; failures are classified into
    /// [`rpc_error::TIMEOUT`] vs [`rpc_error::REJECTED`] so dashboards
    /// can separate flaky-relay symptoms from deterministic
    /// rejections (bad nonce, revert, bad signature). Issue #302.
    pub async fn submit(&self, raw: Bytes) -> Result<TxHash> {
        for attempt in 1..=MAX_ATTEMPTS {
            let fut = self.provider.send_raw_transaction(&raw);
            // `time_rpc` is wrapped *inside* the outer
            // `tokio::time::timeout`, so its histogram sample only
            // lands when the provider future resolves within the
            // submission ceiling (success + rejection branches). A
            // hard timeout skips the sample — by construction it
            // would be `~self.timeout` and carries no extra signal;
            // the timeout branch is covered by
            // `charon_rpc_errors_total{error_kind="timeout"}`
            // instead.
            let timed = tokio::time::timeout(
                self.timeout,
                time_rpc(
                    rpc_method::ETH_SEND_RAW_TRANSACTION,
                    endpoint_kind::PRIVATE,
                    fut,
                ),
            )
            .await;
            match timed {
                Ok(Ok(pending)) => {
                    let hash = *pending.tx_hash();
                    info!(
                        endpoint = %self.endpoint,
                        %hash,
                        attempt,
                        "tx submitted"
                    );
                    return Ok(hash);
                }
                Ok(Err(err)) => {
                    record_rpc_error(
                        rpc_method::ETH_SEND_RAW_TRANSACTION,
                        rpc_error::REJECTED,
                    );
                    warn!(
                        endpoint = %self.endpoint,
                        attempt,
                        error = ?err,
                        "submit rejected by RPC — not retrying"
                    );
                    return Err(anyhow::anyhow!("submit RPC error: {err}"));
                }
                Err(_) => {
                    record_rpc_error(
                        rpc_method::ETH_SEND_RAW_TRANSACTION,
                        rpc_error::TIMEOUT,
                    );
                    warn!(
                        endpoint = %self.endpoint,
                        attempt,
                        timeout_secs = self.timeout.as_secs(),
                        "submit timed out"
                    );
                    if attempt == MAX_ATTEMPTS {
                        anyhow::bail!(
                            "submit: timed out {} times at {}",
                            MAX_ATTEMPTS,
                            self.endpoint
                        );
                    }
                }
            }
        }
        unreachable!("loop exits via return or bail!")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_rejects_invalid_url() {
        let err = Submitter::connect("not-a-real-scheme://nope", DEFAULT_SUBMIT_TIMEOUT)
            .await
            .expect_err("invalid URL should fail");
        // Just verify we got an error with context — exact message
        // depends on alloy's transport layer.
        assert!(
            format!("{err:#}").contains("submitter"),
            "error should be annotated with submitter context"
        );
    }

    #[test]
    fn default_timeout_is_six_seconds() {
        assert_eq!(DEFAULT_SUBMIT_TIMEOUT, Duration::from_secs(6));
    }
}
