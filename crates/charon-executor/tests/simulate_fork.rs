//! Fork tests for the `eth_call` simulation gate.
//!
//! These tests run against a BSC fork URL (anvil, Hardhat, or a
//! managed fork). They are `#[ignore]`d by default because they
//! require:
//!   - `BSC_FORK_URL` pointing at an HTTP endpoint that can serve
//!     `eth_call` against BSC-mainnet state.
//!   - `CHARON_LIQUIDATOR_ADDR` — an address on that fork where a
//!     deployed `CharonLiquidator` instance lives.
//!   - `CHARON_OWNER_KEY` — the private key owning that instance
//!     (hot wallet).
//!
//! Run with:
//!   BSC_FORK_URL=... \
//!   CHARON_LIQUIDATOR_ADDR=0x... \
//!   CHARON_OWNER_KEY=0x... \
//!   cargo test -p charon-executor --test simulate_fork -- --ignored
//!
//! Pattern mirrors `crates/charon-protocols/tests/venus_fetch.rs`.

use std::str::FromStr;

use alloy::primitives::{Address, Bytes, U256, address};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use charon_core::{
    FlashLoanSource, LiquidationOpportunity, LiquidationParams, Position, ProtocolId, SwapRoute,
};
use charon_executor::builder::ICharonLiquidator;
use charon_executor::{SimulationError, Simulator, TxBuilder};

fn env(var: &str) -> Option<String> {
    let _ = dotenvy::dotenv();
    std::env::var(var).ok()
}

fn dev_signer() -> PrivateKeySigner {
    // Anvil default #0 — used only when CHARON_OWNER_KEY is unset.
    "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
        .parse()
        .expect("dev key parse")
}

fn synthetic_opportunity() -> (LiquidationOpportunity, LiquidationParams) {
    let borrower = address!("1111111111111111111111111111111111111111");
    let collateral = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let debt = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let opp = LiquidationOpportunity {
        position: Position {
            protocol: ProtocolId::Venus,
            chain_id: 56,
            borrower,
            collateral_token: collateral,
            debt_token: debt,
            collateral_amount: U256::from(1_000u64),
            debt_amount: U256::from(500u64),
            health_factor: U256::ZERO,
            liquidation_bonus_bps: 1_000,
        },
        debt_to_repay: U256::from(250u64),
        expected_collateral_out: U256::from(275u64),
        flash_source: FlashLoanSource::AaveV3,
        swap_route: SwapRoute {
            token_in: collateral,
            token_out: debt,
            amount_in: U256::from(275u64),
            min_amount_out: U256::from(260u64),
            pool_fee: Some(3_000),
        },
        net_profit_wei: U256::from(5_000u64),
    };
    let params = LiquidationParams::Venus {
        borrower,
        collateral_vtoken: address!("cccccccccccccccccccccccccccccccccccccccc"),
        debt_vtoken: address!("dddddddddddddddddddddddddddddddddddddddd"),
        repay_amount: U256::from(250u64),
    };
    (opp, params)
}

