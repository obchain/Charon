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
    pub flashloan: HashMap<String, FlashLoanConfig>,
    /// Deployed liquidator contracts keyed by chain name.
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
    /// Bind address for the `/metrics` HTTP listener. Defaults to
    /// `127.0.0.1:9091` so the endpoint is unreachable from the public
    /// internet on a bare VPS. Non-loopback binds must pair with a
    /// `auth_token` (enforced by [`MetricsConfig::validate`]).
    #[serde(default = "default_metrics_bind")]
    pub bind: SocketAddr,
    /// Shared secret expected on `Authorization: Bearer <token>` when
    /// the exporter is reached over a non-loopback bind. The exporter
    /// itself does not yet terminate auth — the token is enforced by
    /// the reverse proxy (nginx, caddy, etc.) that sits in front of
    /// `bind`. Holding the value in config makes the proxy + bot share
    /// one source of truth.
    #[serde(default)]
    pub auth_token: Option<String>,
}

impl MetricsConfig {
    /// Refuse to start when the exporter is bound to a non-loopback
    /// address without an accompanying `auth_token`. Stops silent
    /// deployment of an unauthenticated `/metrics` endpoint to any
    /// caller with network reach — see tracking issues #213 #214.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let ip = self.bind.ip();
        if !ip.is_loopback()
            && self
                .auth_token
                .as_deref()
                .map(str::is_empty)
                .unwrap_or(true)
        {
            return Err(anyhow!(
                "metrics.bind {} is non-loopback but metrics.auth_token is empty — \
                 either bind to 127.0.0.1 (scrape via reverse proxy / VPN) or set \
                 CHARON_METRICS_AUTH_TOKEN and front the exporter with a proxy that \
                 enforces Authorization: Bearer on /metrics",
                self.bind
            ));
        }
        Ok(())
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: default_metrics_enabled(),
            bind: default_metrics_bind(),
            auth_token: None,
        }
    }
}

fn default_metrics_enabled() -> bool {
    true
}

fn default_metrics_bind() -> SocketAddr {
    "127.0.0.1:9091"
        .parse()
        .expect("valid default metrics bind")
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
        config
            .metrics
            .validate()
            .with_context(|| format!("invalid [metrics] section in {}", path.display()))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Loopback bind is safe on its own — no auth token required,
    /// because the endpoint is unreachable off-box.
    #[test]
    fn validate_allows_loopback_without_token() {
        let cfg = MetricsConfig {
            enabled: true,
            bind: "127.0.0.1:9091".parse().unwrap(),
            auth_token: None,
        };
        cfg.validate().expect("loopback + no token must pass");

        let cfg_v6 = MetricsConfig {
            enabled: true,
            bind: "[::1]:9091".parse().unwrap(),
            auth_token: None,
        };
        cfg_v6.validate().expect("IPv6 loopback must pass");
    }

    /// Non-loopback bind with a non-empty token is the documented
    /// "front with a reverse proxy" escape hatch.
    #[test]
    fn validate_allows_non_loopback_with_token() {
        let cfg = MetricsConfig {
            enabled: true,
            bind: "0.0.0.0:9091".parse().unwrap(),
            auth_token: Some("not-a-real-token".into()),
        };
        cfg.validate()
            .expect("non-loopback + token must pass (proxy enforces)");
    }

    /// Non-loopback with missing or empty token must fail — covers
    /// both `auth_token = None` (unset in TOML) and `auth_token =
    /// Some("")` (the nasty case where `CHARON_METRICS_AUTH_TOKEN=`
    /// is exported empty and env substitution silently yields a
    /// blank string). This is the regression gate for #213/#214.
    #[test]
    fn validate_rejects_non_loopback_without_token() {
        let none_cfg = MetricsConfig {
            enabled: true,
            bind: "0.0.0.0:9091".parse().unwrap(),
            auth_token: None,
        };
        assert!(none_cfg.validate().is_err(), "None token must fail");

        let empty_cfg = MetricsConfig {
            enabled: true,
            bind: "0.0.0.0:9091".parse().unwrap(),
            auth_token: Some(String::new()),
        };
        assert!(empty_cfg.validate().is_err(), "empty token must fail");
    }

    /// `enabled = false` bypasses validation: a disabled exporter
    /// never opens a socket, so bind/token combinations are moot.
    #[test]
    fn validate_skipped_when_disabled() {
        let cfg = MetricsConfig {
            enabled: false,
            bind: "0.0.0.0:9091".parse().unwrap(),
            auth_token: None,
        };
        cfg.validate()
            .expect("disabled exporter must skip bind checks");
    }
}
