//! Private-RPC transaction submitter.
//!
//! Thin wrapper around `eth_sendRawTransaction`. Primary job: post the
//! raw signed bytes produced by [`crate::builder::TxBuilder::sign`] to
//! a private-RPC endpoint (bloxroute / blocknative on BSC, sequencer
//! URLs on L2s) so pending transactions never hit the public mempool.
//!
//! # Safety invariants
//!
//! 1. **No public-mempool fallback.** [`Submitter::connect`] takes the
//!    private URL as an explicit, non-optional argument. Callers must
//!    obtain it from `ChainConfig::private_rpc_url`; the opt-out lives
//!    in [`charon_core::Config::validate`] behind the
//!    `allow_public_mempool` flag and must never be worked around here.
//! 2. **HTTPS / WSS only.** `http://`, `ws://`, missing scheme, and
//!    exotic schemes are rejected at connect time. Sending signed
//!    calldata over plaintext hands it to anyone on the wire.
//! 3. **Single-shot submission.** `submit()` makes exactly one RPC
//!    attempt, then returns. Retries (and the staleness decision that
//!    comes with them) belong to the caller, which also owns the
//!    opportunity queue TTL and re-quoting logic.
//! 4. **Secrets stay in `SecretString`.** URL and auth header are held
//!    in `secrecy::SecretString`; `Debug` is implemented manually to
//!    redact them. `expose_secret()` is called exactly once, at
//!    transport construction, and never passed to a tracing macro.

use std::time::Duration;

use alloy::primitives::{Bytes, TxHash};
use alloy::providers::{Provider, ProviderBuilder, RootProvider};
use alloy::pubsub::PubSubFrontend;
use alloy::rpc::client::ClientBuilder;
use alloy::transports::Authorization;
use alloy::transports::http::Http;
use alloy::transports::ws::WsConnect;
use alloy::transports::{BoxTransport, RpcError, TransportError, TransportErrorKind};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use tracing::{info, warn};

/// Default submission timeout (6 s ≈ 2 BSC blocks).
pub const DEFAULT_SUBMIT_TIMEOUT: Duration = Duration::from_secs(6);

/// Number of blocks to wait for inclusion before treating a tx as
/// stuck and triggering an RBF replacement. Three BSC blocks ≈ 9 s,
/// long enough that brief mempool-feedback delay does not look
/// stuck, short enough that a sustained gas spike doesn't cost more
/// than a few seconds of latency.
pub const REPLACE_AFTER_BLOCKS: u64 = 3;

/// Per-tx replacement budget. After this many bumps on the same
/// nonce, give up rather than burn unbounded fee on a sustained
/// gas spike.
pub const MAX_REPLACEMENTS_PER_NONCE: u32 = 3;

/// Replacement-fee bump in percent. geth's replacement floor is
/// 10 % per fee field; 12 % gives a comfortable margin so a router
/// that strips a fraction of a wei does not silently drop the
/// replacement.
pub const REPLACEMENT_BUMP_PCT: u32 = 12;

/// Typed failure modes surfaced by the submitter.
///
/// The enum is `#[non_exhaustive]` so new variants (e.g. circuit-breaker
/// trip, vendor-specific nonce-gap response) can land without a breaking
/// change to callers that already match exhaustively.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SubmitError {
    /// Submission did not complete within the timeout. The caller
    /// owns the retry/drop decision; the submitter does not
    /// second-guess it.
    #[error("submission timed out after {0:?}")]
    Timeout(Duration),

    /// RPC returned a non-timeout error (revert, bad nonce, bad
    /// signature, 4xx, rate-limit). Deterministic and not worth
    /// retrying on the same inputs.
    #[error("rpc rejected: {0}")]
    RpcRejected(String),

    /// Transport-level failure (TCP reset, TLS error, DNS, websocket
    /// close, 5xx). The caller should rebuild the `Submitter` on the
    /// next tick — the existing provider may be poisoned.
    #[error("connection lost: {0}")]
    ConnectionLost(#[source] TransportError),

    /// URL scheme is not `https://` or `wss://`. Plaintext submission
    /// of signed calldata is never acceptable.
    #[error("insecure scheme: {0}")]
    InsecureScheme(String),

    /// URL could not be parsed, or the auth header value contained
    /// characters that cannot appear in an HTTP header.
    #[error("invalid endpoint configuration: {0}")]
    InvalidEndpoint(String),

    /// Connect-time `eth_chainId` probe failed (timeout, RPC rejection,
    /// transport error). The endpoint is unusable; the caller must
    /// abort startup rather than start signing into a black hole.
    #[error("connect probe failed: {0}")]
    ConnectFailed(String),

    /// Connect-time chain id probe returned a value that does not
    /// match the chain the bot is configured for. Refusing to start
    /// avoids broadcasting BSC-signed liquidations into a different
    /// chain id.
    #[error("chain id mismatch: expected {expected}, endpoint reports {actual}")]
    ChainIdMismatch { expected: u64, actual: u64 },
}

