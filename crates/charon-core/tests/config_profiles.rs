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
    // `CHARON_ANVIL_PORT` has a `:-8545` default in fork.toml (#247),
    // so the test doesn't need to pre-set it. Loading default.toml
    // below also needs its env vars populated because we compare the
    // fork profit gate against the default one.
    set_env(&[
        ("BNB_WS_URL", "wss://example/bnb"),
        ("BNB_HTTP_URL", "https://example/bnb"),
        ("BSC_PRIVATE_RPC_URL", "https://example/bnb-private"),
    ]);

    let fork_path = workspace_root().join("config/fork.toml");
    let cfg = Config::load(&fork_path).expect("fork.toml should parse");

    // Profile-level invariants the bot relies on at startup.
    cfg.validate()
        .expect("fork profile must pass Config::validate (loopback RPC check)");

    assert_eq!(cfg.chain["bnb"].chain_id, 56);
    assert!(
        cfg.chain["bnb"].ws_url.starts_with("ws://127.0.0.1"),
        "fork profile must point ws_url at the local anvil instance, got {}",
        cfg.chain["bnb"].ws_url
    );
    assert!(
        cfg.chain["bnb"].http_url.starts_with("http://127.0.0.1"),
        "fork profile must point http_url at the local anvil instance, got {}",
        cfg.chain["bnb"].http_url
    );
    assert!(
        cfg.flashloan.contains_key("aave_v3_bsc"),
        "fork profile keeps Aave V3 — mainnet state inherited by the fork"
    );

    // profile_tag guards the loopback-only invariant at startup (#254).
    assert_eq!(
        cfg.bot.profile_tag.as_deref(),
        Some("fork"),
        "fork profile must carry profile_tag=\"fork\" so Config::validate can lock down non-loopback RPCs"
    );

    // The `/metrics` exporter is authless — binding to 0.0.0.0 on a
    // local demo laptop silently leaks scanner/wallet/gas state to LAN
    // peers. Lock loopback-only for the fork profile explicitly.
    assert!(
        cfg.metrics.bind.ip().is_loopback(),
        "fork profile metrics.bind must be a loopback address, got {}",
        cfg.metrics.bind
    );

    // Lowered profit gate is the entire point of the fork profile —
    // if a future edit accidentally raises it to match default.toml
    // (or higher), demos silently stop firing on small staged
    // positions and this assert catches it.
    let default_cfg =
        Config::load(workspace_root().join("config/default.toml")).expect("default.toml parses");
    assert!(
        cfg.bot.min_profit_usd < default_cfg.bot.min_profit_usd,
        "fork profile min_profit_usd ({}) must be strictly lower than default profile ({}) — \
         the fork is a demo-staging surface",
        cfg.bot.min_profit_usd,
        default_cfg.bot.min_profit_usd
    );

    // `[liquidator.bnb]` placeholder was dropped on feat/24 (commit
    // 4969bb7) because its address(0) tripped TxBuilder encoding the
    // moment the executor tried to build calldata (#252). Lock that
    // in so a future refactor doesn't re-introduce a zero-address row.
    assert!(
        cfg.liquidator.is_empty(),
        "fork profile must not ship a liquidator placeholder — deploy via forge and add \
         [liquidator.bnb] post-fact"
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
