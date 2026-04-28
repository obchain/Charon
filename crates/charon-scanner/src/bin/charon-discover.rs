//! `charon-discover` — standalone Venus borrower discovery sidecar.
//!
//! Scans Venus vToken `Borrow` events over a configurable history
//! window via free-tier WebSocket RPCs and writes the unique borrower
//! set to a text file consumable by `charon listen --borrower-file`.
//!
//! Why a separate binary? The main bot opens its WebSocket against
//! the operator's primary RPC and keeps it live; running a 200_000-block
//! `eth_getLogs` backfill on that same endpoint at startup either burns
//! the rate-limit budget on a paid tier or fails outright on a free
//! one. Splitting discovery into a sidecar lets the operator wire a
//! rotating fallback list of free-tier endpoints, run the sidecar on a
//! cron (weekly is enough — the bot's live tail catches new borrowers
//! between runs), and keep the bot's primary RPC clear for the hot
//! per-block scan path.
//!
//! ```sh
//! charon-discover \
//!   --config config/default.toml \
//!   --output borrowers.txt \
//!   --window-blocks 200000 \
//!   --extra-rpc wss://bsc-rpc.publicnode.com \
//!   --extra-rpc wss://bsc.publicnode.com
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, RootProvider, WsConnect};
use alloy::pubsub::PubSubFrontend;
use alloy::sol;
use anyhow::{Context, Result};
use charon_core::Config;
use charon_scanner::{BorrowerSet, DEFAULT_BACKFILL_BLOCKS, backfill_borrowers};
use clap::Parser;
use tokio::time::timeout;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// Per-RPC handshake deadline. Mirrors `ChainProvider::DEFAULT_CONNECT_TIMEOUT`
/// so a slow/unresponsive endpoint is rotated out fast on the fallback
/// chain rather than blocking the whole sidecar.
const RPC_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

// Minimal Venus / Compound Comptroller interface — only `getAllMarkets`
// is needed here. We deliberately avoid `VenusAdapter::connect`: that
// path also resolves oracle, decimals, symbols, and underlying maps
// per vToken which is wasted work for a discovery-only scrape.
sol! {
    #[sol(rpc)]
    interface Comptroller {
        function getAllMarkets() external view returns (address[] memory);
    }
}

/// Charon — Venus borrower discovery sidecar.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Path to the TOML config file. The chain's `ws_url` becomes the
    /// primary endpoint; `--extra-rpc` flags are tried in order on
    /// failure.
    #[arg(long, default_value = "config/default.toml")]
    config: PathBuf,

    /// `[chain.<name>]` key whose Venus protocol section we scrape.
    #[arg(long, default_value = "bnb")]
    chain: String,

    /// Borrower-list output path. Atomic-rename written.
    #[arg(long, default_value = "borrowers.txt")]
    output: PathBuf,

    /// History window in blocks (defaults to ~7 d on BSC).
    #[arg(long, default_value_t = DEFAULT_BACKFILL_BLOCKS)]
    window_blocks: u64,

    /// Repeatable WebSocket fallback URL. Tried in declaration order
    /// after the config's primary endpoint fails. Format: `wss://...`
    /// or `ws://...`.
    #[arg(long = "extra-rpc")]
    extra_rpc: Vec<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Mirror `charon listen`'s tracing setup so log output looks the
    // same in the operator's terminal across the two binaries.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();

    // Silent no-op if `.env` is absent — same behaviour as `charon`.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    info!(
        config = %cli.config.display(),
        chain = %cli.chain,
        output = %cli.output.display(),
        window_blocks = cli.window_blocks,
        extra_rpcs = cli.extra_rpc.len(),
        "charon-discover starting"
    );

    let config = Config::load(&cli.config)
        .with_context(|| format!("failed to load config from {}", cli.config.display()))?;

    let chain_cfg = config
        .chain
        .get(&cli.chain)
        .with_context(|| format!("chain '{}' not in config", cli.chain))?;
    let venus_cfg = config
        .protocol
        .get("venus")
        .context("config has no [protocol.venus] section — discovery is Venus-only")?;
    let comptroller = venus_cfg.comptroller;

    // RPC pool: primary first, fallbacks in order.
    let mut pool: Vec<String> = Vec::with_capacity(cli.extra_rpc.len().saturating_add(1));
    pool.push(chain_cfg.ws_url.clone());
    pool.extend(cli.extra_rpc.iter().cloned());

    let set = BorrowerSet::new();
    let mut tried: usize = 0;
    let mut succeeded = false;

    for url in &pool {
        tried = tried.saturating_add(1);
        let safe = safe_url(url);
        info!(rpc = %safe, "discovery: trying rpc");

        match try_one_rpc(url, comptroller, cli.window_blocks, &set).await {
            Ok(()) => {
                succeeded = true;
                info!(
                    rpc = %safe,
                    discovered = set.len(),
                    "discovery: rpc succeeded"
                );
                break;
            }
            Err(err) => {
                warn!(
                    rpc = %safe,
                    error = ?err,
                    "discovery: rpc failed — rotating to next endpoint"
                );
            }
        }
    }

    if !succeeded {
        error!(
            tried,
            "discovery: all rpc endpoints failed — borrower file not written"
        );
        std::process::exit(1);
    }

    write_borrower_file(&cli.output, &set)?;
    info!(
        path = %cli.output.display(),
        count = set.len(),
        "borrower file written"
    );
    Ok(())
}

