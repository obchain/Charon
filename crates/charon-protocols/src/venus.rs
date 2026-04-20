//! Venus Protocol adapter (BNB Chain).
//!
//! Venus is a Compound V2 fork running on BSC. Underwater accounts are
//! surfaced via `Comptroller.getAccountLiquidity(borrower)` which returns
//! a `(errorCode, liquidity, shortfall)` tuple; a non-zero `shortfall`
//! means the account is liquidatable. The adapter translates that shape
//! into the shared `Position` type and encodes liquidation calls through
//! `VToken.liquidateBorrow(borrower, repayAmount, vTokenCollateral)`.
//!
//! The liquidation-calldata side of the [`LendingProtocol`] impl lands in
//! the next commit; this file covers position discovery and the
//! health-factor synthesis.

use std::collections::HashMap;
use std::sync::Arc;

use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, RootProvider};
use alloy::pubsub::PubSubFrontend;
use alloy::sol;
use anyhow::{Context, Result};
use async_trait::async_trait;
use charon_core::{LendingProtocol, LiquidationParams, Position, ProtocolId};
use tracing::{debug, info, warn};

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
    /// Chain id of the network this adapter runs on (56 on BSC).
    pub chain_id: u64,
    /// `underlying ERC-20 → vToken` lookup, built at connect time.
    pub underlying_to_vtoken: HashMap<Address, Address>,
    /// `vToken → underlying ERC-20` lookup (reverse of the above).
    pub vtoken_to_underlying: HashMap<Address, Address>,
    /// Shared pub-sub provider for all downstream RPC calls.
    provider: ChainProvider,
}

impl VenusAdapter {
    /// Connect to the Venus Comptroller and snapshot its market config.
    ///
    /// On top of the three Comptroller reads (`oracle`, `getAllMarkets`,
    /// `closeFactorMantissa`) this also walks every vToken to resolve
    /// its `underlying()` ERC-20 address and build both directions of
    /// the lookup map. vToken contracts whose `underlying()` reverts
    /// (e.g. `vBNB`, which wraps native BNB) are skipped — that market
    /// is simply unavailable to the adapter until native wrapping lands.
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

        let chain_id = provider
            .get_chain_id()
            .await
            .context("Venus: eth_chainId failed")?;

        let mut underlying_to_vtoken = HashMap::with_capacity(markets.len());
        let mut vtoken_to_underlying = HashMap::with_capacity(markets.len());
        for &vtoken in &markets {
            let vt = abi::IVToken::new(vtoken, provider.clone());
            match vt.underlying().call().await {
                Ok(r) => {
                    underlying_to_vtoken.insert(r._0, vtoken);
                    vtoken_to_underlying.insert(vtoken, r._0);
                }
                Err(err) => {
                    debug!(
                        %vtoken, err = ?err,
                        "vToken has no underlying() — likely native-wrapping market (skipped)"
                    );
                }
            }
        }

        info!(
            %comptroller,
            %oracle,
            chain_id,
            market_count = markets.len(),
            mapped_markets = underlying_to_vtoken.len(),
            close_factor = %close_factor_mantissa,
            "Venus adapter connected"
        );

