//! Aave V3 flash-loan adapter.
//!
//! Aave V3 is the default flash-loan source on BSC for Charon v0.1 —
//! Balancer is not deployed there, so the router falls straight to Aave
//! for every liquidation. The adapter reads the current
//! `FLASHLOAN_PREMIUM_TOTAL` (Aave encodes it as 4-decimal percent,
//! e.g. `5` means 0.05%) at connect time and converts it to the
//! workspace-wide millionths (1e6) convention by multiplying by 100
//! (Aave `5` -> `500` millionths). It also uses the Aave
//! `PoolDataProvider` to resolve each asset's aToken (whose underlying
//! balance is the liquidity ceiling for a flash loan) and to read the
//! reserve configuration bitmap so paused / frozen reserves are
//! rejected before the router even tries to build calldata.

use std::sync::Arc;

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::pubsub::PubSubFrontend;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};
use async_trait::async_trait;
use charon_core::{
    ConfigError, FlashLoanError, FlashLoanProvider, FlashLoanQuote, FlashLoanSource,
};
use tracing::{debug, info};

/// BSC mainnet chain id. The adapter refuses to connect to anything
/// else — Aave V3 is deployed on many chains but the current config
/// and receiver only target BNB Chain.
pub const BSC_CHAIN_ID: u64 = 56;

/// Aave encodes the flash-loan premium as 4-decimal percent (e.g. `5`
/// means 0.05%). We store fee rates in millionths (1e6) workspace-wide,
/// so multiply Aave's value by this factor at conversion time.
const AAVE_PREMIUM_TO_MILLIONTHS: u32 = 100;

/// Denominator used by Aave when applying the premium on-chain. Kept
/// private — external callers should read `fee` out of the quote
/// rather than recomputing it.
const AAVE_PREMIUM_DENOMINATOR: u64 = 10_000;

/// Aave V3 reserve configuration bitmap layout. Only the bits the
/// adapter cares about are extracted; see the Aave V3
/// `ReserveConfiguration.sol` library for the full layout.
const RESERVE_FROZEN_BIT: u32 = 57;
const RESERVE_PAUSED_BIT: u32 = 60;

sol! {
    /// Aave V3 `IPoolAddressesProvider` — canonical registry that
    /// resolves the live `Pool` address. Aave governance migrates
    /// pool implementations periodically; reading this at startup
    /// catches a stale `pool` address in `[flashloan.aave_v3_*]`
    /// before the bot burns RPC budget on a dead deploy.
    #[sol(rpc)]
    interface IPoolAddressesProvider {
        function getPool() external view returns (address);
    }

    /// Aave V3 Pool — flash-loan entry point.
    #[sol(rpc)]
    interface IAaveV3Pool {
        /// Flash-loan a single asset; the receiver's `executeOperation`
        /// is called inside the same tx and must approve `Pool` for
        /// `amount + premium` before returning.
        function flashLoanSimple(
            address receiverAddress,
            address asset,
            uint256 amount,
            bytes calldata params,
            uint16 referralCode
        ) external;

        /// Flash-loan premium in 4-decimal percent (e.g. `5` = 0.05%).
        function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128);
    }

    /// Aave V3 PoolDataProvider — resolves asset → aToken / debt tokens
    /// and exposes the packed reserve configuration bitmap.
    #[sol(rpc)]
    interface IAaveV3DataProvider {
        function getReserveTokensAddresses(address asset)
            external view returns (
                address aTokenAddress,
                address stableDebtTokenAddress,
                address variableDebtTokenAddress
            );

        function getReserveConfigurationData(address asset)
            external view returns (
                uint256 decimals,
                uint256 ltv,
                uint256 liquidationThreshold,
                uint256 liquidationBonus,
                uint256 reserveFactor,
                bool usageAsCollateralEnabled,
                bool borrowingEnabled,
                bool stableBorrowRateEnabled,
                bool isActive,
                bool isFrozen
            );

        /// Packed configuration bitmap — Aave stores paused (bit 60)
        /// and frozen (bit 57) flags here. The per-field accessors
        /// above are convenient but don't currently expose `paused`,
        /// so we read the bitmap directly.
        function getReserveData(address asset)
            external view returns (
                uint256 configuration,
                uint128 liquidityIndex,
                uint128 currentLiquidityRate,
                uint128 variableBorrowIndex,
                uint128 currentVariableBorrowRate,
                uint128 currentStableBorrowRate,
                uint40  lastUpdateTimestamp,
                uint16  id,
                address aTokenAddress,
                address stableDebtTokenAddress,
                address variableDebtTokenAddress,
                address interestRateStrategyAddress,
                uint128 accruedToTreasury,
                uint128 unbacked,
                uint128 isolationModeTotalDebt
            );
    }

    /// ERC-20 surface we need (balance-of only).
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
    }
}

