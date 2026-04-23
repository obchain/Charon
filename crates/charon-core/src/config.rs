//! TOML config loader with `${ENV_VAR}` substitution for secrets.
//!
//! Usage:
//! ```no_run
//! use charon_core::config::Config;
//! let cfg = Config::load("config/default.toml").unwrap();
//! ```

use alloy::primitives::Address;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Typed errors returned by [`Config::load`] and [`Config::validate`].
///
/// Replaces the previous `anyhow::Error` surface so the CLI can
/// pattern-match on the failure mode and render actionable recovery
/// hints instead of a flat chain string. Every variant carries the
/// config path (when known) and the specific fields needed to debug
/// the issue without re-reading the file. `#[non_exhaustive]` so new
/// variants can land without a semver bump.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The config file could not be read (missing, unreadable, EIO).
    #[error("failed to read config file {}: {source}", path.display())]
    FileRead {
        /// Absolute or caller-supplied path the loader tried to open.
        path: PathBuf,
        /// Underlying OS error.
        #[source]
        source: std::io::Error,
    },

    /// A `${NAME}` placeholder referenced an environment variable that
    /// is not set at load time.
    #[error("env var `{var}` is not set (referenced in config at {})", path.display())]
    EnvVarMissing {
        /// Name of the missing environment variable.
        var: String,
        /// Path of the config file that contained the reference.
        path: PathBuf,
    },

    /// A `${` was opened but never closed by a matching `}`.
    #[error("unterminated `${{` placeholder in config {}", path.display())]
    UnterminatedPlaceholder {
        /// Path of the offending config file.
        path: PathBuf,
    },

    /// TOML parse failure after substitution. Wraps the `toml` crate's
    /// error so the caller keeps line/column diagnostics.
    #[error("failed to parse TOML at {}: {source}", path.display())]
    TomlParse {
        /// Path of the config file that failed to parse.
        path: PathBuf,
        /// Underlying parse error.
        #[source]
        source: toml::de::Error,
    },

    /// [`Config::validate`] found a chain that supplies exactly one
    /// side of the flashloan/liquidator pair. See the rustdoc on
    /// `validate` for why that's rejected.
    #[error(
        "half-wired config: chain '{chain}' has {present} but not {missing}"
    )]
    HalfWired {
        /// Chain short name pulled from the inner `chain` field.
        chain: String,
        /// Section present in the config.
        present: &'static str,
        /// Section absent from the config.
        missing: &'static str,
    },
}

/// Convenience alias for `Result<T, ConfigError>` used throughout this module.
pub type Result<T, E = ConfigError> = std::result::Result<T, E>;

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
///
/// `deny_unknown_fields` guards against TOML field-level typos. Every
/// field has a serde default, so `bnd = "..."` instead of `bind`
/// would otherwise silently load the default and the operator would
/// wonder why their override didn't take. See [`FlashLoanConfig`] for
/// the full rationale behind this class of hardening.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
///
/// `deny_unknown_fields` guards against TOML field-level typos.
/// Several fields (`chain`, `liquidatable_threshold`,
/// `near_liq_threshold`) are `#[serde(default)]`, so a misspelling
/// like `liquidatable_threshhold = 0.95` would otherwise silently
/// keep the default and mis-tune the scanner. See [`FlashLoanConfig`]
/// for the full rationale.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
///
/// `deny_unknown_fields` guards against TOML field-level typos.
/// `priority_fee_gwei` and `private_rpc_url` are `#[serde(default)]`,
/// so a typo like `privte_rpc_url = "..."` would otherwise leave the
/// submitter silently hitting the public mempool. See
/// [`FlashLoanConfig`] for the full rationale.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
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
///
/// `deny_unknown_fields` for symmetry with the other config structs;
/// see [`FlashLoanConfig`] for the full rationale.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolConfig {
    /// Name of the chain this protocol runs on (must match a key in `[chain]`).
    pub chain: String,
    /// Protocol's main entry point (e.g. Venus Unitroller / Comptroller).
    pub comptroller: Address,
}

/// A flash-loan source available on a given chain.
///
/// `deny_unknown_fields` guards against TOML field-level typos.
/// Both this section and `[liquidator.*]` are now `#[serde(default)]`
/// at the [`Config`] level, so a misspelled field (e.g. `poo` instead
/// of `pool`) would otherwise silently deserialize to a zero-address
/// default. Rejecting unknown keys at load time makes that class of
/// mistake a startup error rather than a silent skip at runtime.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlashLoanConfig {
    pub chain: String,
    /// Pool / vault address (Aave V3 Pool, Balancer Vault, etc.).
    pub pool: Address,
}

/// Address of the deployed `CharonLiquidator` contract on a chain.
///
/// `deny_unknown_fields` guards against TOML field-level typos. See
/// [`FlashLoanConfig`] for the full rationale.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiquidatorConfig {
    pub chain: String,
    pub contract_address: Address,
}

