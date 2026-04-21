//! Live Aave V3 flash-loan adapter smoke test on BSC.
//!
//! Skipped without `BNB_WS_URL`. Exercises the full adapter wiring:
//! pool handshake, premium read, data-provider lookup, aToken balance.

use std::str::FromStr;
use std::sync::Arc;

use alloy::primitives::{Address, U256};
use alloy::providers::{ProviderBuilder, WsConnect};
use charon_core::{FlashLoanProvider, FlashLoanSource};
use charon_flashloan::AaveFlashLoan;

const AAVE_V3_BSC_POOL: &str = "0x6807dc923806fe8fd134338eabca509979a7e0cb";
/// Burn address used as a stand-in receiver — live calldata emission
/// isn't checked here, only read-side behaviour.
const DUMMY_RECEIVER: &str = "0x000000000000000000000000000000000000dEaD";
/// USDT on BSC — Venus's primary debt asset, known to be an Aave reserve.
const BSC_USDT: &str = "0x55d398326f99059fF775485246999027B3197955";

#[tokio::test]
async fn connects_and_quotes_bsc_usdt() {
    let _ = dotenvy::dotenv();
    let Ok(ws_url) = std::env::var("BNB_WS_URL") else {
        eprintln!("skipping: BNB_WS_URL not set");
        return;
    };

    let provider = ProviderBuilder::new()
        .on_ws(WsConnect::new(ws_url))
        .await
        .expect("ws connect");

    let adapter = AaveFlashLoan::connect(
        Arc::new(provider),
        Address::from_str(AAVE_V3_BSC_POOL).unwrap(),
        Address::from_str(DUMMY_RECEIVER).unwrap(),
    )
    .await
    .expect("aave connect");

    assert_eq!(adapter.source(), FlashLoanSource::AaveV3);
    assert_eq!(adapter.chain_id(), 56);
    assert!(
        adapter.fee_rate_bps() > 0,
        "Aave V3 flash premium expected > 0"
    );

    let usdt = Address::from_str(BSC_USDT).unwrap();
    let liquidity = adapter
        .available_liquidity(usdt)
        .await
        .expect("available_liquidity USDT");
    assert!(liquidity > U256::ZERO, "BSC USDT aToken should hold > 0");

    // 10 USDT (18 decimals) — well within typical Aave BSC liquidity.
    let amount = U256::from(10u64) * U256::from(10u64).pow(U256::from(18u64));
    let quote = adapter
        .quote(usdt, amount)
        .await
        .expect("quote USDT")
        .expect("quote should be Some for small amount");

    assert_eq!(quote.source, FlashLoanSource::AaveV3);
    assert_eq!(quote.token, usdt);
    assert_eq!(quote.amount, amount);
    assert_eq!(quote.fee_bps, adapter.fee_rate_bps());
}