/// Aave V3 flash-loan adapter.
///
/// Paired with a single liquidator receiver — the
/// `CharonLiquidator.sol` contract deployed for this operator. The
/// adapter does not own any keys or tokens; it only encodes calls and
/// reads on-chain state.
#[derive(Clone)]
pub struct AaveFlashLoan {
    provider: Arc<RootProvider<PubSubFrontend>>,
    pool: Address,
    data_provider: Address,
    /// Receiver = `CharonLiquidator.sol`. Must implement
    /// `IFlashLoanSimpleReceiver.executeOperation` to handle the
    /// callback.
    receiver: Address,
    chain_id: u64,
    /// Fee rate in millionths (1e6). Aave V3 BSC is typically
    /// `500` (= 0.05%).
    fee_rate_millionths: u32,
    /// Aave's raw premium in 4-decimal percent (needed to recompute
    /// the absolute fee at quote time without losing precision).
    aave_premium: u32,
}

impl AaveFlashLoan {
    /// Connect to the pool, cache its current flash-loan premium, and
    /// verify the chain id.
    ///
    /// `data_provider` is the Aave V3 `PoolDataProvider` address for
    /// the chain — plumbed in from [`charon_core::config::FlashLoanConfig`]
    /// so multi-chain expansion is a config change, not a code change.
    pub async fn connect(
        provider: Arc<RootProvider<PubSubFrontend>>,
        pool: Address,
        data_provider: Address,
        receiver: Address,
    ) -> Result<Self> {
        debug!(%pool, %data_provider, %receiver, "connecting Aave V3 flash-loan adapter");

        let chain_id = provider
            .get_chain_id()
            .await
            .context("Aave V3: eth_chainId failed")?;
        anyhow::ensure!(
            chain_id == BSC_CHAIN_ID,
            "Aave V3 adapter is BSC-only for v0.1: expected chain_id {BSC_CHAIN_ID}, got {chain_id}"
        );

        let pool_if = IAaveV3Pool::new(pool, provider.clone());
        let premium = pool_if
            .FLASHLOAN_PREMIUM_TOTAL()
            .call()
            .await
            .context("Aave V3: FLASHLOAN_PREMIUM_TOTAL() failed")?
            ._0;
        let aave_premium = u32::try_from(premium)
            .context("Aave V3: premium does not fit in u32 — unexpected value")?;
        // Aave 5 (0.05%) -> 500 millionths.
        let fee_rate_millionths = aave_premium
            .checked_mul(AAVE_PREMIUM_TO_MILLIONTHS)
            .context("Aave V3: premium -> millionths overflow")?;

        info!(
            %pool,
            %data_provider,
            %receiver,
            chain_id,
            aave_premium,
            fee_rate_millionths,
            "Aave V3 flash-loan adapter ready"
        );

        Ok(Self {
            provider,
            pool,
            data_provider,
            receiver,
            chain_id,
            fee_rate_millionths,
            aave_premium,
        })
    }