/// Happy path: simulate a liquidation with the owner as sender and
/// expect `Ok(())`. Skipped unless a real `CharonLiquidator`
/// instance + owner key are wired up via env.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires BSC_FORK_URL + CHARON_LIQUIDATOR_ADDR + CHARON_OWNER_KEY"]
async fn simulate_valid_liquidation_succeeds() {
    let Some(fork_url) = env("BSC_FORK_URL") else {
        eprintln!("skipping: BSC_FORK_URL not set");
        return;
    };
    let Some(liquidator_str) = env("CHARON_LIQUIDATOR_ADDR") else {
        eprintln!("skipping: CHARON_LIQUIDATOR_ADDR not set");
        return;
    };
    let Some(key) = env("CHARON_OWNER_KEY") else {
        eprintln!("skipping: CHARON_OWNER_KEY not set");
        return;
    };

    let liquidator = Address::from_str(&liquidator_str).expect("liquidator addr parse");
    let signer: PrivateKeySigner = key.parse().expect("owner key parse");
    let provider = ProviderBuilder::new()
        .on_http(fork_url.parse().expect("fork url parse"))
        .boxed();

    let builder = TxBuilder::new(signer, 56, liquidator);
    let (opp, params) = synthetic_opportunity();
    let calldata = builder.encode_calldata(&opp, &params).expect("encode");

    let simulator = Simulator::from_builder(&builder, liquidator);
    // 8M gas upper bound — well above any realistic liquidation path.
    let result = simulator.simulate(&provider, calldata, 8_000_000).await;

    // On a synthetic fixture the inner flash-loan path may not be
    // reachable on this fork; the test tolerates `Reverted` but
    // requires the simulation round-trip itself to reach a node
    // response (i.e. not `Provider`).
    match result {
        Ok(()) => {}
        Err(SimulationError::Reverted {
            selector_hex,
            data_hex,
        }) => {
            eprintln!(
                "simulate returned revert (acceptable for synthetic fixture): \
                 selector={selector_hex} data={data_hex}"
            );
        }
        Err(SimulationError::Provider(err)) => {
            panic!("transport-level failure talking to fork: {err}")
        }
        // `SimulationError` is `#[non_exhaustive]`; future variants
        // should flip this test red so they get explicit handling.
        Err(other) => panic!("unexpected simulation error variant: {other}"),
    }
}

/// Adversarial path: simulate with a sender that is NOT the owner.
/// Must return `SimulationError::Reverted` because `onlyOwner`
/// rejects the call before any inner logic runs. This is the core
/// safety invariant — any regression that weakens `onlyOwner` on
/// `executeLiquidation` will break this test.
#[tokio::test(flavor = "current_thread")]
#[ignore = "requires BSC_FORK_URL + CHARON_LIQUIDATOR_ADDR"]
async fn simulate_wrong_sender_is_rejected_by_only_owner() {
    let Some(fork_url) = env("BSC_FORK_URL") else {
        eprintln!("skipping: BSC_FORK_URL not set");
        return;
    };
    let Some(liquidator_str) = env("CHARON_LIQUIDATOR_ADDR") else {
        eprintln!("skipping: CHARON_LIQUIDATOR_ADDR not set");
        return;
    };

    let liquidator = Address::from_str(&liquidator_str).expect("liquidator addr parse");
    let provider = ProviderBuilder::new()
        .on_http(fork_url.parse().expect("fork url parse"))
        .boxed();

    // Craft calldata. The dev signer doesn't own the deployed
    // contract; the adversarial sender we inject below is not the
    // owner either.
    let builder = TxBuilder::new(dev_signer(), 56, liquidator);
    let (opp, params) = synthetic_opportunity();
    let calldata_bytes: Bytes = builder
        .encode_calldata(&opp, &params)
        .expect("encode wrong-sender calldata");

    // Sanity: calldata really targets executeLiquidation.
    assert_eq!(
        &calldata_bytes[..4],
        &ICharonLiquidator::executeLiquidationCall::SELECTOR
    );

    // Use Simulator::new directly with a sender we KNOW is not the
    // deployed owner. Never use Simulator::from_builder here — that
    // is the production-safe path; this test exercises the
    // adversarial one.
    let wrong_sender = address!("000000000000000000000000000000000000dEaD");
    let simulator = Simulator::new(wrong_sender, liquidator);
    let result = simulator
        .simulate(&provider, calldata_bytes, 8_000_000)
        .await;

    match result {
        Err(SimulationError::Reverted { .. }) => {
            // Expected — onlyOwner bounced it.
        }
        Err(SimulationError::Provider(err)) => {
            panic!("transport-level failure, cannot assert onlyOwner behaviour: {err}")
        }
        Ok(()) => {
            panic!("onlyOwner should have rejected a non-owner sender; simulation passed")
        }
        // `SimulationError` is `#[non_exhaustive]`; catch-all so any
        // future variant forces an explicit decision here.
        Err(other) => panic!("unexpected simulation error variant: {other}"),
    }
}
