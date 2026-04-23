//! TOML config loader with `${ENV_VAR}` substitution for secrets.
//!
//! Usage:
//! ```no_run
//! use charon_core::config::Config;
//! let cfg = Config::load("config/default.toml").unwrap();
//! ```

use alloy::primitives::Address;
use anyhow::{Context, anyhow};
use secrecy::{ExposeSecret, SecretString};
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
///
/// `Debug` is implemented manually so the private-RPC URL and auth
/// token are redacted — both may embed API keys and must never reach
/// `tracing` output, panic traces, or crash dumps.
#[derive(Clone, Deserialize)]
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
    ///
    /// Stored in a [`SecretString`] because vendor URLs often embed
    /// an API key in the path (e.g. `https://.../?auth=<key>`). Call
    /// `expose_secret()` only at the single point of use (the
    /// submitter); never log the raw string.
    #[serde(default)]
    pub private_rpc_url: Option<SecretString>,
    /// Optional bearer token for the private RPC. Attached verbatim
    /// as `Authorization: Bearer <token>`. Use this when the vendor
    /// prefers a header over a URL-embedded key. Loaded from
    /// `CHARON_<CHAIN>_PRIVATE_RPC_AUTH` via env substitution.
    #[serde(default)]
    pub private_rpc_auth: Option<SecretString>,
    /// Escape hatch for local / testnet runs where no private RPC
    /// exists. When `false` (the default) [`Config::validate`]
    /// refuses to start a chain that has a deployed liquidator but
    /// no `private_rpc_url`, because submitting liquidation calldata
    /// to the public mempool is a guaranteed front-run.
    #[serde(default)]
    pub allow_public_mempool: bool,
}

impl std::fmt::Debug for ChainConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainConfig")
            .field("chain_id", &self.chain_id)
            .field("ws_url", &self.ws_url)
            .field("http_url", &self.http_url)
            .field("priority_fee_gwei", &self.priority_fee_gwei)
            .field(
                "private_rpc_url",
                &self.private_rpc_url.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "private_rpc_auth",
                &self.private_rpc_auth.as_ref().map(|_| "<redacted>"),
            )
            .field("allow_public_mempool", &self.allow_public_mempool)
            .finish()
    }
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

/// Errors surfaced by [`Config::validate`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// A chain has a deployed liquidator but no `private_rpc_url`
    /// and did not opt in to public-mempool submission. Refusing to
    /// start is deliberate: broadcasting liquidation calldata to the
    /// public mempool reliably loses to front-runners.
    #[error(
        "chain '{chain}' has a deployed liquidator but no private_rpc_url; set one, or set allow_public_mempool = true to opt in (testnet/dev only)"
    )]
    PrivateRpcRequired {
        /// Chain key (matches a `[chain.<name>]` section).
        chain: String,
    },
    /// A chain has a `[liquidator.<chain>]` entry whose
    /// `contract_address` is the zero address. Starting a live-mempool
    /// run with this config would route `eth_call` simulations and
    /// every flashloan callback to `address(0)`: the call returns empty
    /// bytes (no revert), which the simulator interprets as a pass,
    /// producing a silent false-positive gate. Allowed only when the
    /// chain explicitly opts in to public-mempool submission
    /// (`allow_public_mempool = true`) for local anvil / testnet runs
    /// where a real deploy does not yet exist.
    #[error(
        "chain '{chain}' has liquidator.contract_address = 0x0; deploy CharonLiquidator and set the real address, or set allow_public_mempool = true to opt in (testnet/dev only)"
    )]
    ZeroAddressLiquidator {
        /// Chain key (matches a `[chain.<name>]` section).
        chain: String,
    },
    /// `liquidatable_threshold` must not exceed `near_liq_threshold`.
    #[error("liquidatable_threshold ({liquidatable}) must be <= near_liq_threshold ({near_liq})")]
    ThresholdInversion { liquidatable: f64, near_liq: f64 },
}

