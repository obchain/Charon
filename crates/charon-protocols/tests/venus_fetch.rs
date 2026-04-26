#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Live `fetch_positions` smoke test against BSC.
//!
//! Skipped without `BNB_WS_URL`. Verifies the pipeline — Comptroller,
//! oracle, per-vToken reads — survives real on-chain state without
//! panicking and returns well-formed `Position` structs (or an empty
//! vec for addresses with no Venus activity).

use std::str::FromStr;
use std::sync::Arc;

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::Address;
use alloy::providers::{ProviderBuilder, WsConnect};
use charon_core::{LendingProtocol, ProtocolId};
use charon_protocols::VenusAdapter;

const VENUS_COMPTROLLER_BSC: &str = "0xfD36E2c2a6789Db23113685031d7F16329158384";

/// An address with no Venus interaction — should yield an empty result.
const EMPTY_ADDRESS: &str = "0x000000000000000000000000000000000000dEaD";

#[tokio::test]
async fn fetch_positions_returns_ok_for_empty_address() {
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

    let empty = Address::from_str(EMPTY_ADDRESS).unwrap();
    let positions = adapter
        .fetch_positions(&[empty], BlockNumberOrTag::Latest)
        .await
        .expect("fetch_positions should not error on a clean address");

    // Valid outcomes: no positions at all, or a Position whose fields
    // reflect whatever state the address has. Either way, no panic.
    for p in &positions {
        assert_eq!(p.protocol, ProtocolId::Venus);
        assert_eq!(p.chain_id, 56);
        assert_eq!(p.borrower, empty);
    }
}