    /// Cross-check the configured `pool` against
    /// `IPoolAddressesProvider.getPool()`. On mismatch, surface a
    /// typed [`ConfigError::AaveAddressMismatch`] so the CLI can fail
    /// fast at startup rather than discover the stale address one
    /// reverted flashLoanSimple at a time.
    ///
    /// `key` is the `[flashloan.<key>]` section name (e.g.
    /// `aave_v3_bsc`) used in error messages so operators can locate
    /// the offending TOML field at a glance.
    pub async fn validate_against_addresses_provider(
        provider: Arc<RootProvider<PubSubFrontend>>,
        addresses_provider: Address,
        configured_pool: Address,
        key: &str,
    ) -> Result<(), ConfigError> {
        let ap = IPoolAddressesProvider::new(addresses_provider, provider);
        let on_chain = ap
            .getPool()
            .call()
            .await
            .map_err(|e| {
                ConfigError::Validation(format!(
                    "flashloan '{key}': IPoolAddressesProvider.getPool() rpc failed: {e}"
                ))
            })?
            ._0;
        assert_pool_matches(key, configured_pool, on_chain, addresses_provider)?;
        info!(
            %addresses_provider,
            %configured_pool,
            "Aave V3 pool address validated against IPoolAddressesProvider"
        );
        Ok(())
    }

    /// Return the aToken address for `asset`. Falls back to `None` when
    /// Aave does not list the asset on this chain (call reverts or
    /// returns the zero address).
    async fn atoken_for(&self, asset: Address) -> Result<Option<Address>, FlashLoanError> {
        let dp = IAaveV3DataProvider::new(self.data_provider, self.provider.clone());
        match dp.getReserveTokensAddresses(asset).call().await {
            Ok(r) => {
                let atoken = r.aTokenAddress;
                if atoken == Address::ZERO {
                    Ok(None)
                } else {
                    Ok(Some(atoken))
                }
            }
            Err(_) => Ok(None),
        }
    }

    /// Check the reserve's packed configuration for paused / frozen
    /// flags. Either one makes a flash loan revert on Aave, so the
    /// adapter rejects the borrow before even asking for liquidity.
    async fn assert_reserve_open(&self, asset: Address) -> Result<(), FlashLoanError> {
        let dp = IAaveV3DataProvider::new(self.data_provider, self.provider.clone());
        let cfg = dp
            .getReserveConfigurationData(asset)
            .call()
            .await
            .map_err(|e| FlashLoanError::rpc(format!("getReserveConfigurationData: {e}")))?;
        if !cfg.isActive || cfg.isFrozen {
            return Err(FlashLoanError::ReservePaused { asset });
        }
        // Paused is not exposed via the typed accessor, so read the
        // packed bitmap and check bit 60 ourselves.
        let data = dp
            .getReserveData(asset)
            .call()
            .await
            .map_err(|e| FlashLoanError::rpc(format!("getReserveData: {e}")))?;
        if bitmap_says_paused(data.configuration) || bitmap_says_frozen(data.configuration) {
            return Err(FlashLoanError::ReservePaused { asset });
        }
        Ok(())
    }
}

/// Return true when bit `index` is set in the Aave packed
/// configuration `U256`.
fn bit_is_set(bitmap: U256, index: u32) -> bool {
    (bitmap >> index) & U256::from(1u8) == U256::from(1u8)
}

/// Pure extractor: is the `paused` flag set in an Aave V3 packed
/// reserve configuration bitmap (bit 60). Split out so unit tests
/// can exercise hand-crafted bitmaps without standing up a live RPC,
/// and so a future Aave layout change (or a typo in the constant)
/// trips a unit test instead of producing a silent revert in
/// production. See `ReserveConfiguration.sol` in aave-v3-core.
pub fn bitmap_says_paused(bitmap: U256) -> bool {
    bit_is_set(bitmap, RESERVE_PAUSED_BIT)
}

/// Pure extractor: is the `frozen` flag set in an Aave V3 packed
/// reserve configuration bitmap (bit 57). Companion to
/// [`bitmap_says_paused`]; same rationale.
pub fn bitmap_says_frozen(bitmap: U256) -> bool {
    bit_is_set(bitmap, RESERVE_FROZEN_BIT)
}

/// Pure comparison helper: emit `ConfigError::AaveAddressMismatch` if
/// the on-chain pool does not match the configured one. Split out so
/// unit tests can exercise the comparison + error shape without
/// standing up a live `RootProvider`.
fn assert_pool_matches(
    key: &str,
    configured: Address,
    on_chain: Address,
    provider: Address,
) -> Result<(), ConfigError> {
    if configured == on_chain {
        return Ok(());
    }
    Err(ConfigError::AaveAddressMismatch {
        key: key.to_string(),
        configured,
        on_chain,
        provider,
    })
}

