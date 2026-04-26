#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Live Chainlink feed smoke test on BSC.
//!
//! Skipped without `BNB_WS_URL`. Verifies `PriceCache::refresh` speaks
//! to a real aggregator, rejects stale/negative readings, and caches
//! the result for subsequent `get` calls.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, U256};
use alloy::providers::{ProviderBuilder, WsConnect};
use charon_scanner::{DEFAULT_MAX_AGE, PriceCache};

const BNB_USD_FEED: &str = "0x0567F2323251f0Aab15c8dFb1967E4e8A7D42aeE";

#[tokio::test]
async fn refresh_pulls_live_bnb_usd_price() {
    let _ = dotenvy::dotenv();
    let Ok(ws_url) = std::env::var("BNB_WS_URL") else {
        eprintln!("skipping: BNB_WS_URL not set");
        return;
    };

    let provider = ProviderBuilder::new()
        .on_ws(WsConnect::new(ws_url))
        .await
        .expect("ws connect");

    let mut feeds = HashMap::new();
    feeds.insert("BNB".to_string(), Address::from_str(BNB_USD_FEED).unwrap());

    let cache = PriceCache::new(Arc::new(provider), feeds, DEFAULT_MAX_AGE);

    let price = cache.refresh("BNB").await.expect("refresh BNB");
    assert!(price.price > U256::ZERO, "BNB price should be positive");
    assert!(price.decimals >= 6, "Chainlink decimals are typically 8");
    assert!(cache.is_fresh("BNB", &price));

    let cached = cache.get("BNB").expect("cached after refresh");
    assert_eq!(cached.price, price.price);
}

#[tokio::test]
async fn stale_rejection_triggers_when_max_age_is_zero() {
    let _ = dotenvy::dotenv();
    let Ok(ws_url) = std::env::var("BNB_WS_URL") else {
        eprintln!("skipping: BNB_WS_URL not set");
        return;
    };

    let provider = ProviderBuilder::new()
        .on_ws(WsConnect::new(ws_url))
        .await
        .expect("ws connect");

    let mut feeds = HashMap::new();
    feeds.insert("BNB".to_string(), Address::from_str(BNB_USD_FEED).unwrap());

    // max_age = 0 forces every feed to look stale.
    let cache = PriceCache::new(Arc::new(provider), feeds, Duration::from_secs(0));
    let err = cache
        .refresh("BNB")
        .await
        .expect_err("should reject as stale");
    assert!(format!("{err:#}").contains("stale"));
}
