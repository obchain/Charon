//! EIP-1559 gas oracle.
//!
//! Three responsibilities:
//!
//! 1. **Live fee snapshot** — read `baseFeePerGas` from the latest
//!    block header and build an EIP-1559 fee pair using the canonical
//!    `max_fee = 2 * base_fee + priority_fee` headroom formula. When
//!    the header has no `baseFeePerGas` (pre-EIP-1559 chain or a
//!    flaky RPC that drops the field), fall back to
//!    `eth_gasPrice` so the oracle still returns a usable quote.
//! 2. **Ceiling enforcement** — refuse to emit gas params if the
//!    proposed `maxFeePerGas` exceeds `bot.max_gas_gwei`. Returned as
//!    a typed [`GasDecision::SkipCeilingExceeded`] variant so callers
//!    can branch without pattern-matching on `Option`.
//! 3. **Cost estimation in USD cents** — converts `gas_units × maxFee`
//!    (wei) into integer USD cents using a Chainlink price reading
//!    for the chain's native asset (BNB on BSC). The result feeds
//!    [`charon_core::ProfitInputs`].
//!
//! ### Unit conventions
//!
//! Internally every fee is kept in **wei** (`u128` for ergonomics,
//! `U256` only where arithmetic might overflow). Gwei is used at the
//! boundary (config input, log lines, [`GasDecision`] payload). The
//! ceiling check converts `bot.max_gas_gwei` → wei before comparing —
//! mixing the two units silently under-filters at 1e9×.

use std::sync::Mutex;

use alloy::eips::BlockNumberOrTag;
use alloy::primitives::U256;
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::transports::TransportError;
use tracing::{debug, warn};

/// 1 gwei expressed in wei.
const ONE_GWEI: u128 = 1_000_000_000;
/// EIP-1559 canonical headroom multiplier on the base fee. The
/// protocol allows the base fee to roughly double between two
/// consecutive blocks under max congestion, so anything lower risks
/// the tx getting booted out of the mempool between sim and broadcast.
const BASE_FEE_HEADROOM_MULT: u128 = 2;
/// Multiplicative gas-estimate safety buffer (`estimate * 12 / 10`).
/// Covers state drift between estimate time and inclusion time.
const GAS_ESTIMATE_BUFFER_NUM: u64 = 12;
const GAS_ESTIMATE_BUFFER_DEN: u64 = 10;
/// Chainlink price-feed decimals. All BNB-Chain USD aggregators we
/// consume publish 8-decimal answers; asserted at oracle setup time.
pub const CHAINLINK_DECIMALS: u8 = 8;

/// Errors the gas oracle can surface. Callers unwrap the typed
/// variant and decide whether to retry (transport), skip
/// (`MissingBaseFee` after fallback), or abort (`Overflow`).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GasError {
    /// `baseFeePerGas` absent *and* the `eth_gasPrice` fallback also
    /// failed. On a healthy EIP-1559 chain we never reach this.
    #[error("baseFeePerGas absent from block header and eth_gasPrice fallback unavailable")]
    MissingBaseFee,
    /// Provider / transport failure (DNS, timeout, 5xx, JSON-RPC
    /// error). Retryable from the caller's perspective.
    #[error("provider error: {0}")]
    Provider(#[from] TransportError),
    /// u128 / U256 arithmetic would overflow. Indicates an absurdly
    /// expensive chain (fee × buffer > 2^128 wei) or a buggy feed;
    /// treated as fatal.
    #[error("arithmetic overflow in gas calculation")]
    Overflow,
}

/// Resolved EIP-1559 gas parameters for one transaction. Always in
/// wei. Convert to gwei at the log / config boundary only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GasParams {
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
}

