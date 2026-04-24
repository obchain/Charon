//! Chainlink price cache.
//!
//! Polls `IAggregatorV3.latestRoundData()` for a configured set of
//! `(symbol → feed address)` entries and caches the result. Every read
//! carries:
//!   - round-completeness check (`answeredInRound >= roundId`),
//!   - `updatedAt > 0` uninitialized-feed check,
//!   - per-feed staleness window (heartbeat-aware).
//!
//! Storage is a `DashMap<String, CachedPrice>`, same lock-free pattern
//! as the health scanner.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, I256, U256};
use alloy::providers::RootProvider;
use alloy::pubsub::PubSubFrontend;
use alloy::sol;
use anyhow::{Context, Result};
use dashmap::DashMap;
use tracing::{debug, warn};

/// Default freshness window: 10 minutes. Used only for feeds that do not
/// have an explicit per-symbol override in the config.
pub const DEFAULT_MAX_AGE: Duration = Duration::from_secs(10 * 60);

sol! {
    /// Chainlink AggregatorV3 interface (reduced to the fields we use).
    #[sol(rpc)]
    interface IAggregatorV3 {
        function decimals() external view returns (uint8);

        /// Returns the latest round data for this feed.
        ///
        /// `answer` is int256; callers must reject negative values.
        /// `answeredInRound < roundId` means the round is still being
        /// aggregated — the returned answer is a carry-over from an
        /// older round and must not be trusted.
        function latestRoundData()
            external view returns (
                uint80 roundId,
                int256 answer,
                uint256 startedAt,
                uint256 updatedAt,
                uint80 answeredInRound
            );
    }
}

/// One cached price reading with enough metadata to judge freshness and
/// to normalize across oracle scale conventions.
#[derive(Debug, Clone)]
pub struct CachedPrice {
    /// Raw Chainlink answer in the feed's native decimals.
    pub price: U256,
    /// Number of decimals the feed reports (typically 8 on BSC).
    pub decimals: u8,
    /// Chainlink `updatedAt` unix timestamp.
    pub updated_at: u64,
    /// Wall-clock unix timestamp at which we pulled the round.
    pub fetched_at: u64,
}

impl CachedPrice {
    /// Return `price` re-scaled from its native decimals to
    /// `target_decimals`. Integer arithmetic — no f64.
    ///
    /// Callers converting to Venus oracle scale pass `target_decimals`
    /// equal to the underlying token's decimals + (36 - 18) etc. For
    /// Aave-style 18-decimal consumers, pass 18.
    pub fn scaled_to(&self, target_decimals: u8) -> U256 {
        use std::cmp::Ordering;
        match target_decimals.cmp(&self.decimals) {
            Ordering::Equal => self.price,
            Ordering::Greater => {
                let diff = target_decimals - self.decimals;
                self.price * U256::from(10u64).pow(U256::from(diff))
            }
            Ordering::Less => {
                let diff = self.decimals - target_decimals;
                self.price / U256::from(10u64).pow(U256::from(diff))
            }
        }
    }
}

/// Thin wrapper around a provider + a per-symbol feed map + per-feed
/// staleness overrides + a concurrent cache.
pub struct PriceCache {
    provider: Arc<RootProvider<PubSubFrontend>>,
    feeds: HashMap<String, Address>,
    default_max_age: Duration,
    per_symbol_max_age: HashMap<String, Duration>,
    cache: DashMap<String, CachedPrice>,
}

impl PriceCache {
    /// Build a cache with a default staleness window. Equivalent to
    /// `with_per_symbol_max_age(provider, feeds, default_max_age, {})`.
    pub fn new(
        provider: Arc<RootProvider<PubSubFrontend>>,
        feeds: HashMap<String, Address>,
        default_max_age: Duration,
    ) -> Self {
        Self::with_per_symbol_max_age(provider, feeds, default_max_age, HashMap::new())
    }

    /// Build a cache with per-symbol staleness overrides (e.g. stablecoin
    /// feeds accept 24h, volatile pairs 2min).
    pub fn with_per_symbol_max_age(
        provider: Arc<RootProvider<PubSubFrontend>>,
        feeds: HashMap<String, Address>,
        default_max_age: Duration,
        per_symbol_max_age: HashMap<String, Duration>,
    ) -> Self {
        Self {
            provider,
            feeds,
            default_max_age,
            per_symbol_max_age,
            cache: DashMap::new(),
        }
    }

    pub fn symbols(&self) -> impl Iterator<Item = &str> {
        self.feeds.keys().map(String::as_str)
    }

    fn max_age_for(&self, symbol: &str) -> Duration {
        self.per_symbol_max_age
            .get(symbol)
            .copied()
            .unwrap_or(self.default_max_age)
    }

