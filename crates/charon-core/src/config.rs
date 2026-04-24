//! TOML config loader with `${ENV_VAR}` / `${ENV_VAR:-default}` substitution
//! for secrets, structured error variants, secret redaction in `Debug`, and
//! cross-reference validation.
//!
//! Usage:
//! ```no_run
//! use charon_core::config::Config;
//! let cfg = Config::load("config/default.toml").unwrap();
//! ```

use alloy::primitives::{Address, U256};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

/// Structured error returned by `Config::load` / `Config::from_str`.
///
/// Callers match on the variant to choose exit code or remediation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    #[error("config file not found: {0}")]
    NotFound(PathBuf),
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("env var `{0}` not set")]
    UnsetEnvVar(String),
    #[error("invalid env var name `{0}` — must match [A-Z_][A-Z0-9_]*")]
    InvalidEnvVarName(String),
    #[error("unterminated `${{` in config")]
    UnterminatedInterp,
    #[error("toml parse: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("validation: {0}")]
    Validation(String),
}

/// Shorthand `Result`.
pub type Result<T> = std::result::Result<T, ConfigError>;

/// Top-level Charon config loaded from `config/default.toml`.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub bot: BotConfig,
    /// Chains keyed by short name (e.g. `"bnb"`).
    pub chain: HashMap<String, ChainConfig>,
    /// Lending protocols keyed by short name (e.g. `"venus"`).
    pub protocol: HashMap<String, ProtocolConfig>,
    /// Flash-loan sources keyed by short name (e.g. `"aave_v3_bsc"`).
    pub flashloan: HashMap<String, FlashLoanConfig>,
    /// Deployed liquidator contracts keyed by chain name.
    pub liquidator: HashMap<String, LiquidatorConfig>,
}

impl fmt::Debug for Config {
    // Redact the contents of `chain` — it carries full RPC URLs with API keys.
    // Everything else is scalar thresholds and public addresses, safe to print.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("bot", &self.bot)
            .field("chain", &ChainCount(self.chain.len()))
            .field("protocol", &self.protocol)
            .field("flashloan", &self.flashloan)
            .field("liquidator", &self.liquidator)
            .finish()
    }
}

struct ChainCount(usize);
impl fmt::Debug for ChainCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{} chains redacted>", self.0)
    }
}

/// Bot-level knobs — thresholds and intervals.
///
/// Money values are stored as integers to avoid f64 precision and NaN hazards.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotConfig {
    /// Minimum profit threshold in USD × 1e6 (six decimals of USD).
    /// A value of `5_000_000` means `$5.00`. Fixed-point over f64 so
    /// comparisons against oracle-denominated profit are deterministic.
    pub min_profit_usd_1e6: u64,
    /// Maximum acceptable gas price, in wei (decimal string or integer).
    /// Stored as U256 so sub-gwei priority fees are representable and
    /// EIP-1559 math stays exact.
    #[serde(deserialize_with = "deser_u256_string")]
    pub max_gas_wei: U256,
    /// Polling interval for protocols that don't push events.
    pub scan_interval_ms: u64,
    /// Health factor at or below which a position becomes liquidatable,
    /// in basis points of 1e18 (10_000 = 1.0). Integer bps over f64 so
    /// the boundary has no ULP-level drift (1.05 as f64 truncates to
    /// 1_049_999_999_999_999_872 in 1e18 scale and silently leaks
    /// positions out of the NearLiquidation bucket).
    #[serde(default = "default_liquidatable_threshold_bps")]
    pub liquidatable_threshold_bps: u32,
    /// Upper bound of the near-liquidation watch band, same bps space.
    #[serde(default = "default_near_liq_threshold_bps")]
    pub near_liq_threshold_bps: u32,
    /// HOT (Liquidatable) bucket scan cadence, in blocks. Default 1.
    #[serde(default = "default_hot_scan_blocks")]
    pub hot_scan_blocks: u64,
    /// WARM (NearLiquidation) bucket scan cadence. Default every 10 blocks.
    #[serde(default = "default_warm_scan_blocks")]
    pub warm_scan_blocks: u64,
    /// COLD (Healthy) bucket scan cadence. Default every 100 blocks.
    #[serde(default = "default_cold_scan_blocks")]
    pub cold_scan_blocks: u64,
}

fn default_liquidatable_threshold_bps() -> u32 {
    10_000 // 1.0000
}
fn default_near_liq_threshold_bps() -> u32 {
    10_500 // 1.0500
}
fn default_hot_scan_blocks() -> u64 {
    1
}
fn default_warm_scan_blocks() -> u64 {
    10
}
fn default_cold_scan_blocks() -> u64 {
    100
}

/// RPC endpoints for a single chain. **The URLs typically embed API keys;
/// `Debug` prints `<redacted>` rather than the URL.**
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainConfig {
    pub chain_id: u64,
    pub ws_url: String,
    pub http_url: String,
}

impl fmt::Debug for ChainConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainConfig")
            .field("chain_id", &self.chain_id)
            .field("ws_url", &"<redacted>")
            .field("http_url", &"<redacted>")
            .finish()
    }
}

/// Address and metadata for a lending protocol on a specific chain.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolConfig {
    /// Name of the chain this protocol runs on (must match a key in `[chain]`).
    pub chain: String,
    /// Protocol's main entry point (e.g. Venus Unitroller / Comptroller).
    pub comptroller: Address,
}