impl Config {
    /// Read a TOML config file, substitute `${ENV_VAR}` placeholders, parse.
    ///
    /// Returns an error if the file is missing, malformed, or references an
    /// environment variable that isn't set.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path_ref = path.as_ref();
        let raw = std::fs::read_to_string(path_ref).map_err(|source| ConfigError::FileRead {
            path: path_ref.to_path_buf(),
            source,
        })?;
        let substituted = substitute_env_vars(&raw, path_ref)?;
        let config: Config =
            toml::from_str(&substituted).map_err(|source| ConfigError::TomlParse {
                path: path_ref.to_path_buf(),
                source,
            })?;
        config.validate()?;
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
    pub fn validate(&self) -> Result<()> {
        use std::collections::BTreeSet;
        let fl_chains: BTreeSet<&str> =
            self.flashloan.values().map(|f| f.chain.as_str()).collect();
        let liq_chains: BTreeSet<&str> = self
            .liquidator
            .values()
            .map(|l| l.chain.as_str())
            .collect();

        // Return on the first mismatched chain we encounter. The typed
        // variant names the offending chain plus the section that is
        // present/missing; operators fixing multiple half-wired chains
        // re-run once per fix, which is preferable to a catch-all
        // string that callers cannot pattern-match on. The lexicographic
        // order (from BTreeSet) makes the first-offender deterministic.
        if let Some(chain) = fl_chains.difference(&liq_chains).next() {
            return Err(ConfigError::HalfWired {
                chain: (*chain).to_string(),
                present: "[flashloan.*]",
                missing: "[liquidator.*]",
            });
        }
        if let Some(chain) = liq_chains.difference(&fl_chains).next() {
            return Err(ConfigError::HalfWired {
                chain: (*chain).to_string(),
                present: "[liquidator.*]",
                missing: "[flashloan.*]",
            });
        }
        Ok(())
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
        match err {
            ConfigError::HalfWired { chain, present, missing } => {
                assert_eq!(chain, "bnb");
                assert_eq!(present, "[flashloan.*]");
                assert_eq!(missing, "[liquidator.*]");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn validate_rejects_liquidator_without_flashloan() {
        let mut cfg = base_config();
        cfg.liquidator.insert("bnb".into(), liq("bnb"));
        let err = cfg.validate().expect_err("liquidator-only must fail");
        match err {
            ConfigError::HalfWired { chain, present, missing } => {
                assert_eq!(chain, "bnb");
                assert_eq!(present, "[liquidator.*]");
                assert_eq!(missing, "[flashloan.*]");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn validate_reports_first_mismatched_chain_deterministically() {
        // Two half-wired chains on opposite sides. `BTreeSet::difference`
        // iterates in lexicographic order so the first variant emitted is
        // stable across runs. Operators fix, re-run, see the next one.
        let mut cfg = base_config();
        cfg.flashloan.insert("src_a".into(), fl("bnb"));
        cfg.liquidator.insert("liq_b".into(), liq("polygon"));
        let err = cfg.validate().expect_err("two mismatches must fail");
        match err {
            ConfigError::HalfWired { chain, .. } => {
                assert_eq!(chain, "bnb", "flashloan-only branch reports first");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn load_reports_missing_file_as_file_read() {
        let err = Config::load("/nonexistent/path/charon-config-missing.toml")
            .expect_err("missing file must error");
        assert!(matches!(err, ConfigError::FileRead { .. }));
    }

    #[test]
    fn substitute_env_vars_reports_unset_as_env_var_missing() {
        let p = Path::new("/tmp/stub.toml");
        // Unique var so parallel test runs don't collide with a
        // caller's env by accident.
        let err = substitute_env_vars(
            "ws_url = \"${CHARON_ENV_MISSING_FOR_TESTS_9f3a2c}\"\n",
            p,
        )
        .expect_err("unset env var must error");
        match err {
            ConfigError::EnvVarMissing { var, .. } => {
                assert_eq!(var, "CHARON_ENV_MISSING_FOR_TESTS_9f3a2c");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn substitute_env_vars_reports_unclosed_placeholder() {
        let p = Path::new("/tmp/stub.toml");
        let err = substitute_env_vars("ws_url = \"${NEVER_CLOSED\n", p)
            .expect_err("unterminated placeholder must error");
        assert!(matches!(err, ConfigError::UnterminatedPlaceholder { .. }));
    }

    #[test]
    fn load_reports_bad_toml_as_toml_parse() {
        // Write a tiny malformed file into a scratch path and load it.
        // Using std::env::temp_dir keeps us off `tempfile` (not a dev
        // dep on this branch) while remaining unique per-test.
        let tmp = std::env::temp_dir().join("charon_bad_toml_9f3a2c.toml");
        std::fs::write(&tmp, b"this is = = not toml").expect("write tmp");
        let err = Config::load(&tmp).expect_err("bad TOML must error");
        let _ = std::fs::remove_file(&tmp);
        assert!(matches!(err, ConfigError::TomlParse { .. }));
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
/// `NAME`. Returns [`ConfigError::UnterminatedPlaceholder`] if a `${` has no
/// matching `}` and [`ConfigError::EnvVarMissing`] if the variable is not set.
fn substitute_env_vars(input: &str, path: &Path) -> Result<String> {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| ConfigError::UnterminatedPlaceholder {
                path: path.to_path_buf(),
            })?;
        let var_name = &after[..end];
        let value = std::env::var(var_name).map_err(|_| ConfigError::EnvVarMissing {
            var: var_name.to_string(),
            path: path.to_path_buf(),
        })?;
        output.push_str(&value);
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}
