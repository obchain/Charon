//! Venus Protocol adapter (BNB Chain).
//!
//! Venus is a Compound V2 fork running on BSC. Underwater accounts are
//! surfaced via `Comptroller.getAccountLiquidity(borrower)` which returns
//! a `(errorCode, liquidity, shortfall)` tuple; a non-zero `shortfall`
//! means the account is liquidatable. The adapter translates that shape
//! into the shared `Position` type and encodes liquidation calls through
//! `VToken.liquidateBorrow(borrower, repayAmount, vTokenCollateral)`.
//!
//! The `LendingProtocol` impl lands in the next commit — this file
//! wires up the async constructor that snapshots market config from the
//! Comptroller (markets, oracle, close factor, liquidation incentive).

use std::sync::Arc;

use alloy::primitives::{Address, U256};
use alloy::providers::RootProvider;
use alloy::pubsub::PubSubFrontend;
use alloy::sol;
use anyhow::{Context, Result};
use tracing::{debug, info};

/// On-chain ABI bindings used by the Venus adapter.
///
/// `#[sol(rpc)]` generates typed `new(address, provider)` constructors so
/// each call — `getAccountLiquidity`, `liquidateBorrow`, … — is one
/// method on the returned instance, with arguments and return values
/// decoded through `alloy`'s codec.
///
/// Method surface is kept to exactly what the scanner and executor need;
/// we add more entries here as downstream code demands them.
pub mod abi {
    use super::sol;

    sol! {
        /// Venus Unitroller / Comptroller — risk engine and market registry.
        #[sol(rpc)]
        interface IVenusComptroller {
            /// Returns `(errorCode, liquidity, shortfall)`. `shortfall > 0`
            /// means the account can be liquidated.
            function getAccountLiquidity(address account)
                external view returns (uint256, uint256, uint256);

            /// vTokens the account has entered as collateral.
            function getAssetsIn(address account)
                external view returns (address[] memory);

            /// All vToken markets registered on this Comptroller.
            function getAllMarkets()
                external view returns (address[] memory);

            /// Max fraction of debt liquidatable per call (scaled 1e18).
            function closeFactorMantissa() external view returns (uint256);

            /// Address of the Venus price oracle.
            function oracle() external view returns (address);
        }

        /// Venus market token — holds collateral and tracks borrow state.
        ///
        /// Mutating methods (`borrowBalanceCurrent`, `balanceOfUnderlying`)
        /// accrue interest before returning; we call them via `eth_call`
        /// so state is simulated, not committed.
        #[sol(rpc)]
        interface IVToken {
            /// Underlying ERC-20 address (missing on `vBNB` — native wrapped).
            function underlying() external view returns (address);

            /// vToken share balance of `owner`.
            function balanceOf(address owner) external view returns (uint256);

            /// Cached borrow balance — fast but stale by up to one accrual.
            function borrowBalanceStored(address account)
                external view returns (uint256);

            /// Current borrow balance with interest accrued.
            function borrowBalanceCurrent(address account) external returns (uint256);

            /// Collateral expressed in underlying units, interest-accrued.
            function balanceOfUnderlying(address owner) external returns (uint256);

            /// vToken → underlying exchange rate (scaled 1e18 + underlying decimals).
            function exchangeRateStored() external view returns (uint256);

            function decimals() external view returns (uint8);
            function symbol() external view returns (string memory);

            /// Repay `repayAmount` of the borrower's debt and seize collateral
            /// in `vTokenCollateral`. Called by `CharonLiquidator.sol` inside
            /// the flash-loan callback.
            function liquidateBorrow(
                address borrower,
                uint256 repayAmount,
                address vTokenCollateral
            ) external returns (uint256);
        }

        /// Venus price oracle — returns USD price per vToken's underlying.
        #[sol(rpc)]
        interface IVenusOracle {
            /// Price scaled by `1e(36 - underlyingDecimals)` (Compound convention).
            function getUnderlyingPrice(address vToken)
                external view returns (uint256);
        }
    }
}

/// Shared pub-sub provider — adapters are cheap to clone and keep their
/// own `Arc` so the scanner can hand out multiple adapters without
/// re-opening a WebSocket per protocol.
pub type ChainProvider = Arc<RootProvider<PubSubFrontend>>;

/// Venus adapter — see module docs.
#[derive(Debug, Clone)]
pub struct VenusAdapter {
    /// Address of the Venus Unitroller (main Comptroller proxy).
    pub comptroller: Address,
    /// Price oracle address, discovered from the Comptroller.
    pub oracle: Address,
    /// vToken markets registered on the Comptroller at connect time.
    pub markets: Vec<Address>,
    /// Close factor (1e18-scaled fraction of debt liquidatable per call).
    pub close_factor_mantissa: U256,
    /// Shared pub-sub provider for all downstream RPC calls.
    provider: ChainProvider,
}

impl VenusAdapter {
    /// Connect to the Venus Comptroller and snapshot its market config.
    ///
    /// Performs three read-only RPCs in sequence: `oracle`,
    /// `getAllMarkets`, `closeFactorMantissa`. These values are static
    /// enough over a bot's lifetime that caching them at connect time
    /// saves one round-trip per block without meaningful staleness risk
    /// (Venus governance updates are rare and observable).
    ///
    /// Per-market liquidation incentive is resolved lazily when a
    /// liquidation is being built, because Venus's Diamond Comptroller
    /// exposes it per-vToken rather than as a global constant.
    pub async fn connect(provider: ChainProvider, comptroller: Address) -> Result<Self> {
        debug!(%comptroller, "connecting Venus adapter");

        let comp = abi::IVenusComptroller::new(comptroller, provider.clone());

        let oracle = comp
            .oracle()
            .call()
            .await
            .context("Venus: Comptroller.oracle() failed")?
            ._0;

        let markets = comp
            .getAllMarkets()
            .call()
            .await
            .context("Venus: Comptroller.getAllMarkets() failed")?
            ._0;

        let close_factor_mantissa = comp
            .closeFactorMantissa()
            .call()
            .await
            .context("Venus: Comptroller.closeFactorMantissa() failed")?
            ._0;

        info!(
            %comptroller,
            %oracle,
            market_count = markets.len(),
            close_factor = %close_factor_mantissa,
            "Venus adapter connected"
        );

        Ok(Self {
            comptroller,
            oracle,
            markets,
            close_factor_mantissa,
            provider,
        })
    }

    /// Borrow the shared provider — used by downstream call-builders
    /// inside the `LendingProtocol` impl (next commit).
    #[allow(dead_code)]
    pub(crate) fn provider(&self) -> &ChainProvider {
        &self.provider
    }
}
