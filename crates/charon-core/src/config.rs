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
    pub flashloan: HashMap<String, FlashLoanConfig>,
    /// Deployed liquidator contracts keyed by chain name.
    pub liquidator: HashMap<String, LiquidatorConfig>,
    /// Chainlink feed addresses per chain, keyed by asset symbol
    /// (e.g. `chainlink.bnb.BNB = "0x…"`). Missing key = no feed
    /// configured, scanner falls back to protocol oracle.
    #[serde(default)]
    pub chainlink: HashMap<String, HashMap<String, Address>>,
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
        Ok(config)
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