#[async_trait]
impl FlashLoanProvider for AaveFlashLoan {
    fn source(&self) -> FlashLoanSource {
        FlashLoanSource::AaveV3
    }

    fn chain_id(&self) -> u64 {
        self.chain_id
    }

    fn fee_rate_millionths(&self) -> u32 {
        self.fee_rate_millionths
    }

    async fn available_liquidity(&self, token: Address) -> Result<U256, FlashLoanError> {
        self.assert_reserve_open(token).await?;
        let Some(atoken) = self.atoken_for(token).await? else {
            return Ok(U256::ZERO);
        };
        let erc20 = IERC20::new(token, self.provider.clone());
        let bal = erc20
            .balanceOf(atoken)
            .call()
            .await
            .map_err(|e| FlashLoanError::rpc(format!("balanceOf({atoken}): {e}")))?
            ._0;
        Ok(bal)
    }

    async fn quote(
        &self,
        token: Address,
        amount: U256,
    ) -> Result<Option<FlashLoanQuote>, FlashLoanError> {
        let liquidity = self.available_liquidity(token).await?;
        if liquidity < amount {
            return Ok(None);
        }
        // fee = amount * aave_premium / 10_000 (Aave's canonical math,
        // preserved exactly — we don't round-trip through millionths).
        let fee = amount
            .checked_mul(U256::from(self.aave_premium))
            .ok_or_else(|| FlashLoanError::other("fee multiplication overflow"))?
            / U256::from(AAVE_PREMIUM_DENOMINATOR);
        Ok(Some(FlashLoanQuote {
            source: FlashLoanSource::AaveV3,
            chain_id: self.chain_id,
            token,
            amount,
            fee,
            fee_rate_millionths: self.fee_rate_millionths,
            pool_address: self.pool,
        }))
    }