/// One of the two underlying transports the submitter can hold.
enum Inner {
    Http(RootProvider<BoxTransport>),
    Ws(RootProvider<PubSubFrontend>),
}

/// Transaction submitter bound to one private RPC endpoint.
///
/// `Debug` is implemented manually so the raw endpoint URL (which may
/// embed an API key in the path or query string) never appears in logs
/// or panic traces.
pub struct Submitter {
    inner: Inner,
    /// Sanitised label for logs: scheme + host only, no path / query.
    endpoint_label: String,
    timeout: Duration,
}

impl std::fmt::Debug for Submitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Submitter")
            .field("endpoint", &self.endpoint_label)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl Submitter {
    /// Connect to a private RPC endpoint.
    ///
    /// - `url` is the private-RPC URL. Must use `https://` or `wss://`;
    ///   `http://`, `ws://`, missing scheme, and exotic schemes are
    ///   rejected with [`SubmitError::InsecureScheme`].
    /// - `auth` is an optional bearer token. When provided, every HTTP
    ///   request carries `Authorization: Bearer <token>`. For WSS the
    ///   header is attached during the handshake via [`WsConnect`].
    /// - `expected_chain_id` is the chain id this submitter is bound
    ///   to. After the transport is built, `connect` issues one
    ///   `eth_chainId` probe (2 s timeout). On failure the endpoint is
    ///   considered unusable and the function returns
    ///   [`SubmitError::ConnectFailed`]; on a value that does not
    ///   match `expected_chain_id` it returns
    ///   [`SubmitError::ChainIdMismatch`]. The probe surfaces a dead /
    ///   misconfigured / cross-chain endpoint at startup rather than
    ///   inside the first opportunity broadcast (#358).
    /// - `timeout` bounds a single submission attempt.
    ///
    /// The URL stays inside the `SecretString` the caller owns; this
    /// function derives a host-only label for logging and calls
    /// `expose_secret()` exactly once, at transport construction.
    pub async fn connect(
        url: &SecretString,
        auth: Option<&SecretString>,
        expected_chain_id: u64,
        timeout: Duration,
        allow_loopback: bool,
    ) -> Result<Self, SubmitError> {
        let raw = url.expose_secret();
        let parsed = url::Url::parse(raw)
            .map_err(|e| SubmitError::InvalidEndpoint(format!("invalid URL: {e}")))?;

        let scheme = parsed.scheme();
        // The default safety invariant — plaintext schemes are
        // refused — still holds for every mainnet-bound submitter.
        // `allow_loopback` is a fork-only escape hatch (issue #396):
        // it tolerates `http://127.0.0.1` / `ws://127.0.0.1` /
        // `http://localhost` / `ws://localhost` so the bot can
        // broadcast into a local anvil endpoint, but explicitly
        // refuses non-loopback hosts even when the flag is set so a
        // misconfigured fork profile cannot leak signed calldata to
        // a public mempool.
        match scheme {
            "https" | "wss" => {}
            "http" | "ws" if allow_loopback && is_loopback_host(&parsed) => {}
            _ => return Err(SubmitError::InsecureScheme(scheme.to_string())),
        }

        // Host-only label for logging — hides API keys that vendors
        // embed in the path or query string.
        let endpoint_label = match parsed.host_str() {
            Some(h) => match parsed.port() {
                Some(p) => format!("{scheme}://{h}:{p}"),
                None => format!("{scheme}://{h}"),
            },
            None => scheme.to_string(),
        };

        let inner = match scheme {
            "https" | "http" => {
                // alloy's `is_local` flag controls the RPC client's
                // poll-interval (250 ms when local, 7 s otherwise).
                // The only path that reaches `http` here is the
                // loopback-anvil fork escape hatch, so the shorter
                // interval is the right knob — pending-tx and filter
                // pollers reflect anvil's mined blocks immediately
                // instead of every 7 s.
                let is_local = scheme == "http";
                let client = build_reqwest_client(auth)?;
                let http = Http::with_client(client, parsed);
                let rpc_client = ClientBuilder::default().transport(http, is_local);
                let provider: RootProvider<BoxTransport> =
                    ProviderBuilder::new().on_client(rpc_client.boxed());
                Inner::Http(provider)
            }
            "wss" | "ws" => {
                let mut connect = WsConnect::new(raw.to_string());
                if let Some(a) = auth {
                    connect = connect.with_auth(Authorization::bearer(a.expose_secret()));
                }
                let provider = ProviderBuilder::new()
                    .on_ws(connect)
                    .await
                    .map_err(SubmitError::ConnectionLost)?;
                Inner::Ws(provider)
            }
            other => return Err(SubmitError::InsecureScheme(other.to_string())),
        };

        let actual_chain_id = probe_chain_id(&inner, &endpoint_label, expected_chain_id).await?;

        info!(
            endpoint = %endpoint_label,
            timeout_secs = timeout.as_secs(),
            auth = auth.is_some(),
            chain_id = actual_chain_id,
            "submitter ready"
        );
        Ok(Self {
            inner,
            endpoint_label,
            timeout,
        })
    }

    /// Host-only endpoint label — safe for logs; the secret part of
    /// the URL never reaches this accessor.
    pub fn endpoint(&self) -> &str {
        &self.endpoint_label
    }

    /// Re-broadcast a replacement for an in-flight tx (RBF, #364).
    ///
    /// Caller is responsible for re-encoding + re-signing `raw` with
    /// the same nonce as the stuck tx but `maxFeePerGas` and
    /// `maxPriorityFeePerGas` bumped by `>= REPLACEMENT_BUMP_PCT` —
    /// geth requires both fee fields to clear the replacement floor
    /// or the new tx is silently dropped from the pool.
    ///
    /// This method is a thin wrapper around [`Submitter::submit`]
    /// plus a post-condition: the new tx hash must differ from
    /// `original_hash`. A matching hash means the caller forgot to
    /// bump and the replacement was a no-op; surface it as
    /// [`SubmitError::RpcRejected`] rather than silently logging a
    /// "successful" replacement that is actually the same tx.
    /// Increments
    /// `charon_submit_replacements_total{chain, reason}` with the
    /// caller-supplied reason (`"ttl_expired"` / `"fee_spike"` / …).
    pub async fn replace(
        &self,
        raw: Bytes,
        original_hash: TxHash,
        chain: &str,
        reason: &str,
    ) -> Result<TxHash, SubmitError> {
        let new_hash = self.submit(raw).await?;
        if new_hash == original_hash {
            warn!(
                endpoint = %self.endpoint_label,
                %original_hash,
                "submit_replace: returned the same hash — caller did not bump fees"
            );
            return Err(SubmitError::RpcRejected(format!(
                "replacement returned the same hash {original_hash}; bump max_fee + priority_fee by ≥ {REPLACEMENT_BUMP_PCT}% before retrying"
            )));
        }
        info!(
            endpoint = %self.endpoint_label,
            %original_hash,
            %new_hash,
            chain,
            reason,
            "tx replacement broadcast"
        );
        charon_metrics::record_submit_replacement(chain, reason);
        Ok(new_hash)
    }

    /// Submit raw signed transaction bytes. Single attempt, no retry.
    ///
    /// The submitter is deliberately single-shot. Whether a timed-out
    /// broadcast is still worth re-sending (same price, same health
    /// factor, same gas ceiling) is a pipeline-level decision owned
    /// by the caller along with the opportunity queue TTL. For an
    /// explicit nonce-preserving re-broadcast with bumped fees, see
    /// [`Submitter::replace`] (#364).
    ///
    /// Error mapping:
    /// - elapsed deadline -> [`SubmitError::Timeout`]
    /// - JSON-RPC level error / 4xx / 429 -> [`SubmitError::RpcRejected`]
    /// - transport-level error / 5xx -> [`SubmitError::ConnectionLost`]
    pub async fn submit(&self, raw: Bytes) -> Result<TxHash, SubmitError> {
        // Each transport's pending-transaction builder has a
        // different generic parameter, so `async move { ... }` over
        // both arms gives incompatible types. Project to TxHash
        // inside each arm, yielding a uniform `Result<TxHash, _>`.
        let fut = async {
            match &self.inner {
                Inner::Http(p) => p
                    .send_raw_transaction(&raw)
                    .await
                    .map(|pending| *pending.tx_hash()),
                Inner::Ws(p) => p
                    .send_raw_transaction(&raw)
                    .await
                    .map(|pending| *pending.tx_hash()),
            }
        };

        // Wrap the provider send in `time_rpc` so the RPC-latency
        // histogram owns the sample regardless of outcome. Successes
        // and provider-side rejections both land a duration; the
        // hard timeout branch skips the histogram sample by
        // construction (its duration would be ~self.timeout and
        // carries no extra signal — `charon_rpc_errors_total{
        // error_kind="timeout"}` is the canonical surface for that
        // case). `endpoint_kind::PRIVATE` because the submitter only
        // ever posts to the per-chain `private_rpc_url`; the scanner
        // owns public reads.
        let timed = tokio::time::timeout(
            self.timeout,
            charon_metrics::time_rpc(
                charon_metrics::rpc_method::ETH_SEND_RAW_TRANSACTION,
                charon_metrics::endpoint_kind::PRIVATE,
                fut,
            ),
        )
        .await;

        match timed {
            Ok(Ok(hash)) => {
                info!(endpoint = %self.endpoint_label, %hash, "tx submitted");
                Ok(hash)
            }
            Ok(Err(err)) => {
                warn!(
                    endpoint = %self.endpoint_label,
                    error = %err,
                    "submit rejected by RPC"
                );
                // Tag the error counter with the bucket produced by
                // the full JSON-RPC / transport classifier so
                // Grafana pivots on the exact same `rejected` vs
                // `connection_lost` split that drives the typed
                // `SubmitError`.
                let classified = classify_transport_error(err);
                let kind = match &classified {
                    SubmitError::ConnectionLost(_) => charon_metrics::rpc_error::CONNECTION_LOST,
                    _ => charon_metrics::rpc_error::REJECTED,
                };
                charon_metrics::record_rpc_error(
                    charon_metrics::rpc_method::ETH_SEND_RAW_TRANSACTION,
                    kind,
                );
                Err(classified)
            }
            Err(_) => {
                warn!(
                    endpoint = %self.endpoint_label,
                    timeout_secs = self.timeout.as_secs(),
                    "submit timed out"
                );
                charon_metrics::record_rpc_error(
                    charon_metrics::rpc_method::ETH_SEND_RAW_TRANSACTION,
                    charon_metrics::rpc_error::TIMEOUT,
                );
                Err(SubmitError::Timeout(self.timeout))
            }
        }
    }
}

