//! Profile smoke-tests: every shipped `config/*.toml` must parse
//! cleanly once its referenced environment variables are populated.
//!
//! Env vars are set inside the test process via `std::env::set_var`,
//! which is `unsafe` under Rust 2024 — the safety contract ("no other
//! thread is reading env at the same time") holds because `cargo test`
//! serializes per-binary tests by default when they touch process
//! globals, and these tests don't spawn threads.

use std::path::PathBuf;

use charon_core::Config;

fn workspace_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` points to `crates/charon-core/`; walk two
    // parents up to reach the workspace root where `config/` lives.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("charon-core sits two levels below the workspace root")
        .to_path_buf()
}

fn set_env(pairs: &[(&str, &str)]) {
    for (k, v) in pairs {
        // Safety: no other thread touches env in this test process.
        unsafe { std::env::set_var(k, v) };
    }
}

#[test]
fn default_profile_parses() {
    set_env(&[
        ("BNB_WS_URL", "wss://example/bnb"),
        ("BNB_HTTP_URL", "https://example/bnb"),
        ("BSC_PRIVATE_RPC_URL", "https://example/bnb-private"),
    ]);

    let path = workspace_root().join("config/default.toml");
    let cfg = Config::load(&path).expect("default.toml should parse");

    assert_eq!(cfg.chain["bnb"].chain_id, 56);
    assert!(cfg.flashloan.contains_key("aave_v3_bsc"));
    assert!(cfg.liquidator.contains_key("bnb"));
    assert!(cfg.metrics.enabled);
}

#[test]
fn fork_profile_parses_and_targets_localhost() {
    // No env substitution needed — the fork profile hard-codes localhost.
    let path = workspace_root().join("config/fork.toml");
    let cfg = Config::load(&path).expect("fork.toml should parse");

    assert_eq!(cfg.chain["bnb"].chain_id, 56);
    assert!(
        cfg.chain["bnb"].ws_url.starts_with("ws://127.0.0.1"),
        "fork profile must point at the local anvil instance"
    );
    assert!(
        cfg.flashloan.contains_key("aave_v3_bsc"),
        "fork profile keeps Aave V3 — mainnet state inherited by the fork"
    );
}

#[test]
fn testnet_profile_parses_and_omits_flashloan() {
    set_env(&[
        ("BNB_TESTNET_WS_URL", "wss://example/chapel"),
        ("BNB_TESTNET_HTTP_URL", "https://example/chapel"),
    ]);

    let path = workspace_root().join("config/testnet.toml");
    let cfg = Config::load(&path).expect("testnet.toml should parse");

    assert_eq!(cfg.chain["bnb"].chain_id, 97);
    assert!(
        cfg.flashloan.is_empty(),
        "testnet profile must omit flashloan — Aave V3 is not deployed on Chapel"
    );
    assert!(
        cfg.liquidator.is_empty(),
        "testnet profile has no deployed liquidator"
    );
    assert!(cfg.metrics.enabled);
}