        Ok(Self {
            comptroller,
            oracle,
            markets,
            close_factor_mantissa,
            chain_id,
            underlying_to_vtoken,
            vtoken_to_underlying,
            provider,
        })
    }

    /// Fetch one borrower's largest debt/collateral pair, if any.
    ///
    /// Walks `getAssetsIn(borrower)`, reads per-vToken borrow + supply
    /// balances and oracle prices, and picks the single biggest debt
    /// vToken plus the single biggest collateral vToken. Returns `None`
    /// when the borrower has no positions or has missing price data on
    /// every asset. Per-asset errors are logged but non-fatal so one
    /// broken market doesn't blank the entire account.
    async fn fetch_position_inner(&self, borrower: Address) -> Result<Option<Position>> {
        let comp = abi::IVenusComptroller::new(self.comptroller, self.provider.clone());

        let liq = comp
            .getAccountLiquidity(borrower)
            .call()
            .await
            .with_context(|| format!("getAccountLiquidity({borrower}) failed"))?;
        let shortfall = liq._2;

        let assets = comp
            .getAssetsIn(borrower)
            .call()
            .await
            .with_context(|| format!("getAssetsIn({borrower}) failed"))?
            ._0;
        if assets.is_empty() {
            return Ok(None);
        }

        let oracle = abi::IVenusOracle::new(self.oracle, self.provider.clone());

        // (underlying address, amount in underlying units, rough USD value)
        // USD value is a scaled magnitude used only for ranking, not reported.
        let mut best_debt: Option<(Address, U256, U256)> = None;
        let mut best_coll: Option<(Address, U256, U256)> = None;

        for vtoken in &assets {
            let Some(&underlying) = self.vtoken_to_underlying.get(vtoken) else {
                continue;
            };
            let vt = abi::IVToken::new(*vtoken, self.provider.clone());

            let borrow = match vt.borrowBalanceStored(borrower).call().await {
                Ok(r) => r._0,
                Err(err) => {
                    warn!(%vtoken, %borrower, ?err, "borrowBalanceStored failed");
                    continue;
                }
            };
            let supply = match vt.balanceOfUnderlying(borrower).call().await {
                Ok(r) => r._0,
                Err(err) => {
                    warn!(%vtoken, %borrower, ?err, "balanceOfUnderlying failed");
                    continue;
                }
            };
            let price = match oracle.getUnderlyingPrice(*vtoken).call().await {
                Ok(r) => r._0,
                Err(err) => {
                    warn!(%vtoken, ?err, "oracle.getUnderlyingPrice failed");
                    continue;
                }
            };

            let borrow_val = borrow.saturating_mul(price);
            let supply_val = supply.saturating_mul(price);

            if borrow > U256::ZERO && best_debt.as_ref().is_none_or(|x| borrow_val > x.2) {
                best_debt = Some((underlying, borrow, borrow_val));
            }
            if supply > U256::ZERO && best_coll.as_ref().is_none_or(|x| supply_val > x.2) {
                best_coll = Some((underlying, supply, supply_val));
            }
        }

        let Some((debt_token, debt_amount, _)) = best_debt else {
            return Ok(None);
        };
        let Some((collateral_token, collateral_amount, _)) = best_coll else {
            return Ok(None);
        };

        // Binary health factor: 0 when Venus reports shortfall (fully
        // liquidatable), otherwise 2e18 as a healthy marker. The scanner
        // only needs the `< 1e18` predicate to bucket positions; precise
        // HF arithmetic is a follow-up (#9).
        let one_e18 = U256::from(10u64).pow(U256::from(18u64));
        let health_factor = if shortfall > U256::ZERO {
            U256::ZERO
        } else {
            one_e18 * U256::from(2u64)
        };

        // Placeholder bonus — Venus per-market liquidation incentive is
        // resolved in Part E when we build the actual liquidation call.
        let liquidation_bonus_bps = 1000;

        Ok(Some(Position {
            protocol: ProtocolId::Venus,
            chain_id: self.chain_id,
            borrower,
            collateral_token,
            debt_token,
            collateral_amount,
            debt_amount,
            health_factor,
            liquidation_bonus_bps,
        }))
    }

    /// Borrow the shared provider — used by downstream call-builders
    /// inside the `LendingProtocol` impl.
    #[allow(dead_code)]
    pub(crate) fn provider(&self) -> &ChainProvider {
        &self.provider
    }
}

#[async_trait]
impl LendingProtocol for VenusAdapter {
    fn id(&self) -> ProtocolId {
        ProtocolId::Venus
    }

    async fn fetch_positions(&self, borrowers: &[Address]) -> Result<Vec<Position>> {
        let mut out = Vec::with_capacity(borrowers.len());
        for &borrower in borrowers {
            match self.fetch_position_inner(borrower).await {
                Ok(Some(pos)) => out.push(pos),
                Ok(None) => {}
                Err(err) => warn!(%borrower, ?err, "Venus fetch_position failed, skipping"),
            }
        }
        Ok(out)
    }

    fn get_liquidation_params(&self, _position: &Position) -> Result<LiquidationParams> {
        anyhow::bail!("Venus::get_liquidation_params lands in the next #8 commit")
    }

    fn build_liquidation_calldata(&self, _params: &LiquidationParams) -> Result<Vec<u8>> {
        anyhow::bail!("Venus::build_liquidation_calldata lands in the next #8 commit")
    }
}