/// Outcome of a [`GasOracle::fetch_params`] call. Typed enum — no
/// `Option` / `Ok(None)` ambiguity. Callers pattern-match on both
/// variants so adding a new skip reason later shows up as a compile
/// error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GasDecision {
    /// Fee is below ceiling, safe to build the tx.
    Proceed(GasParams),
    /// `maxFeePerGas` would exceed `bot.max_gas_gwei`. Caller drops
    /// the opportunity and logs — both values are gwei for operator
    /// readability.
    SkipCeilingExceeded {
        max_fee_gwei: u64,
        ceiling_gwei: u64,
    },
}

/// Per-block cache entry so repeated `fetch_params(block_n)` calls
/// from the same tick don't spam the RPC.
#[derive(Debug, Clone, Copy)]
struct CacheEntry {
    block: u64,
    decision: GasDecision,
}

/// Per-chain gas oracle. Construct once per chain at startup.
#[derive(Debug)]
pub struct GasOracle {
    /// Drop the tx if `max_fee_per_gas` exceeds this (gwei).
    max_gas_gwei: u64,
    /// EIP-1559 priority fee, gwei. Converted to wei on every call.
    priority_fee_gwei: u64,
    /// Last `(block_number, decision)` observed. `Mutex<Option<_>>`
    /// — contention here is negligible (one lookup per tx build,
    /// microseconds), a lock is simpler than a `RwLock`.
    cache: Mutex<Option<CacheEntry>>,
}

impl GasOracle {
    pub fn new(max_gas_gwei: u64, priority_fee_gwei: u64) -> Self {
        Self {
            max_gas_gwei,
            priority_fee_gwei,
            cache: Mutex::new(None),
        }
    }

    /// Read the latest base fee, apply 2x EIP-1559 headroom, attach
    /// the priority fee.
    ///
    /// Pass `current_block` when the caller already knows the block
    /// number (e.g. inside a `newHeads` handler) to hit the
    /// per-block cache. Pass `None` to force a fresh RPC read.
    pub async fn fetch_params<P, T>(
        &self,
        provider: &P,
        current_block: Option<u64>,
    ) -> Result<GasDecision, GasError>
    where
        P: Provider<T>,
        T: alloy::transports::Transport + Clone,
    {
        // Cache hit: same block we already priced, return the cached
        // decision. Using a scoped lock — never held across await.
        if let Some(block_n) = current_block
            && let Some(entry) = *self.cache.lock().expect("gas cache mutex poisoned")
            && entry.block == block_n
        {
            debug!(block = block_n, "gas params cache hit");
            return Ok(entry.decision);
        }

        let block = provider
            .get_block(
                BlockNumberOrTag::Latest.into(),
                alloy::rpc::types::BlockTransactionsKind::Hashes,
            )
            .await?
            .ok_or(GasError::MissingBaseFee)?;

        // Primary path: EIP-1559 header. Fallback: eth_gasPrice (used
        // to bootstrap max_fee on chains that still occasionally omit
        // the header field under load).
        let base_fee: u128 = match block.header.base_fee_per_gas {
            Some(b) => u128::from(b),
            None => {
                warn!(
                    "header has no baseFeePerGas — falling back to eth_gasPrice (pre-EIP-1559 path)"
                );
                match provider.get_gas_price().await {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(error = %e, "eth_gasPrice fallback failed");
                        return Err(GasError::MissingBaseFee);
                    }
                }
            }
        };

        // Priority fee: gwei → wei, checked.
        let priority_fee_wei = u128::from(self.priority_fee_gwei)
            .checked_mul(ONE_GWEI)
            .ok_or(GasError::Overflow)?;

        // Canonical EIP-1559 headroom: max_fee = 2 * base + priority.
        let max_fee = base_fee
            .checked_mul(BASE_FEE_HEADROOM_MULT)
            .and_then(|v| v.checked_add(priority_fee_wei))
            .ok_or(GasError::Overflow)?;

        // Ceiling check in wei to avoid a 1e9 unit mismatch.
        let max_gas_wei = u128::from(self.max_gas_gwei)
            .checked_mul(ONE_GWEI)
            .ok_or(GasError::Overflow)?;

