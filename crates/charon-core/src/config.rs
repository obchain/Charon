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
use std::net::SocketAddr;
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
    /// A `profile_tag = "fork"` profile has a chain whose `ws_url` /
    /// `http_url` does not resolve to a loopback host. The fork
    /// profile ships an intentionally lowered profit gate tuned for
    /// local demo staging; pointing it at a non-loopback endpoint
    /// (mainnet, testnet, or a remote RPC) would fire liquidations
    /// against real state at that gate. Refused at load time —
    /// operator must either flip back to `config/default.toml` or
    /// point the fork profile at the local anvil it was built for.
    #[error(
        "profile_tag=\"fork\" is a local-only profile and must point every chain's ws_url/http_url at loopback; got chain.{chain}.{field}={url}. Refusing to start with a lowered profit gate against non-loopback RPC."
    )]
    ForkProfileNonLoopbackRpc {
        /// Chain key (matches a `[chain.<name>]` section).
        chain: String,
        /// Which URL field failed the loopback check — `ws_url` or `http_url`.
        field: &'static str,
        /// The offending URL (post env-substitution) so the operator
        /// sees exactly what to fix in the TOML.
        url: String,
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
    ///
    /// `#[serde(default)]` so profiles targeting chains with no
    /// flash-loan venue (e.g. BSC testnet / Chapel, where Aave V3 is
    /// not deployed) can omit the section entirely. When empty, the
    /// off-chain pipeline short-circuits at the router gate: the
    /// scanner still runs, but no opportunity is enqueued because
    /// [`FlashLoanRouter::route`] has no source to quote. Mainnet
    /// profiles continue to populate this section in the usual way;
    /// the default does not relax any mainnet invariant.
    #[serde(default)]
    pub flashloan: HashMap<String, FlashLoanConfig>,
    /// Deployed liquidator contracts keyed by chain name.
    ///
    /// `#[serde(default)]` so profiles without a deployed liquidator
    /// (testnet, or mainnet pre-deploy) can omit the section without
    /// wedging the loader. Absence forces read-only mode: the CLI
    /// refuses to build a `TxBuilder` because it has no receiver
    /// address, so no calldata is signed or simulated.
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
/// `#[serde(deny_unknown_fields)]` makes typos in
/// `config/default.toml` a hard load-time error — a stray
/// `metrics.bindd = …` used to be silently ignored, leaving the
/// exporter on its default loopback bind while the operator
/// assumed the override took effect. `#[non_exhaustive]` reserves
/// room to add fields (e.g. TLS, scrape-path override) without a
/// breaking semver bump; external callers must construct via
/// [`MetricsConfig::default`] and mutate the fields they care
/// about rather than using a struct literal.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
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
    pub fn validate(&self) -> Result<()> {
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
            return Err(ConfigError::Validation(format!(
                "metrics.bind {} is non-loopback but metrics.auth_token is empty — \
                 either bind to 127.0.0.1 (scrape via reverse proxy / VPN) or set \
                 CHARON_METRICS_AUTH_TOKEN and front the exporter with a proxy that \
                 enforces Authorization: Bearer on /metrics",
                self.bind
            )));
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
    /// Optional profile marker used by [`Config::validate`] to enforce
    /// profile-specific invariants at startup. Known tags:
    ///
    /// - `Some("fork")` — marks `config/fork.toml`, a local-only
    ///   profile that must target loopback RPCs because its profit
    ///   gate is intentionally lowered for demo staging. Any chain
    ///   with a non-loopback `ws_url` / `http_url` is rejected at
    ///   load time ([`ConfigError::ForkProfileNonLoopbackRpc`]).
    /// - `None` / `Some("mainnet")` / `Some("testnet")` — production
    ///   profiles; no additional relaxations or invariants.
    ///
    /// Unknown tags parse without error (forward-compat) but carry no
    /// semantics until a new match arm is wired into `validate()`. The
    /// tag is not a secret — operators see it in `Debug` output so a
    /// misconfigured profile is obvious at a glance.
    #[serde(default)]
    pub profile_tag: Option<String>,
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
            .field("profile_tag", &self.profile_tag)
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
        let is_fork = self.is_fork_profile();
        // Fork-profile loopback gate runs BEFORE the mainnet-oriented
        // private-mempool gate: pointing a lowered-profit fork profile
        // at a remote RPC is a bigger footgun than a missing private
        // RPC, and the error copy is more actionable.
        if is_fork {
            for (name, c) in &self.chain {
                for (field, url) in [
                    ("ws_url", c.ws_url.as_str()),
                    ("http_url", c.http_url.as_str()),
                ] {
                    if !is_loopback_url(url) {
                        return Err(ConfigError::ForkProfileNonLoopbackRpc {
                            chain: name.clone(),
                            field,
                            url: url.to_string(),
                        });
                    }
                }
            }
        }
        // Private-mempool gate: every configured chain must either carry
        // a `private_rpc_url` or explicitly opt in to the public mempool
        // via `allow_public_mempool = true`. Applying the check per
        // chain (rather than only per deployed liquidator) means a
        // misconfigured chain can never fall back to public broadcast
        // later in the pipeline. The fork profile bypasses this gate
        // entirely: the loopback invariant above already confines
        // submission to the local anvil, and there is no public
        // mempool on a local fork to front-run into.
        if !is_fork {
            for (name, c) in &self.chain {
                if c.private_rpc_url.is_none() && !c.allow_public_mempool {
                    return Err(ConfigError::PrivateRpcRequired {
                        chain: name.clone(),
                    });
                }
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
        // Metrics exporter: loopback binds are always safe; non-loopback
        // binds must carry a non-empty `auth_token` so an operator
        // never leaks an unauthenticated `/metrics` endpoint onto the
        // public internet (see issues #213 / #214 on feat/22).
        self.metrics.validate()?;
        Ok(())
    }

    /// Return `true` when this config represents the local anvil fork
    /// profile (`config/fork.toml`). The check is intentionally narrow
    /// — only the literal `"fork"` tag flips the relaxations in
    /// [`Config::validate`]. Production profiles (`None`,
    /// `Some("mainnet")`, `Some("testnet")`) never trigger the fork
    /// branches.
    fn is_fork_profile(&self) -> bool {
        matches!(self.bot.profile_tag.as_deref(), Some("fork"))
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
mod metrics_tests {
    //! Tests for `MetricsConfig::validate`, graft from feat/22. These
    //! exercise the non-loopback/auth-token gate (#213 / #214) that
    //! blocks deploy-time leaks of an unauthenticated `/metrics`
    //! endpoint, and the `enabled = false` bypass.

    use super::*;

    /// Loopback bind is safe on its own — no auth token required,
    /// because the endpoint is unreachable off-box.
    #[test]
    fn validate_allows_loopback_without_token() {
        let cfg = MetricsConfig {
            enabled: true,
            bind: "127.0.0.1:9091".parse().expect("loopback parse"),
            auth_token: None,
        };
        cfg.validate().expect("loopback + no token must pass");

        let cfg_v6 = MetricsConfig {
            enabled: true,
            bind: "[::1]:9091".parse().expect("loopback v6 parse"),
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
            bind: "0.0.0.0:9091".parse().expect("non-loopback parse"),
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
            bind: "0.0.0.0:9091".parse().expect("non-loopback parse"),
            auth_token: None,
        };
        assert!(none_cfg.validate().is_err(), "None token must fail");

        let empty_cfg = MetricsConfig {
            enabled: true,
            bind: "0.0.0.0:9091".parse().expect("non-loopback parse"),
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
            bind: "0.0.0.0:9091".parse().expect("non-loopback parse"),
            auth_token: None,
        };
        cfg.validate()
            .expect("disabled exporter must skip bind checks");
    }
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
                profile_tag: None,
            },
            chain: chains,
            protocol: HashMap::new(),
            flashloan: HashMap::new(),
            liquidator: HashMap::new(),
            chainlink: HashMap::new(),
            metrics: MetricsConfig::default(),
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

#[cfg(test)]
mod fork_profile_tests {
    //! Tests for the `profile_tag = "fork"` branch of
    //! [`Config::validate`]. Exercise both directions:
    //! - loopback URLs pass and bypass the private-RPC gate (so
    //!   `fork.toml` can omit `allow_public_mempool` and
    //!   `private_rpc_url`);
    //! - non-loopback URLs fail with
    //!   [`ConfigError::ForkProfileNonLoopbackRpc`] naming the
    //!   offending chain + field.
    //!
    //! A parallel `url` helper block covers [`is_loopback_url`] across
    //! every common RPC-URL shape the loader sees in practice.

    use super::*;

    fn fork_chain(ws: &str, http: &str) -> ChainConfig {
        ChainConfig {
            chain_id: 56,
            ws_url: ws.into(),
            http_url: http.into(),
            priority_fee_gwei: 1,
            private_rpc_url: None,
            private_rpc_auth: None,
            allow_public_mempool: false,
        }
    }

    fn fork_cfg(chain: ChainConfig) -> Config {
        let mut chains = HashMap::new();
        chains.insert("bnb".to_string(), chain);
        Config {
            bot: BotConfig {
                min_profit_usd_1e6: 10_000, // $0.01 — lowered for fork
                max_gas_wei: U256::from(20_000_000_000u64),
                scan_interval_ms: 1000,
                liquidatable_threshold_bps: 10_000,
                near_liq_threshold_bps: 10_500,
                hot_scan_blocks: 1,
                warm_scan_blocks: 10,
                cold_scan_blocks: 100,
                signer_key: None,
                profile_tag: Some("fork".into()),
            },
            chain: chains,
            protocol: HashMap::new(),
            flashloan: HashMap::new(),
            liquidator: HashMap::new(),
            chainlink: HashMap::new(),
            metrics: MetricsConfig::default(),
        }
    }

    #[test]
    fn fork_profile_allows_loopback_without_private_rpc() {
        // Mirrors the shape shipped in `config/fork.toml`: loopback
        // URLs, no `private_rpc_url`, no `allow_public_mempool` opt-in.
        // The fork-profile branch in validate() must bypass the
        // PrivateRpcRequired gate entirely.
        let cfg = fork_cfg(fork_chain("ws://127.0.0.1:8545", "http://127.0.0.1:8545"));
        cfg.validate()
            .expect("fork profile + loopback URLs must validate");
    }

    #[test]
    fn fork_profile_rejects_non_loopback_ws() {
        let cfg = fork_cfg(fork_chain(
            "wss://bsc-rpc.publicnode.com",
            "http://127.0.0.1:8545",
        ));
        let err = cfg
            .validate()
            .expect_err("fork profile with public ws_url must fail");
        match err {
            ConfigError::ForkProfileNonLoopbackRpc { chain, field, url } => {
                assert_eq!(chain, "bnb");
                assert_eq!(field, "ws_url");
                assert_eq!(url, "wss://bsc-rpc.publicnode.com");
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn fork_profile_rejects_non_loopback_http() {
        let cfg = fork_cfg(fork_chain(
            "ws://127.0.0.1:8545",
            "https://bsc.drpc.org",
        ));
        let err = cfg
            .validate()
            .expect_err("fork profile with public http_url must fail");
        match err {
            ConfigError::ForkProfileNonLoopbackRpc { chain, field, .. } => {
                assert_eq!(chain, "bnb");
                assert_eq!(field, "http_url");
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn non_fork_profile_skips_loopback_gate() {
        // A mainnet/testnet profile with a non-loopback RPC must NOT
        // trip the fork loopback gate — that gate only applies when
        // `profile_tag == Some("fork")`. This test locks in that the
        // fork branch is strictly additive and never alters the
        // behaviour of production profiles.
        let mut chain = fork_chain("wss://remote.example", "https://remote.example");
        chain.private_rpc_url = Some(SecretString::from("https://priv.example".to_string()));
        let mut cfg = fork_cfg(chain);
        cfg.bot.profile_tag = None;
        cfg.validate()
            .expect("non-fork profile with remote RPC + private_rpc must validate");
    }

    #[test]
    fn unknown_profile_tag_does_not_relax_any_gate() {
        // Forward-compat: a profile tag the code doesn't recognise
        // (e.g. a typo like "froke") must fall through to mainnet-
        // invariant behaviour. Pair a missing private_rpc_url with
        // no allow_public_mempool and the PrivateRpcRequired gate
        // must fire — same as an untagged mainnet profile.
        let mut cfg = fork_cfg(fork_chain(
            "wss://remote.example",
            "https://remote.example",
        ));
        cfg.bot.profile_tag = Some("froke".into());
        let err = cfg
            .validate()
            .expect_err("unknown tag must not bypass mainnet gates");
        assert!(
            matches!(err, ConfigError::PrivateRpcRequired { .. }),
            "expected PrivateRpcRequired, got {err:?}"
        );
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