    fn build_flashloan_calldata(
        &self,
        quote: &FlashLoanQuote,
        liquidation_params: &[u8],
    ) -> Result<Vec<u8>, FlashLoanError> {
        if liquidation_params.is_empty() {
            return Err(FlashLoanError::other(
                "build_flashloan_calldata: liquidation_params is empty; \
                 executeOperation would revert on ABI decode",
            ));
        }
        let call = IAaveV3Pool::flashLoanSimpleCall {
            receiverAddress: self.receiver,
            asset: quote.token,
            amount: quote.amount,
            params: liquidation_params.to_vec().into(),
            referralCode: 0,
        };
        Ok(call.abi_encode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn flash_loan_simple_calldata_has_correct_selector() {
        let quote = FlashLoanQuote {
            source: FlashLoanSource::AaveV3,
            chain_id: 56,
            token: address!("1111111111111111111111111111111111111111"),
            amount: U256::from(1_000u64),
            fee: U256::from(5u64),
            fee_rate_millionths: 500,
            pool_address: address!("2222222222222222222222222222222222222222"),
        };
        // Standalone encoder mirror of `build_flashloan_calldata` so we
        // can test without constructing the full adapter (needs a real
        // WS provider).
        let call = IAaveV3Pool::flashLoanSimpleCall {
            receiverAddress: address!("3333333333333333333333333333333333333333"),
            asset: quote.token,
            amount: quote.amount,
            params: vec![0xDE, 0xAD, 0xBE, 0xEF].into(),
            referralCode: 0,
        };
        let bytes = call.abi_encode();
        assert_eq!(
            &bytes[..4],
            &IAaveV3Pool::flashLoanSimpleCall::SELECTOR,
            "selector mismatch — check flashLoanSimple arg order"
        );
    }

    #[test]
    fn assert_pool_matches_returns_ok_on_equal_addresses() {
        let a = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let provider = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert_pool_matches("aave_v3_bsc", a, a, provider).expect("equal addresses must pass");
    }

    #[test]
    fn assert_pool_matches_returns_typed_mismatch_on_drift() {
        let configured = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let on_chain = address!("cccccccccccccccccccccccccccccccccccccccc");
        let provider = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let err = assert_pool_matches("aave_v3_bsc", configured, on_chain, provider)
            .expect_err("mismatch must surface");
        match err {
            ConfigError::AaveAddressMismatch {
                key,
                configured: c,
                on_chain: o,
                provider: p,
            } => {
                assert_eq!(key, "aave_v3_bsc");
                assert_eq!(c, configured);
                assert_eq!(o, on_chain);
                assert_eq!(p, provider);
            }
            other => panic!("expected AaveAddressMismatch, got {other:?}"),
        }
    }

    #[test]
    fn aave_address_mismatch_display_names_section_and_addresses() {
        let configured = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let on_chain = address!("cccccccccccccccccccccccccccccccccccccccc");
        let provider = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let err = ConfigError::AaveAddressMismatch {
            key: "aave_v3_bsc".to_string(),
            configured,
            on_chain,
            provider,
        };
        let s = format!("{err}");
        assert!(s.contains("aave_v3_bsc"), "{s}");
        assert!(s.contains(&format!("{configured}")), "{s}");
        assert!(s.contains(&format!("{on_chain}")), "{s}");
        assert!(s.contains(&format!("{provider}")), "{s}");
    }

    #[test]
    fn bit_is_set_reads_paused_and_frozen_bits() {
        let paused = U256::from(1u64) << RESERVE_PAUSED_BIT;
        let frozen = U256::from(1u64) << RESERVE_FROZEN_BIT;
        assert!(bit_is_set(paused, RESERVE_PAUSED_BIT));
        assert!(!bit_is_set(paused, RESERVE_FROZEN_BIT));
        assert!(bit_is_set(frozen, RESERVE_FROZEN_BIT));
        assert!(!bit_is_set(frozen, RESERVE_PAUSED_BIT));
        assert!(!bit_is_set(U256::ZERO, RESERVE_PAUSED_BIT));
        assert!(!bit_is_set(U256::ZERO, RESERVE_FROZEN_BIT));
    }

    /// Only bit 57 set → bitmap_says_frozen, not paused.
    #[test]
    fn bitmap_says_frozen_when_only_bit_57_set() {
        let cfg = U256::from(1u64) << RESERVE_FROZEN_BIT;
        assert!(bitmap_says_frozen(cfg), "frozen helper must trip on bit 57");
        assert!(
            !bitmap_says_paused(cfg),
            "paused helper must NOT trip on the frozen bit"
        );
    }

    /// Only bit 60 set → bitmap_says_paused, not frozen.
    #[test]
    fn bitmap_says_paused_when_only_bit_60_set() {
        let cfg = U256::from(1u64) << RESERVE_PAUSED_BIT;
        assert!(bitmap_says_paused(cfg), "paused helper must trip on bit 60");
        assert!(
            !bitmap_says_frozen(cfg),
            "frozen helper must NOT trip on the paused bit"
        );
    }

    /// Both bits clear → neither helper trips.
    #[test]
    fn bitmap_helpers_clear_when_both_bits_unset() {
        let cfg = U256::ZERO;
        assert!(!bitmap_says_paused(cfg));
        assert!(!bitmap_says_frozen(cfg));

        // Some unrelated bits set — must still report clear.
        let mut other = U256::ZERO;
        other |= U256::from(1u64) << 0;
        other |= U256::from(1u64) << 16;
        other |= U256::from(1u64) << 32;
        assert!(!bitmap_says_paused(other));
        assert!(!bitmap_says_frozen(other));
    }

    /// Off-by-one guard: bits 56, 58, 59, 61, 62 must NOT register as
    /// frozen or paused. Catches a typo that would shift the constant
    /// by one and silently reroute paused/frozen reserves through the
    /// flash-loan path.
    #[test]
    fn bitmap_helpers_immune_to_off_by_one_bits() {
        for bit in [56u32, 58, 59, 61, 62] {
            let cfg = U256::from(1u64) << bit;
            assert!(
                !bitmap_says_paused(cfg),
                "bit {bit} must NOT register as paused (paused = bit 60)"
            );
            assert!(
                !bitmap_says_frozen(cfg),
                "bit {bit} must NOT register as frozen (frozen = bit 57)"
            );
        }
    }
}