/// `true` only when the URL's host resolves to a syntactic loopback
/// — IPv4 `127.0.0.0/8`, IPv6 `::1`, or the literal `localhost` —
/// so the fork-mode escape hatch in [`Submitter::connect`] cannot
/// silently accept a non-loopback http/ws endpoint.
///
/// Trust boundary: this function reads `parsed.host()`, the
/// authority-component host that the WHATWG URL parser already
/// canonicalised. Hostile inputs that try to confuse a hand-rolled
/// substring matcher do not reach the matcher: e.g.
/// `http://127.0.0.1.evil.com` parses to `Domain("127.0.0.1.evil.com")`
/// (rejected), `http://example.com#127.0.0.1` parses to
/// `Domain("example.com")` (rejected), and
/// `http://user:pass@127.0.0.1@evil.com` parses to `Domain("evil.com")`
/// (rejected). The IPv4-mapped IPv6 form `::ffff:127.0.0.1` is
/// deliberately rejected too — `Ipv6Addr::is_loopback` returns false
/// for it, and conflating it with `::1` would weaken the invariant.
fn is_loopback_host(parsed: &url::Url) -> bool {
    use std::net::IpAddr;
    match parsed.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(addr)) => {
            // IpAddr::is_loopback covers 127.0.0.0/8 (RFC 1122).
            IpAddr::V4(addr).is_loopback()
        }
        Some(url::Host::Ipv6(addr)) => IpAddr::V6(addr).is_loopback(),
        None => false,
    }
}