impl Config {
    /// Read a TOML config file, substitute `${ENV_VAR}` placeholders, parse.
    ///
    /// Returns an error if the file is missing, malformed, or references an
    /// environment variable that isn't set.
    ///
    /// Does **not** run [`Self::validate`] — callers decide when invariant
    /// checks fire (e.g. after a dev-only `allow_public_mempool` override).
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config at {}", path.display()))?;
        let substituted = substitute_env_vars(&raw)
            .with_context(|| format!("env substitution failed for {}", path.display()))?;
        let mut config: Config = toml::from_str(&substituted)
            .with_context(|| format!("failed to parse TOML at {}", path.display()))?;
        config.normalize_empty_secrets();
        Ok(config)
    }

    /// Collapse empty `SecretString` values to `None` on every chain.
    ///
    /// `substitute_env_vars` replaces a `${VAR}` placeholder with the
    /// literal env-var value. When an operator leaves
    /// `CHARON_BSC_PRIVATE_RPC_AUTH=` empty in `.env`, the TOML field
    /// becomes `""` and serde deserializes it as `Some(SecretString(""))`
    /// — not `None`. Downstream that becomes a blank `Authorization:
    /// Bearer ` header and a misleading pass from
    /// [`Self::validate`]'s `is_none()` check on `private_rpc_url`. This
    /// normalizer runs once after load so the rest of the codebase can
    /// trust `None` to mean "unset".
    fn normalize_empty_secrets(&mut self) {
        for chain_cfg in self.chain.values_mut() {
            if chain_cfg
                .private_rpc_url
                .as_ref()
                .is_some_and(|s| s.expose_secret().is_empty())
            {
                chain_cfg.private_rpc_url = None;
            }
            if chain_cfg
                .private_rpc_auth
                .as_ref()
                .is_some_and(|s| s.expose_secret().is_empty())
            {
                chain_cfg.private_rpc_auth = None;
            }
        }
    }

    /// Enforce cross-section invariants. Call after [`Self::load`] and
    /// before spawning any submitters.
    ///
    /// Current checks:
    /// - Every `[liquidator.<chain>]` has a `[chain.<chain>]` with a
    ///   `private_rpc_url`, unless that chain set
    ///   `allow_public_mempool = true`.
    /// - Every `[liquidator.<chain>]` has a non-zero
    ///   `contract_address`, unless that chain set
    ///   `allow_public_mempool = true` (dev/testnet escape hatch).
    /// - `liquidatable_threshold <= near_liq_threshold`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for liq in self.liquidator.values() {
            let chain_cfg = match self.chain.get(&liq.chain) {
                Some(c) => c,
                // Missing [chain.<name>] section is a data-model error
                // caught elsewhere; skip here so we surface the
                // private-RPC rule cleanly.
                None => continue,
            };
            if chain_cfg.private_rpc_url.is_none() && !chain_cfg.allow_public_mempool {
                return Err(ConfigError::PrivateRpcRequired {
                    chain: liq.chain.clone(),
                });
            }
            if liq.contract_address == Address::ZERO && !chain_cfg.allow_public_mempool {
                return Err(ConfigError::ZeroAddressLiquidator {
                    chain: liq.chain.clone(),
                });
            }
        }
        if self.bot.liquidatable_threshold > self.bot.near_liq_threshold {
            return Err(ConfigError::ThresholdInversion {
                liquidatable: self.bot.liquidatable_threshold,
                near_liq: self.bot.near_liq_threshold,
            });
        }
        Ok(())
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

    fn chain(private_rpc: Option<&str>, allow_public: bool) -> ChainConfig {
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

    /// Non-zero sentinel address used by tests that are not exercising
    /// the zero-address rule. Keeps the default `base_config` valid
    /// after the `ZeroAddressLiquidator` check landed.
    fn nonzero_liquidator() -> Address {
        Address::from([0x11; 20])
    }

    fn base_config(chain_cfg: ChainConfig, liquidator: Option<Address>) -> Config {
        let mut chains = HashMap::new();
        chains.insert("bnb".to_string(), chain_cfg);
        let mut liquidators = HashMap::new();
        if let Some(addr) = liquidator {
            liquidators.insert(
                "bnb".to_string(),
                LiquidatorConfig {
                    chain: "bnb".to_string(),
                    contract_address: addr,
                },
            );
        }
        Config {
            bot: BotConfig {
                min_profit_usd: 1.0,
                max_gas_gwei: 10,
                scan_interval_ms: 1000,
                liquidatable_threshold: 1.0,
                near_liq_threshold: 1.05,
            },
            chain: chains,
            protocol: HashMap::new(),
            flashloan: HashMap::new(),
            liquidator: liquidators,
            chainlink: HashMap::new(),
        }
    }

    #[test]
    fn validate_rejects_liquidator_without_private_rpc() {
        let cfg = base_config(chain(None, false), Some(nonzero_liquidator()));
        let err = cfg.validate().expect_err("must refuse public mempool");
        match err {
            ConfigError::PrivateRpcRequired { chain } => assert_eq!(chain, "bnb"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_allows_public_mempool_opt_in() {
        let cfg = base_config(chain(None, true), Some(nonzero_liquidator()));
        cfg.validate().expect("opt-in must be honoured");
    }

    #[test]
    fn validate_passes_with_private_rpc_configured() {
        let cfg = base_config(chain(Some("https://private.example"), false), Some(nonzero_liquidator()));
        cfg.validate().expect("private rpc present -> valid");
    }

    #[test]
    fn validate_ignores_chains_without_liquidator() {
        // A chain with no deployed liquidator has nothing to submit,
        // so the private-rpc requirement does not apply. Validation
        // must not trip on it.
        let cfg = base_config(chain(None, false), None);
        cfg.validate().expect("no liquidator -> no private-rpc req");
    }

    #[test]
    fn validate_rejects_threshold_inversion() {
        let mut cfg = base_config(chain(Some("https://p"), false), Some(nonzero_liquidator()));
        cfg.bot.liquidatable_threshold = 1.1;
        cfg.bot.near_liq_threshold = 1.0;
        let err = cfg.validate().expect_err("inverted thresholds rejected");
        assert!(matches!(err, ConfigError::ThresholdInversion { .. }));
    }

    #[test]
    fn normalize_collapses_empty_private_rpc_auth_to_none() {
        let mut c = chain(Some("https://private.example"), false);
        c.private_rpc_auth = Some(SecretString::from(String::new()));
        let mut cfg = base_config(c, Some(nonzero_liquidator()));
        cfg.normalize_empty_secrets();
        let got = cfg.chain.get("bnb").expect("chain present");
        assert!(
            got.private_rpc_auth.is_none(),
            "empty auth must collapse to None"
        );
    }

    #[test]
    fn normalize_collapses_empty_private_rpc_url_to_none() {
        let mut c = chain(Some(""), false);
        c.private_rpc_auth = None;
        let mut cfg = base_config(c, Some(nonzero_liquidator()));
        cfg.normalize_empty_secrets();
        let got = cfg.chain.get("bnb").expect("chain present");
        assert!(
            got.private_rpc_url.is_none(),
            "empty url must collapse to None"
        );
    }

    #[test]
    fn normalize_preserves_non_empty_secrets() {
        let mut c = chain(Some("https://private.example"), false);
        c.private_rpc_auth = Some(SecretString::from("token".to_string()));
        let mut cfg = base_config(c, Some(nonzero_liquidator()));
        cfg.normalize_empty_secrets();
        let got = cfg.chain.get("bnb").expect("chain present");
        assert!(got.private_rpc_url.is_some(), "url must be preserved");
        assert!(got.private_rpc_auth.is_some(), "auth must be preserved");
    }

    #[test]
    fn normalize_walks_every_chain_independently() {
        let empty = chain(Some(""), false);
        let set = chain(Some("https://private.example"), false);
        let mut cfg = base_config(empty, Some(nonzero_liquidator()));
        cfg.chain.insert("l2".to_string(), set);
        cfg.normalize_empty_secrets();
        assert!(
            cfg.chain.get("bnb").unwrap().private_rpc_url.is_none(),
            "empty chain must collapse"
        );
        assert!(
            cfg.chain.get("l2").unwrap().private_rpc_url.is_some(),
            "non-empty chain must survive"
        );
    }

    #[test]
    fn normalize_then_validate_rejects_empty_url_without_opt_in() {
        // Together with normalize_empty_secrets, validate() must now
        // refuse a chain that had an empty `${VAR}` substitution for
        // its private_rpc_url and did not opt in to public mempool.
        let c = chain(Some(""), false);
        let mut cfg = base_config(c, Some(nonzero_liquidator()));
        cfg.normalize_empty_secrets();
        let err = cfg
            .validate()
            .expect_err("empty-substituted url must fail validate()");
        assert!(matches!(err, ConfigError::PrivateRpcRequired { .. }));
    }

    #[test]
    fn validate_rejects_zero_address_liquidator_without_opt_in() {
        let cfg = base_config(
            chain(Some("https://private.example"), false),
            Some(Address::ZERO),
        );
        let err = cfg
            .validate()
            .expect_err("zero-address liquidator must be rejected");
        match err {
            ConfigError::ZeroAddressLiquidator { chain } => assert_eq!(chain, "bnb"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn validate_allows_zero_address_with_public_mempool_opt_in() {
        // Dev / testnet runs before a real deploy: zero-address
        // liquidator is tolerated when the operator explicitly opts in
        // to public-mempool submission.
        let cfg = base_config(
            chain(Some("https://private.example"), true),
            Some(Address::ZERO),
        );
        cfg.validate()
            .expect("zero-addr + opt-in must pass (dev mode)");
    }

    #[test]
    fn validate_private_rpc_check_fires_before_zero_address_check() {
        // Both checks gate on !allow_public_mempool; the private-RPC
        // rule is listed first in the loop so operators see the more
        // actionable error first. Lock that ordering.
        let cfg = base_config(chain(None, false), Some(Address::ZERO));
        let err = cfg
            .validate()
            .expect_err("either check could fire; assert order");
        assert!(
            matches!(err, ConfigError::PrivateRpcRequired { .. }),
            "expected PrivateRpcRequired first, got {err:?}"
        );
    }

    #[test]
    fn debug_redacts_private_rpc_url_and_auth() {
        let mut c = chain(Some("https://key.example/?auth=SUPER_SECRET_KEY"), false);
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
}