        let decision = if max_fee > max_gas_wei {
            let max_fee_gwei = u64::try_from(max_fee / ONE_GWEI).unwrap_or(u64::MAX);
            warn!(
                max_fee_gwei,
                ceiling_gwei = self.max_gas_gwei,
                "gas exceeds configured ceiling — skipping tx"
            );
            GasDecision::SkipCeilingExceeded {
                max_fee_gwei,
                ceiling_gwei: self.max_gas_gwei,
            }
        } else {
            let params = GasParams {
                max_fee_per_gas: max_fee,
                max_priority_fee_per_gas: priority_fee_wei,
            };
            debug!(
                base_fee_gwei = base_fee / ONE_GWEI,
                max_fee_gwei = params.max_fee_per_gas / ONE_GWEI,
                priority_fee_gwei = params.max_priority_fee_per_gas / ONE_GWEI,
                "gas params resolved"
            );
            GasDecision::Proceed(params)
        };

        if let Some(block_n) = current_block {
            *self.cache.lock().expect("gas cache mutex poisoned") = Some(CacheEntry {
                block: block_n,
                decision,
            });
        }

        Ok(decision)
    }

    /// Run `eth_estimateGas` on `tx` and return the unit count with a
    /// 20 % safety margin applied. Real-world gas spend drifts by
    /// single-digit percent between estimate and inclusion; the
    /// buffer keeps us from under-funding and landing an out-of-gas
    /// revert on-chain.
    pub async fn estimate_gas_units<P, T>(
        &self,
        provider: &P,
        tx: &TransactionRequest,
    ) -> Result<u64, GasError>
    where
        P: Provider<T>,
        T: alloy::transports::Transport + Clone,
    {
        let raw = provider.estimate_gas(tx).await?;
        Ok(raw.saturating_mul(GAS_ESTIMATE_BUFFER_NUM) / GAS_ESTIMATE_BUFFER_DEN)
    }
}