/// 2 s budget for the connect-time `eth_chainId` probe. More than
/// enough for any healthy endpoint; bounded so a hung TCP cannot
/// block startup indefinitely.
const CONNECT_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Run the connect-time `eth_chainId` probe against either transport
/// and assert the result matches `expected_chain_id`. Split out from
/// [`Submitter::connect`] so unit tests can exercise the probe path
/// without standing up the full transport pipeline (and httpmock,
/// which serves plain HTTP that the public `connect` rejects by
/// design).
async fn probe_chain_id(
    inner: &Inner,
    endpoint_label: &str,
    expected_chain_id: u64,
) -> Result<u64, SubmitError> {
    let probe = async {
        match inner {
            Inner::Http(p) => p.get_chain_id().await,
            Inner::Ws(p) => p.get_chain_id().await,
        }
    };
    let actual_chain_id = match tokio::time::timeout(CONNECT_PROBE_TIMEOUT, probe).await {
        Ok(Ok(id)) => id,
        Ok(Err(err)) => {
            return Err(SubmitError::ConnectFailed(format!(
                "eth_chainId probe rejected at {endpoint_label}: {err}"
            )));
        }
        Err(_) => {
            return Err(SubmitError::ConnectFailed(format!(
                "eth_chainId probe timed out after {CONNECT_PROBE_TIMEOUT:?} at {endpoint_label}"
            )));
        }
    };
    if actual_chain_id != expected_chain_id {
        return Err(SubmitError::ChainIdMismatch {
            expected: expected_chain_id,
            actual: actual_chain_id,
        });
    }
    Ok(actual_chain_id)
}

