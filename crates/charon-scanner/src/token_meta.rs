//! Cached `(symbol, decimals)` for every ERC-20 the bot cares about.
//!
//! The profit gate needs to convert a raw `repay_amount` (in token
//! units) into USD cents, which means knowing two things per token:
//!
//! 1. How many decimals the ERC-20 uses (`USDT` = 6 on BSC; `BTCB` = 18).
//! 2. Which Chainlink feed to look up in [`crate::PriceCache`] — that
//!    cache is keyed by symbol string, not address.
//!
//! Both are static after deployment, so we query each underlying once
//! at startup and stash the result. Transient RPC failures (HTTP 429,
//! rate-limit JSON-RPC errors, transient archive misses) retry with
//! exponential backoff before giving up — see `fetch_with_retry`.
//! Genuinely missing tokens (legacy MKR-style bytes32 symbols, calls
//! that fail after every retry) are skipped (logged at warn) so the
//! profit gate sees them as unknown and drops the opportunity rather
//! than crashing the bot.

use std::collections::HashMap;
use std::time::Duration;

use alloy::primitives::Address;
use alloy::providers::RootProvider;
use alloy::pubsub::PubSubFrontend;
use alloy::sol;
use tokio::time::sleep;
use tracing::{debug, warn};

sol! {
    /// ERC-20 metadata-only surface: `symbol()` + `decimals()`.
    #[sol(rpc)]
    interface IERC20Meta {
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
    }
}

/// Maximum total attempts (1 initial + 4 retries) before giving up
/// on a single metadata call. Caps `fetch_with_retry`'s backoff
/// schedule at 0.5s + 1s + 2s + 4s = 7.5s of additive sleep in the
/// worst case per token (plus the 5 in-flight RPC round-trips,
/// which on a 429 storm return near-immediately). `build` queries
/// tokens sequentially, so a hypothetical 48-market boot where
/// every single market hits the cap takes ~6 min of sleep — a real
/// 429 storm rarely affects 100% of markets simultaneously, so
/// practical worst case is much smaller. Successful first-try
/// calls (the dominant case) pay zero retry cost.
const META_MAX_ATTEMPTS: usize = 5;

// Compile-time guard: the post-loop `unreachable!()` in
// `fetch_with_retry` is sound only if the loop body executes at
// least once. A zero attempt cap would skip the loop and hit the
// unreachable, which would be a bug, not a panic-by-design.
const _: () = assert!(META_MAX_ATTEMPTS >= 1, "META_MAX_ATTEMPTS must be >= 1");

/// Initial backoff before the first retry. Doubles every failed
/// attempt up to `META_MAX_ATTEMPTS`.
const META_INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// Metadata for one ERC-20: what to call it and how to scale it.
#[derive(Debug, Clone)]
pub struct TokenMeta {
    pub symbol: String,
    pub decimals: u8,
}

/// Address-keyed cache populated once at startup from the list of
/// underlying tokens the adapter discovered.
#[derive(Debug, Default)]
pub struct TokenMetaCache {
    inner: HashMap<Address, TokenMeta>,
}

/// Heuristic: does this RPC error look like a transient
/// rate-limit / overload / connection blip that retrying with
/// backoff is likely to recover from? Matches the failure modes
/// catalogued in #330 plus the common upstream LB / pubsub
/// transport drops we see on free-tier dRPC for BSC archive:
///
/// - HTTP 429 ("Too Many Requests").
/// - HTTP 502 / 503 / 504 from upstream load balancers when a
///   backend node is overloaded or briefly out of rotation.
/// - JSON-RPC `code: -32603` (internal error, used by upstream
///   load balancers when a backend node is throttling).
/// - JSON-RPC `code: -32005` (Alchemy/Infura/most-mirrors
///   "limit exceeded" code).
/// - dRPC's `code: 35` "Too many request" (their own envelope).
/// - `code: -32000 missing trie node` from a non-archive fallback
///   that hasn't yet caught up — recoverable on the next attempt.
/// - WS / IPC transport errors: `connection reset`, `connection
///   closed`, `request timed out`, `timed out` — the pubsub link
///   drops under load and reconnects, mid-flight calls fail.
/// - Anything mentioning "rate limit", "throttl", or "compute
///   units" textually (compute-unit budgets are dRPC's free-tier
///   throttle surface).
///
/// We match against `format!("{:?}", err)` because alloy's
/// `RpcError` doesn't expose a stable status-code surface across
/// transports (HTTP vs. WS vs. IPC) and the JSON-RPC numeric
/// codes (`-32603`, dRPC `35`) aren't in alloy's typed surface
/// — they're upstream-defined. We'd rather over-retry a
/// permanent error than mis-classify a transient one as terminal:
/// a permanent error will exhaust `META_MAX_ATTEMPTS` and still
/// surface, just with a small delay.
fn is_transient(err: &(impl std::fmt::Debug + ?Sized)) -> bool {
    let s = format!("{err:?}");
    let lower = s.to_ascii_lowercase();
    s.contains("429")
        || s.contains(" 502")
        || s.contains(" 503")
        || s.contains(" 504")
        || s.contains("code: 35")
        || s.contains("Too many request")
        || s.contains("-32603")
        || s.contains("-32005")
        || s.contains("missing trie node")
        || lower.contains("rate limit")
        || lower.contains("throttl")
        || lower.contains("compute units")
        || lower.contains("timed out")
        || lower.contains("connection reset")
        || lower.contains("connection closed")
}

