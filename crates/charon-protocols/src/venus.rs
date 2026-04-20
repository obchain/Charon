//! Venus Protocol adapter (BNB Chain).
//!
//! Venus is a Compound V2 fork running on BSC. Underwater accounts are
//! surfaced via `Comptroller.getAccountLiquidity(borrower)` which returns
//! a `(errorCode, liquidity, shortfall)` tuple; a non-zero `shortfall`
//! means the account is liquidatable. The adapter translates that shape
//! into the shared `Position` type and encodes liquidation calls through
//! `VToken.liquidateBorrow(borrower, repayAmount, vTokenCollateral)`.
//!
//! This file is a scaffold — the `LendingProtocol` impl lands alongside
//! the provider wiring in the next commit.

use alloy::primitives::Address;
use alloy::sol;

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

            /// Bonus paid to liquidators (scaled 1e18, e.g. 1.1e18 = 10%).
            function liquidationIncentiveMantissa() external view returns (uint256);

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

/// Venus adapter — see module docs.
///
/// Holds the Comptroller address for the chain it's running on. Further
/// fields (pub-sub provider, cached vToken list, price oracle address)
/// are added alongside the provider wiring in the next commit.
#[derive(Debug, Clone)]
pub struct VenusAdapter {
    /// Address of the Venus Unitroller (main Comptroller proxy).
    pub comptroller: Address,
}

impl VenusAdapter {
    /// Build an adapter pointing at the given Venus Comptroller.
    ///
    /// This is intentionally minimal for now; the async constructor that
    /// also discovers vToken markets and the price oracle lands in the
    /// next commit.
    pub fn new(comptroller: Address) -> Self {
        Self { comptroller }
    }
}