/// A flash-loan source available on a given chain.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlashLoanConfig {
    pub chain: String,
    /// Pool / vault address (Aave V3 Pool, Balancer Vault, etc.).
    pub pool: Address,
}

/// Address of the deployed `CharonLiquidator` contract on a chain.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiquidatorConfig {
    pub chain: String,
    pub contract_address: Address,
}

impl Config {
    /// Read a TOML config file, substitute `${ENV_VAR}` placeholders, parse
    /// and validate.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path_buf = path.as_ref().to_path_buf();
        let raw = std::fs::read_to_string(&path_buf).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                ConfigError::NotFound(path_buf.clone())
            } else {
                ConfigError::Io {
                    path: path_buf.clone(),
                    source,
                }
            }
        })?;
        Self::from_str(&raw)
    }

    /// Parse an already-loaded TOML string (used by tests and embedded configs).
    pub fn from_str(raw: &str) -> Result<Self> {
        let substituted = substitute_env_vars(raw)?;
        let config: Config = toml::from_str(&substituted)?;
        config.validate()?;
        Ok(config)
    }

    /// Cross-reference chain keys, reject sentinel zero addresses, and
    /// sanity-check scanner bucket thresholds + cadence.
    fn validate(&self) -> Result<()> {
        if self.chain.is_empty() {
            return Err(ConfigError::Validation("no [chain.*] entries".into()));
        }
        if self.bot.near_liq_threshold_bps <= self.bot.liquidatable_threshold_bps {
            return Err(ConfigError::Validation(format!(
                "near_liq_threshold_bps ({}) must be > liquidatable_threshold_bps ({})",
                self.bot.near_liq_threshold_bps, self.bot.liquidatable_threshold_bps
            )));
        }
        if self.bot.hot_scan_blocks == 0
            || self.bot.warm_scan_blocks == 0
            || self.bot.cold_scan_blocks == 0
        {
            return Err(ConfigError::Validation(
                "hot/warm/cold_scan_blocks must all be > 0".into(),
            ));
        }
        for (name, p) in &self.protocol {
            if !self.chain.contains_key(&p.chain) {
                return Err(ConfigError::Validation(format!(
                    "protocol `{name}` references unknown chain `{}`",
                    p.chain
                )));
            }
            if p.comptroller == Address::ZERO {
                return Err(ConfigError::Validation(format!(
                    "protocol `{name}` has zero comptroller address"
                )));
            }
        }
        for (name, f) in &self.flashloan {
            if !self.chain.contains_key(&f.chain) {
                return Err(ConfigError::Validation(format!(
                    "flashloan `{name}` references unknown chain `{}`",
                    f.chain
                )));
            }
            if f.pool == Address::ZERO {
                return Err(ConfigError::Validation(format!(
                    "flashloan `{name}` has zero pool address"
                )));
            }
        }
        for (name, l) in &self.liquidator {
            if !self.chain.contains_key(&l.chain) {
                return Err(ConfigError::Validation(format!(
                    "liquidator `{name}` references unknown chain `{}`",
                    l.chain
                )));
            }
            if l.contract_address == Address::ZERO {
                return Err(ConfigError::Validation(format!(
                    "liquidator `{name}` has zero contract address — deploy the contract first"
                )));
            }
        }
        Ok(())
    }
}

fn deser_u256_string<'de, D>(d: D) -> std::result::Result<U256, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrInt {
        String(String),
        Int(u128),
    }
    match StringOrInt::deserialize(d)? {
        StringOrInt::String(s) => {
            let trimmed = s.trim();
            if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
                U256::from_str_radix(hex, 16).map_err(D::Error::custom)
            } else {
                U256::from_str_radix(trimmed, 10).map_err(D::Error::custom)
            }
        }
        StringOrInt::Int(n) => Ok(U256::from(n)),
    }
}

/// Replace every `${NAME}` or `${NAME:-default}` in `input` with the value of
/// the environment variable `NAME`. Values are escaped so that TOML-special
/// characters (`"`, `\`, newline) inside env values cannot corrupt the parse.
///
/// Values are expected to be placed inside double-quoted TOML strings.
fn substitute_env_vars(input: &str) -> Result<String> {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or(ConfigError::UnterminatedInterp)?;
        let token = &after[..end];
        let (var_name, default) = match token.split_once(":-") {
            Some((name, def)) => (name, Some(def)),
            None => (token, None),
        };
        if !is_valid_env_name(var_name) {
            return Err(ConfigError::InvalidEnvVarName(var_name.to_string()));
        }
        let value = match std::env::var(var_name) {
            Ok(v) => v,
            Err(_) => match default {
                Some(d) => d.to_string(),
                None => return Err(ConfigError::UnsetEnvVar(var_name.to_string())),
            },
        };
        for c in value.chars() {
            match c {
                '\\' => output.push_str("\\\\"),
                '"' => output.push_str("\\\""),
                '\n' => output.push_str("\\n"),
                '\r' => output.push_str("\\r"),
                '\t' => output.push_str("\\t"),
                _ => output.push(c),
            }
        }
        rest = &after[end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn is_valid_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_uppercase() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}
