//! TOML config loader with `${ENV_VAR}` substitution for secrets.
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
    /// Key into `[chain.*]` naming the active chain for this profile
    /// (e.g. `"bnb"` for mainnet, `"bnb_testnet"` for Chapel). The CLI
    /// `listen` command resolves every chain-scoped lookup (RPC,
    /// flashloan, liquidator, chainlink feeds) through this key so a
    /// profile that uses a non-mainnet key does not panic on a
    /// hard-coded `"bnb"` lookup. Defaults to `"bnb"` for backwards
    /// compatibility with the v0.1 mainnet profile.
    #[serde(default = "default_bot_chain")]
    pub chain: String,
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
}

fn default_bot_chain() -> String {
    "bnb".to_string()
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

impl Config {
    /// Read a TOML config file, substitute `${ENV_VAR}` placeholders, parse.
    ///
    /// Returns an error if the file is missing, malformed, or references an
    /// environment variable that isn't set.
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config at {}", path.display()))?;
        let substituted = substitute_env_vars(&raw)
            .with_context(|| format!("env substitution failed for {}", path.display()))?;
        let config: Config = toml::from_str(&substituted)
            .with_context(|| format!("failed to parse TOML at {}", path.display()))?;
        config
            .validate()
            .with_context(|| format!("invalid config at {}", path.display()))?;
        Ok(config)
    }

    /// Reject configurations whose liquidation path is half-wired.
    ///
    /// `flashloan` and `liquidator` are both `#[serde(default)]` so a
    /// profile (e.g. testnet) can omit both and run in read-only mode.
    /// A profile that supplies exactly one of the two is almost
    /// always an accidental omission: the bot starts, every
    /// opportunity silently short-circuits at the missing half, and
    /// the operator sees no error. Fail fast at load time instead,
    /// naming the offending chain so the mismatch is obvious.
    ///
    /// Symmetric: for every `flashloan` entry on chain X, require a
    /// `liquidator` entry on chain X, and vice versa. All mismatches are
    /// collected into a single error (sorted) rather than short-circuiting
    /// on the first one, so an operator fixing a broken profile sees
    /// every offending chain in one pass instead of running `charon` N
    /// times to surface N problems.
    pub fn validate(&self) -> anyhow::Result<()> {
        use std::collections::BTreeSet;
        let fl_chains: BTreeSet<&str> =
            self.flashloan.values().map(|f| f.chain.as_str()).collect();
        let liq_chains: BTreeSet<&str> = self
            .liquidator
            .values()
            .map(|l| l.chain.as_str())
            .collect();

        let fl_only: Vec<&str> = fl_chains.difference(&liq_chains).copied().collect();
        let liq_only: Vec<&str> = liq_chains.difference(&fl_chains).copied().collect();

        if fl_only.is_empty() && liq_only.is_empty() {
            return Ok(());
        }

        let mut msg = String::from(
            "half-wired liquidation path — every chain must supply both a \
             [flashloan.*] entry and a [liquidator.*] entry, or neither \
             (both omitted ⇒ read-only mode). Offending chains:\n",
        );
        for chain in &fl_only {
            msg.push_str(&format!(
                "  - '{chain}': has [flashloan.*] but no matching [liquidator.*] — \
                 liquidation would be routed but never executed\n"
            ));
        }
        for chain in &liq_only {
            msg.push_str(&format!(
                "  - '{chain}': has [liquidator.*] but no matching [flashloan.*] — \
                 liquidation cannot execute without a flash-loan source\n"
            ));
        }
        Err(anyhow!(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn bot() -> BotConfig {
        BotConfig {
            chain: "bnb".to_string(),
            min_profit_usd: 5.0,
            max_gas_gwei: 10,
            scan_interval_ms: 1000,
            liquidatable_threshold: 1.0,
            near_liq_threshold: 1.05,
        }
    }

    fn base_config() -> Config {
        Config {
            bot: bot(),
            chain: HashMap::new(),
            protocol: HashMap::new(),
            flashloan: HashMap::new(),
            liquidator: HashMap::new(),
            chainlink: HashMap::new(),
            metrics: MetricsConfig::default(),
        }
    }

    fn fl(chain: &str) -> FlashLoanConfig {
        FlashLoanConfig {
            chain: chain.to_string(),
            pool: address!("6807dc923806fe8fd134338eabca509979a7e0cb"),
        }
    }

    fn liq(chain: &str) -> LiquidatorConfig {
        LiquidatorConfig {
            chain: chain.to_string(),
            contract_address: address!("0000000000000000000000000000000000000001"),
        }
    }

    #[test]
    fn validate_passes_when_both_sides_empty() {
        // Testnet profile: no flashloan, no liquidator ⇒ read-only OK.
        base_config().validate().expect("fully-empty profile valid");
    }

    #[test]
    fn validate_passes_when_both_sides_paired() {
        let mut cfg = base_config();
        cfg.flashloan.insert("aave_v3_bsc".into(), fl("bnb"));
        cfg.liquidator.insert("bnb".into(), liq("bnb"));
        cfg.validate().expect("paired profile valid");
    }

    #[test]
    fn validate_passes_when_map_keys_differ_but_inner_chain_matches() {
        // Map keys are labels; the inner `chain` field is what the
        // pipeline pivots on. A profile keyed under arbitrary labels is
        // still valid as long as the chain tags pair up.
        let mut cfg = base_config();
        cfg.flashloan.insert("primary_source".into(), fl("bnb"));
        cfg.liquidator.insert("mainnet_liq".into(), liq("bnb"));
        cfg.validate().expect("inner-chain match is sufficient");
    }

    #[test]
    fn validate_rejects_flashloan_without_liquidator() {
        let mut cfg = base_config();
        cfg.flashloan.insert("aave_v3_bsc".into(), fl("bnb"));
        let err = cfg.validate().expect_err("flashloan-only must fail");
        let msg = format!("{err}");
        assert!(msg.contains("'bnb'"), "error must name the chain: {msg}");
        assert!(msg.contains("[flashloan.*]"), "error must cite flashloan: {msg}");
    }

    #[test]
    fn validate_rejects_liquidator_without_flashloan() {
        let mut cfg = base_config();
        cfg.liquidator.insert("bnb".into(), liq("bnb"));
        let err = cfg.validate().expect_err("liquidator-only must fail");
        let msg = format!("{err}");
        assert!(msg.contains("'bnb'"), "error must name the chain: {msg}");
        assert!(msg.contains("[liquidator.*]"), "error must cite liquidator: {msg}");
    }

    #[test]
    fn validate_reports_every_mismatched_chain_in_one_pass() {
        // Two half-wired chains, one of each shape. Operator sees both
        // without re-running charon.
        let mut cfg = base_config();
        cfg.flashloan.insert("src_a".into(), fl("bnb"));
        cfg.liquidator.insert("liq_b".into(), liq("polygon"));
        let err = cfg.validate().expect_err("two mismatches must fail");
        let msg = format!("{err}");
        assert!(msg.contains("'bnb'"), "missing bnb half-wire: {msg}");
        assert!(msg.contains("'polygon'"), "missing polygon half-wire: {msg}");
    }

    #[test]
    fn validate_passes_for_same_chain_under_different_map_keys() {
        // Guards the review's "no false positive when flashloan and
        // liquidator share the same chain but via different map keys"
        // invariant.
        let mut cfg = base_config();
        cfg.flashloan.insert("aave_v3_bsc".into(), fl("bnb"));
        cfg.liquidator.insert("charon_bnb_v1".into(), liq("bnb"));
        cfg.validate().expect("same chain via different keys is fine");
    }
}

/// Replace every `${NAME}` in `input` with the value of environment variable
/// `NAME`. Returns an error if any referenced variable is unset or if a
/// `${` is not closed by `}`.
fn substitute_env_vars(input: &str) -> anyhow::Result<String> {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| anyhow!("unterminated `${{` in config"))?;
        let var_name = &after[..end];
        let value =
            std::env::var(var_name).with_context(|| format!("env var `{var_name}` is not set"))?;
        output.push_str(&value);
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}