/// Build a reqwest client that attaches `Authorization: Bearer <token>`
/// to every request when `auth` is `Some`. The `HeaderValue` is marked
/// sensitive so reqwest / hyper omit it from their own debug output.
fn build_reqwest_client(auth: Option<&SecretString>) -> Result<reqwest::Client, SubmitError> {
    let mut builder = reqwest::Client::builder();
    if let Some(token) = auth {
        let value = format!("Bearer {}", token.expose_secret());
        let mut header_value = HeaderValue::from_str(&value).map_err(|e| {
            SubmitError::InvalidEndpoint(format!("invalid Authorization header: {e}"))
        })?;
        header_value.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, header_value);
        builder = builder.default_headers(headers);
    }
    builder
        .build()
        .map_err(|e| SubmitError::InvalidEndpoint(format!("reqwest build failed: {e}")))
}

/// Split an alloy `TransportError` into an RPC-level rejection vs. a
/// connection-level loss.
///
/// The distinction matters: `RpcRejected` means the wire is fine but
/// the node did not like the request (revert, bad nonce, 4xx, 429).
/// The caller should not rebuild the submitter. `ConnectionLost`
/// means the caller should drop this submitter and reconnect on the
/// next tick.
fn classify_transport_error(err: TransportError) -> SubmitError {
    match err {
        RpcError::ErrorResp(ref payload) => {
            SubmitError::RpcRejected(format!("code={} msg={}", payload.code, payload.message))
        }
        RpcError::DeserError { .. } | RpcError::SerError(_) | RpcError::NullResp => {
            SubmitError::RpcRejected(err.to_string())
        }
        RpcError::Transport(TransportErrorKind::HttpError(ref http)) => {
            // 4xx (including 429) is a deterministic RPC-level rejection;
            // 5xx is server-side and means the caller should reconnect.
            // Read the structured status code rather than substring-matching
            // the rendered error — substring matches collide with random
            // ephemeral ports in transport error messages on Linux runners.
            if (400..500).contains(&http.status) {
                SubmitError::RpcRejected(format!("HTTP {} {}", http.status, http.body))
            } else {
                SubmitError::ConnectionLost(err)
            }
        }
        _ => SubmitError::ConnectionLost(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    fn sec(s: &str) -> SecretString {
        SecretString::from(s.to_string())
    }

    // ---- scheme enforcement ---------------------------------------------

    #[tokio::test]
    async fn connect_rejects_plain_http_scheme() {
        let err = Submitter::connect(
            &sec("http://example.com/rpc"),
            None,
            56,
            DEFAULT_SUBMIT_TIMEOUT,
            false,
        )
        .await
        .expect_err("http:// must be rejected");
        assert!(matches!(err, SubmitError::InsecureScheme(ref s) if s == "http"));
    }

    #[tokio::test]
    async fn connect_rejects_plain_ws_scheme() {
        let err = Submitter::connect(
            &sec("ws://example.com/rpc"),
            None,
            56,
            DEFAULT_SUBMIT_TIMEOUT,
            false,
        )
        .await
        .expect_err("ws:// must be rejected");
        assert!(matches!(err, SubmitError::InsecureScheme(ref s) if s == "ws"));
    }

    #[tokio::test]
    async fn connect_rejects_exotic_scheme() {
        let err = Submitter::connect(
            &sec("ftp://example.com/"),
            None,
            56,
            DEFAULT_SUBMIT_TIMEOUT,
            false,
        )
        .await
        .expect_err("ftp must be rejected");
        assert!(matches!(err, SubmitError::InsecureScheme(ref s) if s == "ftp"));
    }

    #[tokio::test]
    async fn connect_rejects_missing_scheme() {
        let err = Submitter::connect(
            &sec("example.com/rpc"),
            None,
            56,
            DEFAULT_SUBMIT_TIMEOUT,
            false,
        )
        .await
        .expect_err("missing scheme must be rejected");
        // url::Url::parse either fails outright (InvalidEndpoint) or
        // returns a non-http(s) scheme (InsecureScheme). Both are safe.
        assert!(matches!(
            err,
            SubmitError::InvalidEndpoint(_) | SubmitError::InsecureScheme(_)
        ));
    }

    /// Loopback http is rejected when the fork escape hatch is off
    /// (mainnet path) — guards against the flag silently being
    /// promoted to default.
    #[tokio::test]
    async fn connect_rejects_loopback_http_when_loopback_disabled() {
        let err = Submitter::connect(
            &sec("http://127.0.0.1:8545"),
            None,
            56,
            DEFAULT_SUBMIT_TIMEOUT,
            false,
        )
        .await
        .expect_err("loopback http must be rejected without allow_loopback");
        assert!(matches!(err, SubmitError::InsecureScheme(ref s) if s == "http"));
    }

    /// Non-loopback http is rejected even when the fork escape hatch
    /// is on — `allow_loopback = true` must never accept a non-loopback
    /// host. Guards against signed calldata leaking to a public mempool
    /// from a misconfigured fork profile.
    #[tokio::test]
    async fn connect_rejects_nonloopback_http_with_loopback_enabled() {
        let err = Submitter::connect(
            &sec("http://example.com/rpc"),
            None,
            56,
            DEFAULT_SUBMIT_TIMEOUT,
            true,
        )
        .await
        .expect_err("non-loopback http must be rejected even with allow_loopback");
        assert!(matches!(err, SubmitError::InsecureScheme(ref s) if s == "http"));
    }

    /// Non-loopback ws is rejected even when the fork escape hatch is
    /// on — same invariant, ws transport.
    #[tokio::test]
    async fn connect_rejects_nonloopback_ws_with_loopback_enabled() {
        let err = Submitter::connect(
            &sec("ws://example.com/rpc"),
            None,
            56,
            DEFAULT_SUBMIT_TIMEOUT,
            true,
        )
        .await
        .expect_err("non-loopback ws must be rejected even with allow_loopback");
        assert!(matches!(err, SubmitError::InsecureScheme(ref s) if s == "ws"));
    }

    /// 127.0.0.1, [::1], and `localhost` all resolve as loopback.
    #[test]
    fn is_loopback_host_recognises_loopback_variants() {
        for url in [
            "http://127.0.0.1:8545",
            "http://127.255.255.254:1234",
            "http://[::1]:8545",
            "http://localhost:8545",
            "http://LOCALHOST:8545",
            "ws://127.0.0.1:8545",
        ] {
            let parsed = url::Url::parse(url).expect("parse");
            assert!(is_loopback_host(&parsed), "expected loopback for {url}");
        }
    }

    /// Positive path: with `allow_loopback = true` and a loopback
    /// httpmock endpoint that satisfies `eth_chainId == 56`,
    /// `Submitter::connect` returns a live submitter. Combined with
    /// the negative paths above, this proves the fork escape hatch
    /// (issue #396) accepts loopback http and nothing else.
    #[tokio::test]
    async fn connect_accepts_loopback_http_under_fork_profile() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"jsonrpc":"2.0","id":0,"result":"0x38"}"#);
            })
            .await;

        let url = sec(&server.url("/"));
        let submitter = Submitter::connect(&url, None, 56, DEFAULT_SUBMIT_TIMEOUT, true)
            .await
            .expect("loopback http must be accepted under allow_loopback=true");
        assert!(submitter.endpoint().starts_with("http://"));
    }

    /// Non-loopback hosts are correctly classified.
    #[test]
    fn is_loopback_host_rejects_public_hosts() {
        for url in [
            "http://example.com",
            "http://1.2.3.4",
            "http://10.0.0.1",
            "http://[2001:db8::1]:8545",
        ] {
            let parsed = url::Url::parse(url).expect("parse");
            assert!(!is_loopback_host(&parsed), "expected NOT loopback for {url}");
        }
    }

    /// Adversarial inputs that try to smuggle "127.0.0.1" past a
    /// substring-matcher. The WHATWG URL parser canonicalises the
    /// authority before we look at it, so each of these resolves to
    /// a non-loopback host and is rejected. This pins the boundary
    /// so a future refactor that introduces a `host_str.contains(..)`
    /// shortcut would fail this test loudly.
    #[test]
    fn is_loopback_host_rejects_smuggled_loopback_strings() {
        for url in [
            "http://127.0.0.1.evil.com",
            "http://example.com#127.0.0.1",
            "http://user:pass@127.0.0.1@evil.com",
            "http://127.0.0.1@evil.com",
            "http://localhost.evil.com",
            "http://evil.com/?h=127.0.0.1",
            // IPv4-mapped IPv6: deliberately not classified as
            // loopback by std (Ipv6Addr::is_loopback).
            "http://[::ffff:127.0.0.1]",
            // INADDR_ANY is not loopback.
            "http://0.0.0.0",
        ] {
            let parsed = url::Url::parse(url).expect("parse");
            assert!(
                !is_loopback_host(&parsed),
                "smuggled-loopback URL {url} must NOT match"
            );
        }
    }

    #[test]
    fn default_timeout_is_six_seconds() {
        assert_eq!(DEFAULT_SUBMIT_TIMEOUT, Duration::from_secs(6));
    }

    // ---- Debug redaction -------------------------------------------------

    #[tokio::test]
    async fn debug_impl_does_not_expose_raw_url() {
        // Hand-construct a submitter (bypasses scheme check) so we
        // can prove Debug only reveals the scrubbed label.
        let s = Submitter {
            inner: Inner::Http(boxed_http_provider("http://127.0.0.1:1/")),
            endpoint_label: "https://private.example".to_string(),
            timeout: DEFAULT_SUBMIT_TIMEOUT,
        };
        let dbg = format!("{s:?}");
        assert!(dbg.contains("https://private.example"));
        assert!(
            !dbg.contains("127.0.0.1"),
            "raw transport URL leaked: {dbg}"
        );
    }

    // ---- httpmock-backed behaviour tests --------------------------------
    //
    // httpmock serves plain HTTP, which the public `Submitter::connect`
    // rejects by design. These tests build the provider directly so we
    // can exercise the hash-parsing and error-classification paths that
    // sit underneath `submit()`. Scheme enforcement is covered by the
    // unit tests above.

    /// Construct a `RootProvider<BoxTransport>` pointing at an
    /// http:// URL so the httpmock-backed tests can bypass the
    /// https-only check in `Submitter::connect`.
    fn boxed_http_provider(url: &str) -> RootProvider<BoxTransport> {
        let parsed: url::Url = url.parse().expect("valid http url");
        let http = Http::with_client(reqwest::Client::new(), parsed);
        let rpc_client = ClientBuilder::default().transport(http, false);
        ProviderBuilder::new().on_client(rpc_client.boxed())
    }

    fn build_test_submitter(url: &str, timeout: Duration) -> Submitter {
        Submitter {
            inner: Inner::Http(boxed_http_provider(url)),
            endpoint_label: url.to_string(),
            timeout,
        }
    }

    /// Mock returns chain id 1; probe expects 56 → typed mismatch.
    #[tokio::test]
    async fn probe_chain_id_returns_mismatch_when_endpoint_disagrees() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"jsonrpc":"2.0","id":0,"result":"0x1"}"#);
            })
            .await;

        let inner = Inner::Http(boxed_http_provider(&server.url("/")));
        let err = probe_chain_id(&inner, "http://test", 56)
            .await
            .expect_err("mismatch must surface");
        match err {
            SubmitError::ChainIdMismatch { expected, actual } => {
                assert_eq!(expected, 56);
                assert_eq!(actual, 1);
            }
            other => panic!("expected ChainIdMismatch, got {other:?}"),
        }
    }

    /// Mock returns chain id 56; probe expects 56 → ok.
    #[tokio::test]
    async fn probe_chain_id_returns_ok_when_endpoint_matches() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(r#"{"jsonrpc":"2.0","id":0,"result":"0x38"}"#);
            })
            .await;

        let inner = Inner::Http(boxed_http_provider(&server.url("/")));
        let id = probe_chain_id(&inner, "http://test", 56)
            .await
            .expect("matching chain id must pass");
        assert_eq!(id, 56);
    }

    /// Mock returns a JSON-RPC error → typed ConnectFailed.
    #[tokio::test]
    async fn probe_chain_id_maps_rpc_error_to_connect_failed() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(
                        r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"method not found"}}"#,
                    );
            })
            .await;

        let inner = Inner::Http(boxed_http_provider(&server.url("/")));
        let err = probe_chain_id(&inner, "http://test", 56)
            .await
            .expect_err("rpc-level error must surface as ConnectFailed");
        assert!(
            matches!(err, SubmitError::ConnectFailed(ref msg) if msg.contains("method not found")),
            "expected ConnectFailed mentioning the upstream message, got {err:?}"
        );
    }

    #[tokio::test]
    async fn submit_parses_hash_from_valid_rpc_response() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(
                        r#"{"jsonrpc":"2.0","id":0,"result":"0x1111111111111111111111111111111111111111111111111111111111111111"}"#,
                    );
            })
            .await;

        let submitter = build_test_submitter(&server.url("/"), DEFAULT_SUBMIT_TIMEOUT);
        let hash = submitter
            .submit(Bytes::from_static(&[0x02, 0xc0]))
            .await
            .expect("submit must parse hash");
        assert_eq!(
            format!("{hash:?}"),
            "0x1111111111111111111111111111111111111111111111111111111111111111"
        );
    }

    #[tokio::test]
    async fn submit_maps_429_to_rpc_rejected() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(429)
                    .header("content-type", "application/json")
                    .body(r#"{"error":"rate limited"}"#);
            })
            .await;

        let submitter = build_test_submitter(&server.url("/"), DEFAULT_SUBMIT_TIMEOUT);
        let err = submitter
            .submit(Bytes::from_static(&[0x02, 0xc0]))
            .await
            .expect_err("429 must bubble up");
        assert!(
            matches!(err, SubmitError::RpcRejected(_)),
            "expected RpcRejected, got {err:?}"
        );
    }

    #[tokio::test]
    async fn submit_maps_jsonrpc_error_to_rpc_rejected() {
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200)
                .header("content-type", "application/json")
                .body(
                    r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32000,"message":"nonce too low"}}"#,
                );
            })
            .await;

        let submitter = build_test_submitter(&server.url("/"), DEFAULT_SUBMIT_TIMEOUT);
        let err = submitter
            .submit(Bytes::from_static(&[0x02, 0xc0]))
            .await
            .expect_err("node error must surface");
        match err {
            SubmitError::RpcRejected(msg) => {
                assert!(msg.contains("nonce too low"), "unexpected msg: {msg}");
            }
            other => panic!("expected RpcRejected, got {other:?}"),
        }
    }

    /// `replace` rejects the case where the new tx hash matches the
    /// original — a no-op replacement (caller forgot to bump fees)
    /// must surface as a typed error rather than a silent "OK".
    #[tokio::test]
    async fn replace_rejects_matching_hash_as_no_bump() {
        let server = MockServer::start_async().await;
        let original =
            "0x1111111111111111111111111111111111111111111111111111111111111111".to_string();
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(format!(
                        r#"{{"jsonrpc":"2.0","id":0,"result":"{original}"}}"#
                    ));
            })
            .await;

        let submitter = build_test_submitter(&server.url("/"), DEFAULT_SUBMIT_TIMEOUT);
        let original_hash = original.parse::<TxHash>().expect("hash parse");
        let err = submitter
            .replace(
                Bytes::from_static(&[0x02, 0xc0]),
                original_hash,
                "bnb",
                "ttl_expired",
            )
            .await
            .expect_err("matching hash must reject");
        match err {
            SubmitError::RpcRejected(msg) => {
                assert!(
                    msg.contains("same hash"),
                    "expected matching-hash diagnostic, got: {msg}"
                );
            }
            other => panic!("expected RpcRejected, got {other:?}"),
        }
    }

    /// Happy-path replacement: the mock returns a different hash, so
    /// `replace` returns it and bumps the metric counter.
    #[tokio::test]
    async fn replace_returns_new_hash_on_distinct_response() {
        let server = MockServer::start_async().await;
        let new_hash =
            "0x2222222222222222222222222222222222222222222222222222222222222222".to_string();
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200)
                    .header("content-type", "application/json")
                    .body(format!(
                        r#"{{"jsonrpc":"2.0","id":0,"result":"{new_hash}"}}"#
                    ));
            })
            .await;

        let submitter = build_test_submitter(&server.url("/"), DEFAULT_SUBMIT_TIMEOUT);
        let original_hash = "0x1111111111111111111111111111111111111111111111111111111111111111"
            .parse::<TxHash>()
            .expect("hash parse");
        let returned = submitter
            .replace(
                Bytes::from_static(&[0x02, 0xc0]),
                original_hash,
                "bnb",
                "ttl_expired",
            )
            .await
            .expect("distinct hash must succeed");
        assert_eq!(format!("{returned:?}"), new_hash);
    }

    /// Pin the public RBF constants. Dashboards / runbooks reference
    /// these directly, so a future change should be a deliberate
    /// schema bump rather than a silent typo.
    #[test]
    fn rbf_constants_have_expected_values() {
        assert_eq!(REPLACE_AFTER_BLOCKS, 3);
        assert_eq!(MAX_REPLACEMENTS_PER_NONCE, 3);
        assert_eq!(REPLACEMENT_BUMP_PCT, 12);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn submit_times_out_on_single_attempt() {
        // Paused tokio clock lets us assert single-shot timeout
        // without a real 6 s wait.
        let server = MockServer::start_async().await;
        let _m = server
            .mock_async(|when, then| {
                when.method(POST).path("/");
                then.status(200).delay(Duration::from_secs(3600)).body("{}");
            })
            .await;

        let submitter = build_test_submitter(&server.url("/"), Duration::from_millis(50));

        let handle =
            tokio::spawn(async move { submitter.submit(Bytes::from_static(&[0x02, 0xc0])).await });

        tokio::time::advance(Duration::from_millis(100)).await;

        let err = handle
            .await
            .expect("task must not panic")
            .expect_err("must time out");
        assert!(
            matches!(err, SubmitError::Timeout(d) if d == Duration::from_millis(50)),
            "expected Timeout(50ms), got {err:?}"
        );
    }

    #[tokio::test]
    async fn submit_returns_connection_lost_when_transport_fails() {
        // Point at a bound but closed TCP port on loopback: grab a
        // listener, read its address, then drop the listener so the
        // connect attempt gets an immediate ECONNREFUSED. This
        // exercises the same code path a mid-flight reset would hit
        // (transport error, not RPC rejection), and must surface as
        // ConnectionLost so the caller rebuilds the submitter on
        // the next tick.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("http://{addr}/");

        let submitter = build_test_submitter(&url, DEFAULT_SUBMIT_TIMEOUT);
        let err = submitter
            .submit(Bytes::from_static(&[0x02, 0xc0]))
            .await
            .expect_err("closed port must fail");
        assert!(
            matches!(err, SubmitError::ConnectionLost(_)),
            "expected ConnectionLost, got {err:?}"
        );
    }
}
