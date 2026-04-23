//! TOML config loader with `${ENV_VAR}` / `${ENV_VAR:-default}`
//! substitution for secrets.
//!
//! Usage:
//! ```no_run
//! use charon_core::config::Config;
//! let cfg = Config::load("config/default.toml").unwrap();
//! ```

use alloy::primitives::Address;
use anyhow::{Context, anyhow};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;

/// Profile tag marking the `config/fork.toml` local-anvil profile. Used
/// by [`Config::validate`] to refuse mainnet endpoints under a lowered
/// profit gate.
const PROFILE_TAG_FORK: &str = "fork";

/// Top-level Charon config loaded from `config/default.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub bot: BotConfig,
    /// Chains keyed by short name (e.g. `"bnb"`).
    pub chain: HashMap<String, ChainConfig>,
    /// Lending protocols keyed by short name (e.g. `"venus"`).
    pub protocol: HashMap<String, ProtocolConfig>,
    /// Flash-loan sources keyed by short name (e.g. `"aave_v3_bsc"`).
    /// Optional so profiles targeting chains without a deployed
    /// flash-loan venue (e.g. BSC testnet / Chapel, where Aave V3 is
    /// not live) can omit the section entirely. Missing map ⇒ bot runs
    /// read-only: block listener + scanner populate, but the executor
    /// path short-circuits because no opportunity can be routed.
    #[serde(default)]
    pub flashloan: HashMap<String, FlashLoanConfig>,
    /// Deployed liquidator contracts keyed by chain name. Optional for
    /// the same reason as `flashloan` — testnet profiles have no
    /// liquidator deployed yet.
    #[serde(default)]
    pub liquidator: HashMap<String, LiquidatorConfig>,
    /// Chainlink feed addresses per chain, keyed by asset symbol
    /// (e.g. `chainlink.bnb.BNB = "0x…"`). Missing key = no feed
    /// configured, scanner falls back to protocol oracle.
    #[serde(default)]
    pub chainlink: HashMap<String, HashMap<String, Address>>,
    /// Prometheus exporter configuration. Missing `[metrics]` block ⇒
    /// defaults from [`MetricsConfig::default`] (enabled, port 9091).
    #[serde(default)]
    pub metrics: MetricsConfig,
}

/// Prometheus exporter configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct MetricsConfig {
    /// Start the exporter at bot startup. Set to `false` to run charon
    /// with zero metrics overhead (e.g. one-shot debug runs).
    #[serde(default = "default_metrics_enabled")]
    pub enabled: bool,
    /// Bind address for the `/metrics` HTTP listener. `0.0.0.0:9091`
    /// keeps it off the Prometheus-server default port (`9090`) so a
    /// local compose stack doesn't collide.
    #[serde(default = "default_metrics_bind")]
    pub bind: SocketAddr,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: default_metrics_enabled(),
            bind: default_metrics_bind(),
        }
    }
}

fn default_metrics_enabled() -> bool {
    true
}

fn default_metrics_bind() -> SocketAddr {
    "0.0.0.0:9091".parse().expect("valid default metrics bind")
}

/// Bot-level knobs — thresholds and intervals.
#[derive(Debug, Clone, Deserialize)]
pub struct BotConfig {
    /// Drop opportunities below this USD profit threshold.
    pub min_profit_usd: f64,
    /// Skip liquidations when gas price exceeds this (gwei).
    pub max_gas_gwei: u64,
    /// Polling interval for protocols that don't push events.
    pub scan_interval_ms: u64,
    /// Health factor at or below which a position becomes liquidatable.
    /// Stored as a float for readability (e.g. `1.0`); the scanner
    /// scales it to a 1e18-fixed `U256` internally.
    #[serde(default = "default_liquidatable_threshold")]
    pub liquidatable_threshold: f64,
    /// Upper bound of the near-liquidation watch band. Positions in
    /// `[liquidatable_threshold, near_liq_threshold)` are pre-cached so
    /// the bot can fire immediately on the next adverse price move.
    #[serde(default = "default_near_liq_threshold")]
    pub near_liq_threshold: f64,
    /// Optional profile marker used by [`Config::validate`] to enforce
    /// profile-specific invariants at startup. `Some("fork")` marks
    /// `config/fork.toml` — a local-only profile that must target
    /// loopback RPCs because its profit gate is intentionally lowered
    /// for demo staging. Production profiles leave this unset.
    #[serde(default)]
    pub profile_tag: Option<String>,
}