/// Connect, list vTokens, run a single backfill. Returns `Ok(())` if
/// the borrower set was successfully appended to; the caller decides
/// whether to rotate to the next RPC on error.
async fn try_one_rpc(
    url: &str,
    comptroller: Address,
    window_blocks: u64,
    set: &BorrowerSet,
) -> Result<()> {
    let provider = timeout(RPC_CONNECT_TIMEOUT, async {
        ProviderBuilder::new()
            .on_ws(WsConnect::new(url))
            .await
            .context("ws connect failed")
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "ws connect timed out after {}s",
            RPC_CONNECT_TIMEOUT.as_secs()
        )
    })??;
    let provider: Arc<RootProvider<PubSubFrontend>> = Arc::new(provider);

    let comp = Comptroller::new(comptroller, provider.clone());
    let vtokens = comp
        .getAllMarkets()
        .call()
        .await
        .context("Comptroller.getAllMarkets() failed")?
        ._0;
    if vtokens.is_empty() {
        anyhow::bail!("Comptroller returned zero markets");
    }

    let head = provider
        .get_block_number()
        .await
        .context("get_block_number failed")?;
    // saturating_sub so a chain younger than the requested window
    // collapses to genesis rather than wrapping around.
    let from_block = head.saturating_sub(window_blocks);
    let to_block = head;
    info!(
        head,
        from_block,
        to_block,
        markets = vtokens.len(),
        "discovery: starting backfill"
    );

    backfill_borrowers(provider.as_ref(), vtokens, set, from_block, to_block)
        .await
        .context("backfill_borrowers failed")?;
    Ok(())
}

/// Strip query strings (which commonly carry api keys on free-tier
/// gateways) before logging. Mirrors `provider::redact_url`'s intent —
/// kept inline here to avoid widening the scanner's public surface for
/// a binary-local helper.
fn safe_url(url: &str) -> String {
    match url.find('?') {
        Some(i) => url[..i].to_string(),
        None => url.to_string(),
    }
}

/// Sort the borrower set lexically and write it to `output` via a
/// `<output>.tmp` + `rename` two-step so a consumer (the bot) reading
/// the file concurrently never sees a half-written line.
fn write_borrower_file(output: &std::path::Path, set: &BorrowerSet) -> Result<()> {
    let mut addrs = set.snapshot();
    // Sort by raw bytes so two consecutive runs produce reviewable
    // diffs. `Address` is `[u8; 20]` so the natural `Ord` works.
    addrs.sort();

    let mut tmp_path = output.to_path_buf();
    let mut tmp_name = tmp_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    tmp_name.push(".tmp");
    tmp_path.set_file_name(tmp_name);

    let mut buf = String::with_capacity(addrs.len().saturating_mul(43));
    for addr in &addrs {
        // EIP-55 mixed-case hex via Display.
        buf.push_str(&format!("{:#x}", addr));
        buf.push('\n');
    }
    std::fs::write(&tmp_path, buf)
        .with_context(|| format!("write temp borrower file {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, output)
        .with_context(|| format!("rename {} -> {}", tmp_path.display(), output.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_url_strips_query_string() {
        assert_eq!(
            safe_url("wss://bsc-mainnet.example/ws?key=ABC"),
            "wss://bsc-mainnet.example/ws"
        );
    }

    #[test]
    fn safe_url_passes_through_when_no_query() {
        assert_eq!(
            safe_url("wss://bsc-rpc.publicnode.com"),
            "wss://bsc-rpc.publicnode.com"
        );
    }
}
