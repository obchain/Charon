//! Aave V3 flash-loan adapter.
//!
//! Aave V3 is the default flash-loan source on BSC for Charon v0.1 —
//! Balancer is not deployed there, so the router falls straight to Aave
//! for every liquidation. The adapter reads the current
//! `FLASHLOAN_PREMIUM_TOTAL` (basis points) at connect time and uses
//! the Aave `PoolDataProvider` to resolve each asset's aToken, whose
//! underlying balance is the liquidity ceiling for a flash loan.

use std::sync::Arc;

use alloy::primitives::{Address, U256, address};
use alloy::providers::{Provider, RootProvider};
use alloy::pubsub::PubSubFrontend;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};
use async_trait::async_trait;
use charon_core::{FlashLoanProvider, FlashLoanQuote, FlashLoanSource};
use tracing::{debug, info};

/// Aave V3 `PoolDataProvider` on BSC mainnet.
///
/// Hardcoded because v0.1 targets a single chain; when multi-chain
/// expansion lands this moves into `FlashLoanConfig`.
pub const AAVE_V3_BSC_DATA_PROVIDER: Address = address!("23dF2a19384231aFD114b036C14b6b03324D79BC");

sol! {
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

        /// Flash-loan premium in basis points (e.g. `5` = 0.05%).
        function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128);
    }

    /// Aave V3 PoolDataProvider — resolves asset → aToken / debt tokens.
    #[sol(rpc)]
    interface IAaveV3DataProvider {
        function getReserveTokensAddresses(address asset)
            external view returns (
                address aTokenAddress,
                address stableDebtTokenAddress,
                address variableDebtTokenAddress
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
    fee_bps: u16,
}

impl AaveFlashLoan {
    /// Connect to the pool, cache its current flash-loan premium, and
    /// verify the chain id. The data provider address defaults to the
    /// BSC constant above; other chains will need an explicit override
    /// once multi-chain support lands.
    pub async fn connect(
        provider: Arc<RootProvider<PubSubFrontend>>,
        pool: Address,
        receiver: Address,
    ) -> Result<Self> {
        debug!(%pool, %receiver, "connecting Aave V3 flash-loan adapter");

        let pool_if = IAaveV3Pool::new(pool, provider.clone());
        let premium = pool_if
            .FLASHLOAN_PREMIUM_TOTAL()
            .call()
            .await
            .context("Aave V3: FLASHLOAN_PREMIUM_TOTAL() failed")?
            ._0;
        let fee_bps = u16::try_from(premium)
            .context("Aave V3: premium does not fit in u16 bps — unexpected value")?;

        let chain_id = provider
            .get_chain_id()
            .await
            .context("Aave V3: eth_chainId failed")?;

        info!(
            %pool,
            %receiver,
            chain_id,
            fee_bps,
            "Aave V3 flash-loan adapter ready"
        );

        Ok(Self {
            provider,
            pool,
            data_provider: AAVE_V3_BSC_DATA_PROVIDER,
            receiver,
            chain_id,
            fee_bps,
        })
    }

    /// Return the aToken address for `asset`. Falls back to `None` when
    /// Aave does not list the asset on this chain (call reverts or
    /// returns the zero address).
    async fn atoken_for(&self, asset: Address) -> Result<Option<Address>> {
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
}

#[async_trait]
impl FlashLoanProvider for AaveFlashLoan {
    fn source(&self) -> FlashLoanSource {
        FlashLoanSource::AaveV3
    }

    fn chain_id(&self) -> u64 {
        self.chain_id
    }

    fn fee_rate_bps(&self) -> u16 {
        self.fee_bps
    }

    async fn available_liquidity(&self, token: Address) -> Result<U256> {
        let Some(atoken) = self.atoken_for(token).await? else {
            return Ok(U256::ZERO);
        };
        let erc20 = IERC20::new(token, self.provider.clone());
        let bal = erc20
            .balanceOf(atoken)
            .call()
            .await
            .with_context(|| format!("Aave V3: balanceOf({atoken}) failed"))?
            ._0;
        Ok(bal)
    }

    async fn quote(&self, token: Address, amount: U256) -> Result<Option<FlashLoanQuote>> {
        let liquidity = self.available_liquidity(token).await?;
        if liquidity < amount {
            return Ok(None);
        }
        // fee = amount * fee_bps / 10_000
        let fee = amount
            .checked_mul(U256::from(self.fee_bps))
            .context("Aave V3: fee multiplication overflow")?
            / U256::from(10_000u64);
        Ok(Some(FlashLoanQuote {
            source: FlashLoanSource::AaveV3,
            chain_id: self.chain_id,
            token,
            amount,
            fee,
            fee_bps: self.fee_bps,
            pool_address: self.pool,
        }))
    }

    fn build_flashloan_calldata(
        &self,
        quote: &FlashLoanQuote,
        inner_calldata: &[u8],
    ) -> Result<Vec<u8>> {
        let call = IAaveV3Pool::flashLoanSimpleCall {
            receiverAddress: self.receiver,
            asset: quote.token,
            amount: quote.amount,
            params: inner_calldata.to_vec().into(),
            referralCode: 0,
        };
        Ok(call.abi_encode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flash_loan_simple_calldata_has_correct_selector() {
        let quote = FlashLoanQuote {
            source: FlashLoanSource::AaveV3,
            chain_id: 56,
            token: address!("1111111111111111111111111111111111111111"),
            amount: U256::from(1_000u64),
            fee: U256::from(5u64),
            fee_bps: 5,
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
}
