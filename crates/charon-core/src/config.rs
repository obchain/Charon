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
use secrecy::SecretString;
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
    /// A chain has no `private_rpc_url` and did not opt in to
    /// public-mempool submission via `allow_public_mempool = true`.
    /// Refusing to start is deliberate: broadcasting liquidation
    /// calldata to the public mempool is a guaranteed front-run.
    #[error(
        "chain '{chain}' has no private_rpc_url; set one, or set allow_public_mempool = true to opt in (testnet/dev only)"
    )]
    PrivateRpcRequired {
        /// Chain key (matches a `[chain.<name>]` section).
        chain: String,
    },
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
    /// Chainlink feed addresses per chain, keyed by asset symbol
    /// (e.g. `chainlink.bnb.BNB = "0x…"`). Missing key = no feed
    /// configured, scanner falls back to protocol oracle.
    #[serde(default)]
    pub chainlink: HashMap<String, HashMap<String, Address>>,
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
///
/// `signer_key` is the only field whose *value* is a secret. It is stored
/// in a [`SecretString`] so the raw hex is never materialised in `Debug`
/// output; the raw bytes are exposed only at the signing site, via
/// `expose_secret()`. A hand-written `Debug` impl is provided below so the
/// field is rendered as `<redacted>` / `<unset>` instead of relying on
/// `secrecy`'s default `Secret(...)` rendering — any future change that
/// would expose the key therefore has to first delete a visible redaction
/// marker in source.
#[derive(Clone, Deserialize)]
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
    /// Hot-wallet signer key, fed in via `${CHARON_SIGNER_KEY}` env
    /// substitution in `config/default.toml`. Held in a
    /// [`SecretString`] so the raw hex never reaches `Debug` output or
    /// log lines — `expose_secret()` is called only at the signing
    /// site in the CLI pipeline, never stored back.
    ///
    /// An empty or missing value puts the bot in **scan-only** mode:
    /// the CLI pipeline refuses to build / simulate / enqueue anything
    /// that would require a signature (the simulation gate is hard —
    /// no signer → no enqueue, ever). Production runs must supply a
    /// non-empty value via the env var, never a literal in the file.
    #[serde(default, deserialize_with = "deser_optional_secret")]
    pub signer_key: Option<SecretString>,
}

impl fmt::Debug for BotConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BotConfig")
            .field("min_profit_usd_1e6", &self.min_profit_usd_1e6)
            .field("max_gas_wei", &self.max_gas_wei)
            .field("scan_interval_ms", &self.scan_interval_ms)
            .field("liquidatable_threshold_bps", &self.liquidatable_threshold_bps)
            .field("near_liq_threshold_bps", &self.near_liq_threshold_bps)
            .field("hot_scan_blocks", &self.hot_scan_blocks)
            .field("warm_scan_blocks", &self.warm_scan_blocks)
            .field("cold_scan_blocks", &self.cold_scan_blocks)
            .field(
                "signer_key",
                &if self.signer_key.is_some() {
                    "<redacted>"
                } else {
                    "<unset>"
                },
            )
            .finish()
    }
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
///
/// `priority_fee_gwei` is the EIP-1559 priority fee (tip) the gas
/// oracle attaches per chain, expressed in gwei for operator
/// readability. Defaults to `1` — BSC validators are fine with a
/// 1 gwei tip; L2s running sub-gwei tips should override explicitly.
/// The oracle converts to wei internally; mixing units silently
/// under-filters at 1e9×.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainConfig {
    pub chain_id: u64,
    pub ws_url: String,
    pub http_url: String,
    #[serde(default = "default_priority_fee_gwei")]
    pub priority_fee_gwei: u64,
    /// Optional private-RPC endpoint for transaction submission
    /// (bloxroute / blocknative on BSC, sequencer endpoints on L2s).
    /// When set, the submitter posts `eth_sendRawTransaction` here
    /// instead of the public `http_url`, so pending txs skip the
    /// public mempool and front-runners.
    ///
    /// Held in a [`SecretString`] because vendor URLs typically embed
    /// an API key in the path (e.g. `https://.../?auth=<key>`). Call
    /// `expose_secret()` only at the single point of use (the
    /// submitter); never log the raw string.
    ///
    /// An empty env-substituted string is treated as `None`, so
    /// `CHARON_<CHAIN>_PRIVATE_RPC_URL=` in `.env` produces an unset
    /// endpoint (caught by validation unless `allow_public_mempool`
    /// is set) rather than a nonsense empty-URL submitter.
    #[serde(default, deserialize_with = "deser_optional_secret")]
    pub private_rpc_url: Option<SecretString>,
    /// Optional bearer token for the private RPC. Attached verbatim
    /// as `Authorization: Bearer <token>`. Use this when the vendor
    /// prefers a header over a URL-embedded key. Loaded from
    /// `CHARON_<CHAIN>_PRIVATE_RPC_AUTH` via env substitution. Empty
    /// string = unset.
    #[serde(default, deserialize_with = "deser_optional_secret")]
    pub private_rpc_auth: Option<SecretString>,
    /// Escape hatch for local / testnet runs where no private RPC
    /// exists. When `false` (the default) [`Config::validate`]
    /// refuses to start a chain with no `private_rpc_url`, because
    /// submitting liquidation calldata to the public mempool is a
    /// guaranteed front-run. NEVER enable on mainnet.
    #[serde(default)]
    pub allow_public_mempool: bool,
}

