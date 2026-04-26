#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Live connectivity smoke test for `VenusAdapter::connect`.
//!
//! Skipped unless `BNB_WS_URL` is set — CI / offline runs see no failure,
//! local dev gets a real BSC handshake. Kept intentionally thin; richer
//! integration tests against borrower snapshots land with the
//! `LendingProtocol` impl.

use std::str::FromStr;
use std::sync::Arc;

use alloy::primitives::{Address, U256};
use alloy::providers::{ProviderBuilder, WsConnect};
use charon_protocols::VenusAdapter;

/// Venus Unitroller on BSC mainnet.
const VENUS_COMPTROLLER_BSC: &str = "0xfD36E2c2a6789Db23113685031d7F16329158384";

#[tokio::test]
async fn connect_against_bsc_snapshots_markets() {
    let _ = dotenvy::dotenv();
    let Ok(ws_url) = std::env::var("BNB_WS_URL") else {
        eprintln!("skipping: BNB_WS_URL not set");
        return;
    };

    let provider = ProviderBuilder::new()
        .on_ws(WsConnect::new(ws_url))
        .await
        .expect("ws connect");
    let comptroller = Address::from_str(VENUS_COMPTROLLER_BSC).unwrap();

    let adapter = VenusAdapter::connect(Arc::new(provider), comptroller)
        .await
        .expect("venus connect");

    assert!(
        !adapter.markets().await.is_empty(),
        "Venus Comptroller should expose at least one vToken market"
    );
    assert_ne!(
        adapter.oracle().await,
        Address::ZERO,
        "Venus oracle address should be non-zero"
    );
    assert!(
        adapter.close_factor_mantissa().await > U256::ZERO,
        "close factor should be non-zero"
    );
}