fn default_liquidatable_threshold() -> f64 {
    1.0
}

fn default_near_liq_threshold() -> f64 {
    1.05
}

/// RPC endpoints for a single chain.
#[derive(Debug, Clone, Deserialize)]
pub struct ChainConfig {
    pub chain_id: u64,
    pub ws_url: String,
    pub http_url: String,
    /// EIP-1559 priority fee (tip) in gwei. Per chain because BSC's
    /// validator-friendly tip is ~1 gwei whereas L2 tips run sub-gwei.
    #[serde(default = "default_priority_fee_gwei")]
    pub priority_fee_gwei: u64,
    /// Optional private-RPC endpoint for transaction submission
    /// (bloxroute / blocknative on BSC, sequencer endpoints on L2s).
    /// When set, the submitter posts `eth_sendRawTransaction` here
    /// instead of the public `http_url` so pending txs skip the
    /// public mempool and front-runners.
    #[serde(default)]
    pub private_rpc_url: Option<String>,
}

fn default_priority_fee_gwei() -> u64 {
    1
}

/// Address and metadata for a lending protocol on a specific chain.
#[derive(Debug, Clone, Deserialize)]
pub struct ProtocolConfig {
    /// Name of the chain this protocol runs on (must match a key in `[chain]`).
    pub chain: String,
    /// Protocol's main entry point (e.g. Venus Unitroller / Comptroller).
    pub comptroller: Address,
}

/// A flash-loan source available on a given chain.
#[derive(Debug, Clone, Deserialize)]
pub struct FlashLoanConfig {
    pub chain: String,
    /// Pool / vault address (Aave V3 Pool, Balancer Vault, etc.).
    pub pool: Address,
}

/// Address of the deployed `CharonLiquidator` contract on a chain.
#[derive(Debug, Clone, Deserialize)]
pub struct LiquidatorConfig {
    pub chain: String,
    pub contract_address: Address,
}

