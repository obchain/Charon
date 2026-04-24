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
//! at startup and stash the result. A missing or failing token is
//! skipped (logged at warn) rather than panicking — the profit gate
//! treats "no meta" the same as "no price" and drops the opportunity.

use std::collections::HashMap;

use alloy::primitives::Address;
use alloy::providers::RootProvider;
use alloy::pubsub::PubSubFrontend;
use alloy::sol;
use tracing::{debug, error};

sol! {
    /// ERC-20 metadata-only surface: `symbol()` + `decimals()`.
    #[sol(rpc)]
    interface IERC20Meta {
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
    }
}

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

impl TokenMetaCache {
    /// Query `symbol()` and `decimals()` for every address in `tokens`
    /// and return a populated cache. Tokens whose calls fail or whose
    /// `symbol()` returns something unprintable are dropped from the
    /// cache; callers see them as unknown and skip the opportunity.
    pub async fn build(
        provider: &RootProvider<PubSubFrontend>,
        tokens: impl IntoIterator<Item = Address>,
    ) -> Self {
        let mut inner = HashMap::new();
        for addr in tokens {
            let contract = IERC20Meta::new(addr, provider);
            let symbol = match contract.symbol().call().await {
                Ok(r) => r._0,
                Err(err) => {
                    // Legacy tokens (MKR-style bytes32 symbol, non-standard
                    // ERC-20s) and RPC failures both land here. Either way
                    // the profit gate cannot price this market — log loud
                    // so the operator notices, and skip.
                    error!(
                        token = %addr,
                        error = ?err,
                        "symbol() failed — market is now UNREACHABLE by the profit gate",
                    );
                    continue;
                }
            };
            let decimals = match contract.decimals().call().await {
                Ok(r) => r._0,
                Err(err) => {
                    error!(
                        token = %addr,
                        error = ?err,
                        "decimals() failed — market is now UNREACHABLE by the profit gate",
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
}