    /// Fetch one feed by symbol, validate round completeness and
    /// freshness, insert into the cache, return the parsed reading.
    pub async fn refresh(&self, symbol: &str) -> Result<CachedPrice> {
        let feed = self
            .feeds
            .get(symbol)
            .with_context(|| format!("no Chainlink feed configured for '{symbol}'"))?;

        let agg = IAggregatorV3::new(*feed, self.provider.clone());
        let decimals = agg
            .decimals()
            .call()
            .await
            .with_context(|| format!("feed '{symbol}' ({feed}): decimals() failed"))?
            ._0;
        let round = agg
            .latestRoundData()
            .call()
            .await
            .with_context(|| format!("feed '{symbol}' ({feed}): latestRoundData() failed"))?;

        // Reject negative answers: most aggregators emit negative on
        // degraded state; coercing to U256 would produce a huge number.
        let raw_answer = round.answer;
        let price = if raw_answer < I256::ZERO {
            anyhow::bail!("feed '{symbol}' returned negative answer: {raw_answer}");
        } else {
            U256::try_from(raw_answer)
                .with_context(|| format!("feed '{symbol}': answer {raw_answer} → U256"))?
        };

        // Round-completeness: answeredInRound < roundId ⇒ current round
        // has not finished aggregating. The returned answer is the
        // previous round's value; reject to avoid stale pricing passed
        // off as fresh updatedAt.
        if round.answeredInRound < round.roundId {
            anyhow::bail!(
                "feed '{symbol}': round not complete (answeredInRound={}, roundId={})",
                round.answeredInRound,
                round.roundId
            );
        }

        let updated_at: u64 = round.updatedAt.try_into().with_context(|| {
            format!(
                "feed '{symbol}': updatedAt {:?} does not fit in u64",
                round.updatedAt
            )
        })?;
        if updated_at == 0 {
            anyhow::bail!("feed '{symbol}': updatedAt=0, aggregator is uninitialized");
        }

        let now = unix_now().context("system clock unavailable, cannot judge freshness")?;
        let max_age = self.max_age_for(symbol);
        let age = now.saturating_sub(updated_at);
        if age > max_age.as_secs() {
            anyhow::bail!(
                "feed '{symbol}' is stale (updated {} s ago, max_age {} s)",
                age,
                max_age.as_secs()
            );
        }

        let cached = CachedPrice {
            price,
            decimals,
            updated_at,
            fetched_at: now,
        };
        debug!(
            symbol,
            price = %cached.price,
            decimals,
            age_secs = age,
            "chainlink price refreshed"
        );
        self.cache.insert(symbol.to_string(), cached.clone());
        Ok(cached)
    }

    /// Refresh every configured feed. Individual failures are logged
    /// and do not abort the batch.
    pub async fn refresh_all(&self) {
        for symbol in self.feeds.keys() {
            if let Err(err) = self.refresh(symbol).await {
                warn!(symbol = %symbol, ?err, "chainlink refresh failed");
            }
        }
    }

    /// Return the most recently cached price iff it is still fresh.
    /// Stale entries, entries from an uninitialized feed, or an
    /// unusable system clock all yield `None`.
    pub fn get(&self, symbol: &str) -> Option<CachedPrice> {
        let entry = self.cache.get(symbol)?;
        if self.is_fresh(symbol, &entry) {
            Some(entry.clone())
        } else {
            None
        }
    }

    /// Freshness predicate. Returns `false` if the system clock fails;
    /// better to treat stale as stale than to serve old prices to the
    /// liquidation path.
    pub fn is_fresh(&self, symbol: &str, cached: &CachedPrice) -> bool {
        match unix_now() {
            Ok(now) => {
                let max_age = self.max_age_for(symbol);
                cached.updated_at + max_age.as_secs() >= now
            }
            Err(_) => false,
        }
    }
}

/// Unix seconds since epoch. Errors on clock skew / pre-epoch so callers
/// can treat the failure distinctly (e.g. treat all cache entries as
/// stale rather than silently serve any entry because `now = 0`).
fn unix_now() -> Result<u64> {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?;
    Ok(d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn feeds_map() -> HashMap<String, Address> {
        let mut m = HashMap::new();
        m.insert(
            "BNB".to_string(),
            address!("0567F2323251f0Aab15c8dFb1967E4e8A7D42aeE"),
        );
        m
    }

    #[test]
    fn scaled_to_up_and_down() {
        // 300.00000000 in 8 decimals.
        let p = CachedPrice {
            price: U256::from(300_00000000u64),
            decimals: 8,
            updated_at: 1,
            fetched_at: 1,
        };
        // 300 * 1e18 when scaled to 18 decimals.
        assert_eq!(
            p.scaled_to(18),
            U256::from(300u64) * U256::from(10u64).pow(U256::from(18u64))
        );
        // 300 (no decimals) when scaled down.
        assert_eq!(p.scaled_to(0), U256::from(300u64));
    }

    #[test]
    fn feeds_map_is_iterable() {
        let m = feeds_map();
        assert!(m.contains_key("BNB"));
    }
}