/// Run an async fallible call with bounded exponential backoff on
/// transient errors. Returns `Ok` on first success; returns the
/// final `Err` once retries are exhausted or the error is classified
/// as permanent. `op_name` and `token` are only used for the warn
/// log on retry, so the operator can correlate dropped markets to
/// upstream blips.
async fn fetch_with_retry<F, Fut, T, E>(op_name: &str, token: Address, mut op: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Debug,
{
    let mut backoff = META_INITIAL_BACKOFF;

    for attempt in 1..=META_MAX_ATTEMPTS {
        match op().await {
            Ok(v) => return Ok(v),
            Err(err) => {
                if !is_transient(&err) || attempt == META_MAX_ATTEMPTS {
                    return Err(err);
                }
                warn!(
                    op = op_name,
                    token = %token,
                    attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    error = ?err,
                    "transient RPC failure on token meta — retrying with backoff",
                );
                sleep(backoff).await;
                backoff = backoff.saturating_mul(2);
            }
        }
    }

    // Unreachable: the loop iterates `META_MAX_ATTEMPTS` times
    // (compile-time asserted >= 1 above), and the final iteration
    // always returns — either `Ok(v)` or `Err(err)` via the
    // `attempt == META_MAX_ATTEMPTS` branch. If this fires, the
    // const guard was bypassed.
    unreachable!("fetch_with_retry: loop must return on final attempt")
}

impl TokenMetaCache {
    /// Query `symbol()` and `decimals()` for every address in `tokens`
    /// and return a populated cache. Each call is wrapped in
    /// `fetch_with_retry` so a single transient 429 / rate-limit
    /// blip no longer permanently blacklists a market for the
    /// process lifetime (#330). Tokens that still fail after retries
    /// — e.g. legacy MKR-style bytes32 symbols, or a sustained RPC
    /// outage — are dropped from the cache; callers see them as
    /// unknown and skip the opportunity.
    pub async fn build(
        provider: &RootProvider<PubSubFrontend>,
        tokens: impl IntoIterator<Item = Address>,
    ) -> Self {
        let mut inner = HashMap::new();
        for addr in tokens {
            let contract = IERC20Meta::new(addr, provider);

            let symbol =
                match fetch_with_retry("symbol", addr, || async { contract.symbol().call().await })
                    .await
                {
                    Ok(r) => r._0,
                    Err(err) => {
                        // Genuinely missing or non-standard ERC-20 — skip.
                        // Logged at warn (not error) because the bot
                        // continues running; the profit gate treats
                        // "no meta" the same as "no price".
                        warn!(
                            token = %addr,
                            error = ?err,
                            "symbol() failed after retries — market skipped from token meta",
                        );
                        continue;
                    }
                };

            let decimals = match fetch_with_retry("decimals", addr, || async {
                contract.decimals().call().await
            })
            .await
            {
                Ok(r) => r._0,
                Err(err) => {
                    warn!(
                        token = %addr,
                        error = ?err,
                        "decimals() failed after retries — market skipped from token meta",
                    );
                    continue;
                }
            };

            debug!(token = %addr, %symbol, decimals, "token meta cached");
            inner.insert(addr, TokenMeta { symbol, decimals });
        }
        Self { inner }
    }

    /// Look up meta by underlying address. `None` if the token was
    /// never queried or its metadata calls failed at startup.
    pub fn get(&self, addr: &Address) -> Option<&TokenMeta> {
        self.inner.get(addr)
    }

    /// Count of successfully cached tokens.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// `true` when no tokens cached — useful for startup sanity checks.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cache_returns_none_on_lookup() {
        let c = TokenMetaCache::default();
        assert!(c.is_empty());
        assert_eq!(c.len(), 0);
        assert!(c.get(&Address::ZERO).is_none());
    }

    #[test]
    fn populated_cache_reports_len_and_hit() {
        let mut c = TokenMetaCache::default();
        let addr = Address::from([0x11; 20]);
        c.inner.insert(
            addr,
            TokenMeta {
                symbol: "USDT".into(),
                decimals: 18,
            },
        );
        assert_eq!(c.len(), 1);
        assert!(!c.is_empty());
        let meta = c.get(&addr).expect("hit");
        assert_eq!(meta.symbol, "USDT");
        assert_eq!(meta.decimals, 18);
    }

    #[test]
    fn is_transient_classifies_known_rate_limits() {
        // dRPC envelope.
        assert!(is_transient(&"server error: code: 35 Too many request"));
        // HTTP 429 surfaced through alloy.
        assert!(is_transient(&"http error: status 429 Too Many Requests"));
        // Upstream LB 5xx (overload / brief out-of-rotation).
        assert!(is_transient(&"upstream error: status 502 Bad Gateway"));
        assert!(is_transient(
            &"upstream error: status 503 Service Unavailable"
        ));
        assert!(is_transient(&"upstream error: status 504 Gateway Timeout"));
        // Generic JSON-RPC throttle codes.
        assert!(is_transient(&"rpc error: code: -32603 internal error"));
        assert!(is_transient(&"rpc error: code: -32005 limit exceeded"));
        // Archive miss on fallback node.
        assert!(is_transient(&"missing trie node 0xabc..."));
        // Plain-text rate-limit phrasing.
        assert!(is_transient(&"rate limit exceeded"));
        assert!(is_transient(&"upstream is throttling"));
        // dRPC compute-unit budget exhaustion.
        assert!(is_transient(&"daily compute units exceeded for plan"));
        // Pubsub / WS transport drops.
        assert!(is_transient(&"request timed out after 5s"));
        assert!(is_transient(&"websocket connection reset by peer"));
        assert!(is_transient(&"connection closed before response"));
    }

    #[test]
    fn is_transient_rejects_permanent_errors() {
        // Non-existent contract / wrong ABI — not retryable.
        assert!(!is_transient(&"execution reverted: 0x"));
        assert!(!is_transient(&"deserialization error: bytes too short"));
        assert!(!is_transient(&"invalid address"));
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_with_retry_returns_first_ok() {
        let mut calls = 0_usize;
        let result: Result<u8, &'static str> = fetch_with_retry("test", Address::ZERO, || {
            calls += 1;
            async move { Ok::<u8, &'static str>(42) }
        })
        .await;
        assert_eq!(result, Ok(42));
        assert_eq!(calls, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_with_retry_recovers_after_transient_429() {
        let mut calls = 0_usize;
        let result: Result<u8, String> = fetch_with_retry("test", Address::ZERO, || {
            calls += 1;
            let attempt = calls;
            async move {
                if attempt < 3 {
                    Err(format!(
                        "http error: status 429 Too Many Requests (attempt {attempt})"
                    ))
                } else {
                    Ok(7)
                }
            }
        })
        .await;
        assert_eq!(result, Ok(7));
        assert_eq!(calls, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_with_retry_returns_permanent_error_immediately() {
        let mut calls = 0_usize;
        let result: Result<u8, &'static str> = fetch_with_retry("test", Address::ZERO, || {
            calls += 1;
            async move { Err::<u8, &'static str>("execution reverted: 0x") }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls, 1, "permanent errors must not retry");
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_with_retry_handles_mixed_transient_classes() {
        // Reproduces the realistic dRPC blip pattern: a 429, then
        // an upstream 503, then success. Exercises that doubling
        // backoff applies across heterogeneous transient errors.
        let mut calls = 0_usize;
        let result: Result<u8, String> = fetch_with_retry("test", Address::ZERO, || {
            calls += 1;
            let attempt = calls;
            async move {
                match attempt {
                    1 => Err("http error: status 429 Too Many Requests".to_string()),
                    2 => Err("upstream error: status 503 Service Unavailable".to_string()),
                    _ => Ok(99),
                }
            }
        })
        .await;
        assert_eq!(result, Ok(99));
        assert_eq!(calls, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_with_retry_stops_at_permanent_after_transient() {
        // Transient first, then a permanent revert — the permanent
        // must terminate the loop immediately (no further retries).
        let mut calls = 0_usize;
        let result: Result<u8, String> = fetch_with_retry("test", Address::ZERO, || {
            calls += 1;
            let attempt = calls;
            async move {
                if attempt == 1 {
                    Err("http error: status 429 Too Many Requests".to_string())
                } else {
                    Err("execution reverted: 0x".to_string())
                }
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            calls, 2,
            "permanent error must short-circuit further retries"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn fetch_with_retry_gives_up_after_max_attempts() {
        let mut calls = 0_usize;
        let result: Result<u8, &'static str> = fetch_with_retry("test", Address::ZERO, || {
            calls += 1;
            async move { Err::<u8, &'static str>("status 429 Too Many Requests") }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls, META_MAX_ATTEMPTS);
    }
}
