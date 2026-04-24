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
use alloy::transports::{BoxTransport, RpcError, TransportError};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use secrecy::{ExposeSecret, SecretString};
use tracing::{info, warn};

/// Default submission timeout (6 s ≈ 2 BSC blocks).
pub const DEFAULT_SUBMIT_TIMEOUT: Duration = Duration::from_secs(6);

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
    /// - `timeout` bounds a single submission attempt.
    ///
    /// The URL stays inside the `SecretString` the caller owns; this
    /// function derives a host-only label for logging and calls
    /// `expose_secret()` exactly once, at transport construction.
    pub async fn connect(
        url: &SecretString,
        auth: Option<&SecretString>,
        timeout: Duration,
    ) -> Result<Self, SubmitError> {
        let raw = url.expose_secret();
        let parsed = url::Url::parse(raw)
            .map_err(|e| SubmitError::InvalidEndpoint(format!("invalid URL: {e}")))?;

        let scheme = parsed.scheme();
        if scheme != "https" && scheme != "wss" {
            return Err(SubmitError::InsecureScheme(scheme.to_string()));
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
            "https" => {
                let client = build_reqwest_client(auth)?;
                let http = Http::with_client(client, parsed);
                let is_local = false;
                let rpc_client = ClientBuilder::default().transport(http, is_local);
                let provider: RootProvider<BoxTransport> =
                    ProviderBuilder::new().on_client(rpc_client.boxed());
                Inner::Http(provider)
            }
            "wss" => {
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

        info!(
            endpoint = %endpoint_label,
            timeout_secs = timeout.as_secs(),
            auth = auth.is_some(),
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

    /// Submit raw signed transaction bytes. Single attempt, no retry.
    ///
    /// The submitter is deliberately single-shot. Whether a timed-out
    /// broadcast is still worth re-sending (same price, same health
    /// factor, same gas ceiling) is a pipeline-level decision owned
    /// by the caller along with the opportunity queue TTL.
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
        RpcError::Transport(ref kind) => {
            let msg = kind.to_string();
            // alloy renders HTTP status errors as "HTTP error <code>...".
            // 4xx (including 429) is a deterministic rejection; 5xx and
            // everything else (TCP reset, TLS, DNS) means the caller
            // should reconnect.
            let is_4xx = msg.contains("429")
                || msg.contains("400")
                || msg.contains("401")
                || msg.contains("403")
                || msg.contains("404")
                || msg.contains("408")
                || msg.contains("413")
                || msg.contains("415")
                || msg.contains("422");
            if is_4xx {
                SubmitError::RpcRejected(msg)
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
        let err = Submitter::connect(&sec("http://example.com/rpc"), None, DEFAULT_SUBMIT_TIMEOUT)
            .await
            .expect_err("http:// must be rejected");
        assert!(matches!(err, SubmitError::InsecureScheme(ref s) if s == "http"));
    }

    #[tokio::test]
    async fn connect_rejects_plain_ws_scheme() {
        let err = Submitter::connect(&sec("ws://example.com/rpc"), None, DEFAULT_SUBMIT_TIMEOUT)
            .await
            .expect_err("ws:// must be rejected");
        assert!(matches!(err, SubmitError::InsecureScheme(ref s) if s == "ws"));
    }

    #[tokio::test]
    async fn connect_rejects_exotic_scheme() {
        let err = Submitter::connect(&sec("ftp://example.com/"), None, DEFAULT_SUBMIT_TIMEOUT)
            .await
            .expect_err("ftp must be rejected");
        assert!(matches!(err, SubmitError::InsecureScheme(ref s) if s == "ftp"));
    }

    #[tokio::test]
    async fn connect_rejects_missing_scheme() {
        let err = Submitter::connect(&sec("example.com/rpc"), None, DEFAULT_SUBMIT_TIMEOUT)
            .await
            .expect_err("missing scheme must be rejected");
        // url::Url::parse either fails outright (InvalidEndpoint) or
        // returns a non-http(s) scheme (InsecureScheme). Both are safe.
        assert!(matches!(
            err,
            SubmitError::InvalidEndpoint(_) | SubmitError::InsecureScheme(_)
        ));
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