fn default_priority_fee_gwei() -> u64 {
    1
}

impl fmt::Debug for ChainConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChainConfig")
            .field("chain_id", &self.chain_id)
            .field("ws_url", &"<redacted>")
            .field("http_url", &"<redacted>")
            .field("priority_fee_gwei", &self.priority_fee_gwei)
            .field(
                "private_rpc_url",
                &if self.private_rpc_url.is_some() {
                    "<redacted>"
                } else {
                    "<unset>"
                },
            )
            .field(
                "private_rpc_auth",
                &if self.private_rpc_auth.is_some() {
                    "<redacted>"
                } else {
                    "<unset>"
                },
            )
            .field("allow_public_mempool", &self.allow_public_mempool)
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
    /// Optional auxiliary data-provider address used by some sources
    /// to resolve per-asset state (e.g. Aave V3 `PoolDataProvider`
    /// for aToken lookup and reserve configuration bitmaps). `None`
    /// for sources that don't need one (Balancer, Uniswap).
    #[serde(default)]
    pub data_provider: Option<Address>,
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
    /// sanity-check scanner bucket thresholds + cadence. Also enforces
    /// the private-mempool gate: every chain must either carry a
    /// `private_rpc_url` or opt in to `allow_public_mempool`.
    ///
    /// Called from `from_str` on every load, and additionally exposed
    /// for callers (CLI) that want an explicit belt-and-braces check
    /// after any programmatic override.
    pub fn validate(&self) -> Result<()> {
        if self.chain.is_empty() {
            return Err(ConfigError::Validation("no [chain.*] entries".into()));
        }
        // Private-mempool gate: every configured chain must either carry
        // a `private_rpc_url` or explicitly opt in to the public mempool
        // via `allow_public_mempool = true`. Applying the check per
        // chain (rather than only per deployed liquidator) means a
        // misconfigured chain can never fall back to public broadcast
        // later in the pipeline.
        for (name, c) in &self.chain {
            if c.private_rpc_url.is_none() && !c.allow_public_mempool {
                return Err(ConfigError::PrivateRpcRequired {
                    chain: name.clone(),
                });
            }
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

/// Treat an empty string as "unset" and return `None`. Non-empty strings
/// become `Some(SecretString)` so the env substitution form
/// `${CHARON_SIGNER_KEY:-}` (env-optional secret) flows naturally from
/// the env-var layer to the typed config without the caller having to
/// distinguish "missing" from "explicitly empty".
fn deser_optional_secret<'de, D>(d: D) -> std::result::Result<Option<SecretString>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = Option::<String>::deserialize(d)?;
    Ok(match s {
        Some(v) if !v.trim().is_empty() => Some(SecretString::from(v)),
        _ => None,
    })
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

#[cfg(test)]
mod private_rpc_tests {
    //! Tests for the private-RPC gate and secret redaction on
    //! `ChainConfig`. These tests are isolated from the file-loading
    //! `substitute_env_vars` path so they do not race with any other
    //! `std::env::set_var` usage in the crate.

    use super::*;
    use secrecy::ExposeSecret;

    fn chain_cfg(private_rpc: Option<&str>, allow_public: bool) -> ChainConfig {
        ChainConfig {
            chain_id: 56,
            ws_url: "wss://example/ws".into(),
            http_url: "https://example/http".into(),
            priority_fee_gwei: 1,
            private_rpc_url: private_rpc.map(|s| SecretString::from(s.to_string())),
            private_rpc_auth: None,
            allow_public_mempool: allow_public,
        }
    }

    fn base(chain: ChainConfig) -> Config {
        let mut chains = HashMap::new();
        chains.insert("bnb".to_string(), chain);
        Config {
            bot: BotConfig {
                min_profit_usd_1e6: 5_000_000,
                max_gas_wei: U256::from(3_000_000_000u64),
                scan_interval_ms: 1000,
                liquidatable_threshold_bps: 10_000,
                near_liq_threshold_bps: 10_500,
                hot_scan_blocks: 1,
                warm_scan_blocks: 10,
                cold_scan_blocks: 100,
                signer_key: None,
            },
            chain: chains,
            protocol: HashMap::new(),
            flashloan: HashMap::new(),
            liquidator: HashMap::new(),
            chainlink: HashMap::new(),
        }
    }

    #[test]
    fn validate_rejects_chain_without_private_rpc() {
        let cfg = base(chain_cfg(None, false));
        let err = cfg.validate().expect_err("must refuse public mempool");
        match err {
            ConfigError::PrivateRpcRequired { chain } => assert_eq!(chain, "bnb"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_allows_public_mempool_opt_in() {
        let cfg = base(chain_cfg(None, true));
        cfg.validate().expect("opt-in must be honoured");
    }

    #[test]
    fn validate_passes_with_private_rpc_configured() {
        let cfg = base(chain_cfg(Some("https://private.example"), false));
        cfg.validate().expect("private rpc present -> valid");
    }

    #[test]
    fn debug_redacts_private_rpc_url_and_auth() {
        let mut c = chain_cfg(Some("https://key.example/?auth=SUPER_SECRET_KEY"), false);
        c.private_rpc_auth = Some(SecretString::from("SECRET_TOKEN".to_string()));
        let dbg = format!("{c:?}");
        assert!(
            !dbg.contains("SUPER_SECRET_KEY"),
            "private_rpc_url leaked: {dbg}"
        );
        assert!(!dbg.contains("SECRET_TOKEN"), "auth token leaked: {dbg}");
        assert!(
            dbg.contains("<redacted>"),
            "redaction marker missing: {dbg}"
        );
    }

    #[test]
    fn deser_treats_empty_private_rpc_as_unset() {
        // Simulates the env-substitution result when
        // `CHARON_BSC_PRIVATE_RPC_URL=` is blank: the string reaches
        // serde as `""`, which must collapse to `None` so the
        // `PrivateRpcRequired` gate fires instead of constructing a
        // bogus empty-URL submitter.
        let toml_src = r#"
            chain_id = 56
            ws_url = "wss://x/y"
            http_url = "https://x/y"
            private_rpc_url = ""
            private_rpc_auth = ""
            allow_public_mempool = true
        "#;
        let c: ChainConfig = toml::from_str(toml_src).expect("parse");
        assert!(c.private_rpc_url.is_none());
        assert!(c.private_rpc_auth.is_none());
    }

    #[test]
    fn deser_keeps_non_empty_private_rpc() {
        let toml_src = r#"
            chain_id = 56
            ws_url = "wss://x/y"
            http_url = "https://x/y"
            private_rpc_url = "https://priv.example/rpc"
        "#;
        let c: ChainConfig = toml::from_str(toml_src).expect("parse");
        let url = c.private_rpc_url.expect("url present");
        assert_eq!(url.expose_secret(), "https://priv.example/rpc");
    }
}