/// Typed errors produced by [`Config::validate`]. Kept separate from
/// `anyhow::Error` so callers (main.rs, integration tests) can match on
/// the exact invariant that failed and render remediation copy that
/// names the offending field. New variants get added as new profile
/// guards land.
#[derive(Debug)]
pub enum ConfigError {
    /// A `profile_tag = "fork"` profile has a chain whose `ws_url` /
    /// `http_url` does not resolve to a loopback host. The attached
    /// strings name the chain, field, and offending URL so the operator
    /// sees exactly what to fix in the TOML.
    ForkProfileNonLoopbackRpc {
        chain: String,
        field: &'static str,
        url: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::ForkProfileNonLoopbackRpc { chain, field, url } => write!(
                f,
                "profile_tag=\"fork\" in config/fork.toml is a local-only profile and must \
                 point every chain's ws_url/http_url at loopback; got chain.{chain}.{field}={url}. \
                 Refusing to start with a lowered profit gate against non-loopback RPC."
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Read a TOML config file, substitute `${ENV_VAR}` placeholders, parse.
    ///
    /// Returns an error if the file is missing, malformed, or references an
    /// environment variable that isn't set (and has no `:-default` clause).
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config at {}", path.display()))?;
        let substituted = substitute_env_vars(&raw)
            .with_context(|| format!("env substitution failed for {}", path.display()))?;
        let config: Config = toml::from_str(&substituted)
            .with_context(|| format!("failed to parse TOML at {}", path.display()))?;
        Ok(config)
    }

    /// Enforce profile-specific invariants. Call this at startup before
    /// opening any RPC connection so a misconfigured profile fails
    /// fast with an actionable error rather than quietly pointing a
    /// lowered profit gate at a production endpoint.
    ///
    /// Current rules:
    /// - `bot.profile_tag == Some("fork")` ⇒ every chain's `ws_url` and
    ///   `http_url` must resolve to a loopback host (`127.0.0.1`,
    ///   `::1`, or `localhost`). A non-fork profile pointing at
    ///   loopback is *not* rejected — local-geth dev runs are a
    ///   supported workflow.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.bot.profile_tag.as_deref() == Some(PROFILE_TAG_FORK) {
            for (chain_name, chain_cfg) in &self.chain {
                for (field, url) in [
                    ("ws_url", chain_cfg.ws_url.as_str()),
                    ("http_url", chain_cfg.http_url.as_str()),
                ] {
                    if !is_loopback_url(url) {
                        return Err(ConfigError::ForkProfileNonLoopbackRpc {
                            chain: chain_name.clone(),
                            field,
                            url: url.to_string(),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

/// Return `true` iff `url`'s host component is a loopback address.
///
/// Accepts `127.0.0.0/8`, `::1`, and the `localhost` hostname (case
/// insensitive). Works against `http://`, `https://`, `ws://`, and
/// `wss://` URLs; any scheme that uses the `scheme://host[:port]/…`
/// shape is handled.
///
/// This is a string-level check, not a DNS resolve — the fork profile
/// only needs to reject obviously-non-local URLs at config-load time.
/// DNS-based `localhost` aliases that resolve off-loopback are
/// sufficiently rare that we accept them here and rely on the operator
/// not to shoot themselves in the foot with a pathological /etc/hosts.
fn is_loopback_url(url: &str) -> bool {
    let Some(after_scheme) = url.split_once("://").map(|(_, rest)| rest) else {
        return false;
    };
    // Strip off userinfo if present ("user:pass@host").
    let after_userinfo = after_scheme.rsplit_once('@').map_or(after_scheme, |(_, h)| h);
    // Host ends at the first '/', '?', '#', or end-of-string.
    let host_and_port = after_userinfo
        .find(['/', '?', '#'])
        .map_or(after_userinfo, |i| &after_userinfo[..i]);

    // IPv6 literal: "[::1]:8545" → pull out "::1".
    let host = if let Some(rest) = host_and_port.strip_prefix('[') {
        match rest.find(']') {
            Some(end) => &rest[..end],
            None => return false,
        }
    } else {
        // IPv4 / hostname: split off ":port" by rfind so hostnames with
        // colons (none expected here) aren't mis-parsed.
        host_and_port
            .rsplit_once(':')
            .map_or(host_and_port, |(h, _)| h)
    };

    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        return v4.is_loopback();
    }
    if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
        return v6.is_loopback();
    }
    false
}

/// Replace every `${NAME}` and `${NAME:-default}` occurrence in `input`
/// with the value of environment variable `NAME`, falling back to
/// `default` when the `:-` form is used and the variable is unset or
/// empty.
///
/// Unset variable without a `:-default` is a hard error — existing
/// behavior preserved for profiles that want to enforce "env must be
/// set" (e.g. secrets in `config/default.toml`).
///
/// An unterminated `${` (no closing `}`) is also a hard error.
fn substitute_env_vars(input: &str) -> anyhow::Result<String> {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| anyhow!("unterminated `${{` in config"))?;
        let expr = &after[..end];
        let (var_name, default) = match expr.split_once(":-") {
            Some((name, def)) => (name, Some(def)),
            None => (expr, None),
        };
        let value = match (std::env::var(var_name), default) {
            // Set and non-empty: use the env value regardless of default.
            (Ok(v), _) if !v.is_empty() => v,
            // Set-but-empty or unset with an explicit default: use the default.
            // (POSIX `${VAR:-default}` semantics — default applies when unset OR empty.)
            (_, Some(def)) => def.to_string(),
            // Unset with no default: hard error (preserves prior behavior).
            (Err(_), None) => {
                return Err(anyhow!("env var `{var_name}` is not set"));
            }
            // Set-but-empty with no default: keep the empty value to
            // preserve prior behavior for secret-bearing profiles.
            (Ok(v), None) => v,
        };
        output.push_str(&value);
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests touch process-global env. Using dedicated var names
    // per test keeps them safe against parallel test execution —
    // cargo only serializes tests inside a single `#[test]` when they
    // race on the same key.
    fn set_var(k: &str, v: &str) {
        // Safety: tests use unique var names per case; no other thread
        // reads these concurrently.
        unsafe { std::env::set_var(k, v) };
    }
    fn unset_var(k: &str) {
        // Safety: same reasoning as set_var.
        unsafe { std::env::remove_var(k) };
    }

    #[test]
    fn env_substitution_plain_var_set() {
        set_var("CHARON_TEST_PLAIN", "hello");
        let out = substitute_env_vars("x=${CHARON_TEST_PLAIN}").unwrap();
        assert_eq!(out, "x=hello");
        unset_var("CHARON_TEST_PLAIN");
    }

    #[test]
    fn env_substitution_plain_var_unset_errors() {
        unset_var("CHARON_TEST_UNSET_NO_DEFAULT");
        let err = substitute_env_vars("x=${CHARON_TEST_UNSET_NO_DEFAULT}")
            .expect_err("unset var without default must error");
        assert!(
            format!("{err}").contains("CHARON_TEST_UNSET_NO_DEFAULT"),
            "error must name the missing var: {err}"
        );
    }

    #[test]
    fn env_substitution_default_used_when_unset() {
        unset_var("CHARON_TEST_UNSET_WITH_DEFAULT");
        let out = substitute_env_vars("p=${CHARON_TEST_UNSET_WITH_DEFAULT:-8545}").unwrap();
        assert_eq!(out, "p=8545");
    }

    #[test]
    fn env_substitution_default_overridden_when_set() {
        set_var("CHARON_TEST_DEFAULT_OVERRIDDEN", "8546");
        let out =
            substitute_env_vars("p=${CHARON_TEST_DEFAULT_OVERRIDDEN:-8545}").unwrap();
        assert_eq!(out, "p=8546");
        unset_var("CHARON_TEST_DEFAULT_OVERRIDDEN");
    }

    #[test]
    fn env_substitution_default_used_when_empty() {
        set_var("CHARON_TEST_EMPTY_WITH_DEFAULT", "");
        let out =
            substitute_env_vars("p=${CHARON_TEST_EMPTY_WITH_DEFAULT:-8545}").unwrap();
        assert_eq!(out, "p=8545");
        unset_var("CHARON_TEST_EMPTY_WITH_DEFAULT");
    }

    #[test]
    fn env_substitution_unterminated_errors() {
        let err = substitute_env_vars("x=${UNCLOSED").expect_err("must reject unterminated ${");
        assert!(format!("{err}").contains("unterminated"));
    }

    #[test]
    fn loopback_url_matches_common_forms() {
        assert!(is_loopback_url("http://127.0.0.1:8545"));
        assert!(is_loopback_url("ws://127.0.0.1"));
        assert!(is_loopback_url("http://localhost:9091"));
        assert!(is_loopback_url("https://LocalHost/"));
        assert!(is_loopback_url("ws://[::1]:8545"));
        assert!(is_loopback_url("http://127.255.255.254"));
    }

    #[test]
    fn loopback_url_rejects_public_hosts() {
        assert!(!is_loopback_url("wss://bsc-rpc.publicnode.com"));
        assert!(!is_loopback_url("https://bsc.drpc.org"));
        assert!(!is_loopback_url("http://10.0.0.1:8545"));
        assert!(!is_loopback_url("http://192.168.1.1"));
        assert!(!is_loopback_url("not-a-url"));
    }
}
