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
}
