//! Profile smoke-tests: every shipped `config/*.toml` must parse
//! cleanly once its referenced environment variables are populated.
//!
//! These tests are pure deserialization — they exercise only TOML
//! parse + struct validation and never open a socket, never construct
//! a `ChainProvider`, and never call any RPC method. That is a hard
//! invariant so `cargo test --workspace` stays green on clean CI
//! checkouts that have no live Chapel/BSC endpoint (see #258). Any
//! test that would touch live IO belongs behind `#[ignore]` with an
//! env-var guard (e.g. `CHARON_INTEGRATION_TEST=1`), not here.
//!
//! The workspace forbids `unsafe_code` (see top-level `Cargo.toml`),
//! which rules out `std::env::set_var` — that call is `unsafe` under
//! Rust 2024. To avoid process-global env mutation entirely, these
//! tests read the TOML file directly, perform the `${VAR}` ⇒ stub
//! substitution in a local string, and hand the result to
//! `Config::from_str`. Both functions are public, both exercise the
//! exact validation path `Config::load` uses.

use std::fs;
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

/// Read `path` and replace every `${VAR}` / `${VAR:-default}` token
/// using `pairs` instead of the real process environment. Leaves
/// `${VAR:-default}` placeholders whose `VAR` is not in `pairs` to
/// resolve via their embedded default. Unknown tokens without a
/// default trigger a test-time panic — a missing stub for a required
/// var is almost always a test-bug rather than a config issue.
fn load_with_stubbed_env(path: &PathBuf, pairs: &[(&str, &str)]) -> String {
    let raw = fs::read_to_string(path).expect("read config toml");
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw.as_str();
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find('}').expect("unterminated placeholder in fixture");
        let token = &after[..end];
        let (name, default) = match token.split_once(":-") {
            Some((n, d)) => (n, Some(d)),
            None => (token, None),
        };
        let value = pairs
            .iter()
            .find_map(|(k, v)| (*k == name).then_some(*v))
            .or(default)
            .unwrap_or_else(|| panic!("env var `{name}` not stubbed and has no default"));
        out.push_str(value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

#[test]
fn default_profile_parses() {
    let pairs = [
        ("BNB_WS_URL", "wss://example/bnb"),
        ("BNB_HTTP_URL", "https://example/bnb"),
        ("CHARON_BSC_PRIVATE_RPC_URL", "https://example/bnb-private"),
        ("CHARON_BSC_PRIVATE_RPC_AUTH", ""),
        ("CHARON_SIGNER_KEY", ""),
        // `${CHARON_METRICS_AUTH_TOKEN}` sits inside a commented-out
        // line in default.toml, but `substitute_env_vars` is a raw
        // text scan — it replaces the token regardless of the
        // surrounding TOML comment — so the stub has to satisfy it.
        ("CHARON_METRICS_AUTH_TOKEN", ""),
    ];
    let raw = load_with_stubbed_env(&workspace_root().join("config/default.toml"), &pairs);
    let cfg = Config::from_str(&raw).expect("default.toml should parse");

    assert_eq!(cfg.chain["bnb"].chain_id, 56);
    assert!(cfg.flashloan.contains_key("aave_v3_bsc"));
    assert!(cfg.metrics.enabled);
    // Default profile is untagged — fork-branch relaxations must not
    // trigger. Pair this with `fork_profile_parses_*` below so a
    // future edit that accidentally tags default.toml as "fork" fails
    // at least one test.
    assert!(
        cfg.bot.profile_tag.is_none(),
        "default profile must NOT carry profile_tag — that marker is reserved for fork.toml"
    );
}

#[test]
fn testnet_profile_parses_and_omits_flashloan() {
    let pairs = [
        ("CHARON_BNB_TESTNET_WS_URL", "wss://example/chapel"),
        ("CHARON_BNB_TESTNET_HTTP_URL", "https://example/chapel"),
        ("CHARON_BNB_TESTNET_PRIVATE_RPC_URL", ""),
        ("CHARON_SIGNER_KEY", ""),
    ];
    let raw = load_with_stubbed_env(&workspace_root().join("config/testnet.toml"), &pairs);
    let cfg = Config::from_str(&raw).expect("testnet.toml should parse");

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
    assert!(
        cfg.chain["bnb"].allow_public_mempool,
        "testnet profile must opt in to public mempool — no private RPC on Chapel"
    );
    assert!(
        cfg.bot.profile_tag.is_none(),
        "testnet profile must NOT carry profile_tag=\"fork\" — that's reserved for fork.toml"
    );
}

#[test]
fn fork_profile_parses_and_targets_localhost() {
    // `config/fork.toml` env substitution: only `CHARON_ANVIL_PORT`
    // is referenced and it carries its own `:-8545` default, so no
    // stubs are strictly required. We still pass an empty pairs list
    // so the fixture helper panics loudly if a future edit adds a
    // new `${VAR}` without a default — forcing the test to be
    // updated alongside the TOML.
    let fork_path = workspace_root().join("config/fork.toml");
    let raw = load_with_stubbed_env(&fork_path, &[]);
    let cfg = Config::from_str(&raw).expect("fork.toml should parse and validate");

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
    // local demo laptop silently leaks scanner/wallet/gas state to
    // LAN peers. Lock loopback-only for the fork profile explicitly.
    assert!(
        cfg.metrics.bind.ip().is_loopback(),
        "fork profile metrics.bind must be a loopback address, got {}",
        cfg.metrics.bind
    );

    // Lowered profit gate is the entire point of the fork profile —
    // if a future edit accidentally raises it to match default.toml
    // (or higher), demos silently stop firing on small staged
    // positions and this assert catches it. Compare in 1e6 USD
    // fixed-point (the post-feat/19 schema); f64 comparisons are not
    // a thing any more.
    let default_pairs = [
        ("BNB_WS_URL", "wss://example/bnb"),
        ("BNB_HTTP_URL", "https://example/bnb"),
        ("CHARON_BSC_PRIVATE_RPC_URL", "https://example/bnb-private"),
        ("CHARON_BSC_PRIVATE_RPC_AUTH", ""),
        ("CHARON_SIGNER_KEY", ""),
        ("CHARON_METRICS_AUTH_TOKEN", ""),
    ];
    let default_raw = load_with_stubbed_env(
        &workspace_root().join("config/default.toml"),
        &default_pairs,
    );
    let default_cfg = Config::from_str(&default_raw).expect("default.toml parses");
    assert!(
        cfg.bot.min_profit_usd_1e6 < default_cfg.bot.min_profit_usd_1e6,
        "fork profile min_profit_usd_1e6 ({}) must be strictly lower than default profile ({}) — \
         the fork is a demo-staging surface",
        cfg.bot.min_profit_usd_1e6,
        default_cfg.bot.min_profit_usd_1e6
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