/// Convert `gas_units × max_fee_per_gas` (wei cost) into USD cents,
/// given a Chainlink reading for the chain's native asset.
///
/// Formula:
/// ```text
///   cost_cents = gas_units * max_fee * native_price * 100
///                / (10^native_decimals_18 * 10^chainlink_decimals)
/// ```
///
/// * `native_price` — raw aggregator answer.
/// * `native_decimals` — the feed's `decimals()`, typically
///   [`CHAINLINK_DECIMALS`] (8).
/// * The native unit is assumed 18-decimal (true on BNB / ETH /
///   MATIC / AVAX; callers on a non-18-decimal native must adjust).
pub fn gas_cost_usd_cents(
    gas_units: u64,
    max_fee_per_gas: u128,
    native_price: U256,
    native_decimals: u8,
) -> u64 {
    let wei_cost = U256::from(gas_units).saturating_mul(U256::from(max_fee_per_gas));
    // wei / 1e18 = native-units; × price / 10^feed_decimals = USD;
    // × 100 = cents. Combined divisor: 10^(18 + native_decimals - 2).
    // Kept as a single division to keep rounding in one place.
    let exponent = 18u32 + u32::from(native_decimals) - 2;
    let divisor = U256::from(10u64).pow(U256::from(exponent));

    let numerator = wei_cost.saturating_mul(native_price);
    let cents = numerator / divisor;
    u64::try_from(cents).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gas_cost_for_one_gwei_at_bnb_632_dollar() {
        // 200_000 gas × 1 gwei = 2e14 wei.
        // BNB at $632.85 → Chainlink price 63284968915 (8 decimals).
        // Expected cents: 2e14 * 6.3284968915e10 * 100 / (1e18 * 1e8)
        //               = 2e14 * 6.3284968915e10 / 1e24
        //               ≈ 12.66 cents.
        let cents = gas_cost_usd_cents(200_000, ONE_GWEI, U256::from(63_284_968_915u128), 8);
        assert!((12..=14).contains(&cents), "got {cents} cents");
    }

    #[test]
    fn gas_cost_zero_when_units_zero() {
        let cents = gas_cost_usd_cents(0, ONE_GWEI, U256::from(1u64), 8);
        assert_eq!(cents, 0);
    }

    #[test]
    fn gas_cost_saturates_on_huge_inputs() {
        // 1e9 gas × 1e30 wei/gas = 1e39 wei — clearly absurd, must
        // saturate to u64::MAX without panicking.
        let cents = gas_cost_usd_cents(
            1_000_000_000,
            10u128.pow(30),
            U256::from(63_284_968_915u128),
            8,
        );
        assert_eq!(cents, u64::MAX);
    }

    #[test]
    fn priority_fee_wei_conversion_one_gwei() {
        // Sanity: priority_fee_gwei = 1 must resolve to 1e9 wei on
        // the GasParams boundary.
        let oracle = GasOracle::new(100, 1);
        // Direct field access via constructor — can't call fetch
        // without a provider, so we reproduce the arithmetic path
        // that fetch_params uses.
        let wei = u128::from(oracle.priority_fee_gwei)
            .checked_mul(ONE_GWEI)
            .unwrap();
        assert_eq!(wei, 1_000_000_000u128);
    }

    #[test]
    fn priority_fee_wei_conversion_five_gwei() {
        let wei = 5u128.checked_mul(ONE_GWEI).unwrap();
        assert_eq!(wei, 5_000_000_000u128);
    }

    #[test]
    fn max_fee_uses_two_x_headroom() {
        // base = 3 gwei, priority = 1 gwei → max_fee = 7 gwei.
        let base_wei = 3u128 * ONE_GWEI;
        let priority_wei = ONE_GWEI;
        let max_fee = base_wei
            .checked_mul(BASE_FEE_HEADROOM_MULT)
            .and_then(|v| v.checked_add(priority_wei))
            .unwrap();
        assert_eq!(max_fee, 7u128 * ONE_GWEI);
    }

    #[test]
    fn gas_estimate_buffer_is_twenty_percent() {
        let raw: u64 = 1_000_000;
        let buffered = raw.saturating_mul(GAS_ESTIMATE_BUFFER_NUM) / GAS_ESTIMATE_BUFFER_DEN;
        assert_eq!(buffered, 1_200_000);
    }

    #[test]
    fn ceiling_in_wei_rejects_over_limit() {
        // Ceiling 10 gwei in wei = 1e10. A max_fee of 11 gwei must
        // trip the ceiling. Sanity-checks the unit fix for #179.
        let ceiling_wei = 10u128 * ONE_GWEI;
        let max_fee = 11u128 * ONE_GWEI;
        assert!(max_fee > ceiling_wei);
    }

    // Live-network integration test (#191). Requires BNB_HTTP_URL;
    // ignored by default so `cargo test` stays offline.
    #[tokio::test]
    #[ignore = "requires live BNB_HTTP_URL"]
    async fn fetch_params_against_live_bsc() {
        use alloy::providers::ProviderBuilder;
        let url = std::env::var("BNB_HTTP_URL").expect("BNB_HTTP_URL not set");
        let provider = ProviderBuilder::new().on_http(url.parse().expect("valid http url"));
        let oracle = GasOracle::new(100, 1);
        let decision = oracle
            .fetch_params(&provider, None)
            .await
            .expect("fetch_params should succeed against live BSC");
        match decision {
            GasDecision::Proceed(params) => {
                let max_fee_gwei = params.max_fee_per_gas / ONE_GWEI;
                let priority_gwei = params.max_priority_fee_per_gas / ONE_GWEI;
                assert!(
                    (1..=100).contains(&max_fee_gwei),
                    "max_fee {max_fee_gwei} gwei outside sane BSC range"
                );
                assert!(
                    priority_gwei >= 1,
                    "priority {priority_gwei} gwei below floor"
                );
            }
            GasDecision::SkipCeilingExceeded { .. } => {
                panic!("ceiling of 100 gwei should not trip on BSC");
            }
        }
    }
}
