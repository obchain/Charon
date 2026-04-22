//! Venus Protocol adapter (BNB Chain).
//!
//! Venus is a Compound V2 fork running on BSC. Underwater accounts are
//! surfaced via `Comptroller.getAccountLiquidity(borrower)` which returns
//! a `(errorCode, liquidity, shortfall)` tuple; a non-zero `shortfall`
//! means the account is liquidatable. The adapter translates that shape
//! into the shared `Position` type and encodes liquidation calls through
//! `VToken.liquidateBorrow(borrower, repayAmount, vTokenCollateral)`.

use std::collections::HashMap;
use std::sync::Arc;

use alloy::primitives::{Address, U256, address};
use alloy::providers::{Provider, RootProvider};
use alloy::pubsub::PubSubFrontend;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};
use async_trait::async_trait;
use charon_core::{LendingProtocol, LiquidationParams, Position, ProtocolId};
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// vBNB does not implement `underlying()` — BSC's native BNB market. Map it
/// to the canonical Wrapped BNB token so oracle and router paths still work.
const VBNB: Address = address!("A07c5b74C9B40447a954e1466938b865b6BBea36");
const WBNB: Address = address!("bb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c");

/// 1e18 — reused constant to avoid re-computing inside tight loops.
fn one_e18() -> U256 {
    U256::from(10u64).pow(U256::from(18u64))
}

/// On-chain ABI bindings used by the Venus adapter.
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

            /// Liquidation incentive (bonus) paid to liquidators, scaled 1e18.
            /// 1.1e18 = 10% bonus. Governance-set; refreshed on demand.
            function liquidationIncentiveMantissa() external view returns (uint256);

            /// Address of the Venus price oracle.
            function oracle() external view returns (address);
        }

        /// Venus market token — holds collateral and tracks borrow state.
        ///
        /// Only pure view methods are exposed so every scan-path call is
        /// safe on rate-limited proxies that reject state-mutating
        /// `eth_call`s.
        #[sol(rpc)]
        interface IVToken {
            /// Underlying ERC-20 address (missing on `vBNB` — native BNB).
            function underlying() external view returns (address);

            /// vToken share balance of `owner`.
            function balanceOf(address owner) external view returns (uint256);

            /// Cached borrow balance — fast but stale by up to one accrual.
            function borrowBalanceStored(address account)
                external view returns (uint256);

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

pub type ChainProvider = Arc<RootProvider<PubSubFrontend>>;

/// Mutable snapshot the adapter refreshes from the Comptroller on demand.
#[derive(Debug, Clone)]
struct VenusSnapshot {
    oracle: Address,
    markets: Vec<Address>,
    close_factor_mantissa: U256,
    liquidation_incentive_mantissa: U256,
    underlying_to_vtoken: HashMap<Address, Address>,
    vtoken_to_underlying: HashMap<Address, Address>,
}

/// Venus adapter — see module docs.
#[derive(Debug, Clone)]
pub struct VenusAdapter {
    comptroller: Address,
    chain_id: u64,
    snapshot: Arc<RwLock<VenusSnapshot>>,
    provider: ChainProvider,
}

impl VenusAdapter {
    /// Connect to the Venus Comptroller and snapshot its market config.
    pub async fn connect(provider: ChainProvider, comptroller: Address) -> Result<Self> {
        debug!(%comptroller, "connecting Venus adapter");

        let chain_id = provider
            .get_chain_id()
            .await
            .context("Venus: eth_chainId failed")?;

        let snapshot = Self::take_snapshot(&provider, comptroller).await?;
        info!(
            %comptroller,
            oracle = %snapshot.oracle,
            chain_id,
            market_count = snapshot.markets.len(),
            mapped_markets = snapshot.underlying_to_vtoken.len(),
            close_factor = %snapshot.close_factor_mantissa,
            liquidation_incentive = %snapshot.liquidation_incentive_mantissa,
            "Venus adapter connected"
        );

        Ok(Self {
            comptroller,
            chain_id,
            snapshot: Arc::new(RwLock::new(snapshot)),
            provider,
        })
    }

    /// Re-query the Comptroller for oracle / close factor / incentive /
    /// market list and rebuild the lookup maps. Safe to call on a timer
    /// or in response to a `NewMarket` / `NewPriceOracle` event.
    pub async fn refresh(&self) -> Result<()> {
        let fresh = Self::take_snapshot(&self.provider, self.comptroller).await?;
        let mut guard = self.snapshot.write().await;
        *guard = fresh;
        debug!("Venus snapshot refreshed");
        Ok(())
    }

    async fn take_snapshot(
        provider: &ChainProvider,
        comptroller: Address,
    ) -> Result<VenusSnapshot> {
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
        let liquidation_incentive_mantissa = comp
            .liquidationIncentiveMantissa()
            .call()
            .await
            .context("Venus: Comptroller.liquidationIncentiveMantissa() failed")?
            ._0;

        let mut underlying_to_vtoken = HashMap::with_capacity(markets.len());
        let mut vtoken_to_underlying = HashMap::with_capacity(markets.len());
        for &vtoken in &markets {
            if vtoken == VBNB {
                underlying_to_vtoken.insert(WBNB, VBNB);
                vtoken_to_underlying.insert(VBNB, WBNB);
                continue;
            }
            let vt = abi::IVToken::new(vtoken, provider.clone());
            match vt.underlying().call().await {
                Ok(r) => {
                    underlying_to_vtoken.insert(r._0, vtoken);
                    vtoken_to_underlying.insert(vtoken, r._0);
                }
                Err(err) => {
                    warn!(
                        %vtoken, err = ?err,
                        "vToken has no underlying() and is not the known vBNB market — scanner will ignore it"
                    );
                }
            }
        }

        Ok(VenusSnapshot {
            oracle,
            markets,
            close_factor_mantissa,
            liquidation_incentive_mantissa,
            underlying_to_vtoken,
            vtoken_to_underlying,
        })
    }

    /// Read accessors for downstream crates. Held behind an async RwLock
    /// because `refresh()` swaps the snapshot atomically.
    pub async fn markets(&self) -> Vec<Address> {
        self.snapshot.read().await.markets.clone()
    }
    pub async fn oracle(&self) -> Address {
        self.snapshot.read().await.oracle
    }
    pub async fn close_factor_mantissa(&self) -> U256 {
        self.snapshot.read().await.close_factor_mantissa
    }
    pub async fn liquidation_incentive_mantissa(&self) -> U256 {
        self.snapshot.read().await.liquidation_incentive_mantissa
    }

    /// Fetch one borrower's largest debt/collateral pair, if any.
    ///
    /// Walks `getAssetsIn(borrower)`, reads per-vToken borrow + supply
    /// balances and oracle prices through pure view methods only
    /// (`balanceOf * exchangeRateStored / 1e18`; never `balanceOfUnderlying`
    /// which triggers `accrueInterest` and breaks on view-only endpoints).
    async fn fetch_position_inner(&self, borrower: Address) -> Result<Option<Position>> {
        let snap = self.snapshot.read().await.clone();
        let comp = abi::IVenusComptroller::new(self.comptroller, self.provider.clone());

        let liq = comp
            .getAccountLiquidity(borrower)
            .call()
            .await
            .with_context(|| format!("getAccountLiquidity({borrower}) failed"))?;
        let liquidity = liq._1;
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

        let oracle = abi::IVenusOracle::new(snap.oracle, self.provider.clone());
        let scale = one_e18();

        let mut best_debt: Option<(Address, U256, U256)> = None;
        let mut best_coll: Option<(Address, U256, U256)> = None;
        let mut total_borrow_val = U256::ZERO;

        for vtoken in &assets {
            let Some(&underlying) = snap.vtoken_to_underlying.get(vtoken) else {
                warn!(%vtoken, "vToken not in snapshot — skipping (stale snapshot?)");
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
            // View-only underlying balance: vToken shares × exchangeRate / 1e18.
            let v_balance = match vt.balanceOf(borrower).call().await {
                Ok(r) => r._0,
                Err(err) => {
                    warn!(%vtoken, %borrower, ?err, "balanceOf failed");
                    continue;
                }
            };
            let exchange_rate = match vt.exchangeRateStored().call().await {
                Ok(r) => r._0,
                Err(err) => {
                    warn!(%vtoken, ?err, "exchangeRateStored failed");
                    continue;
                }
            };
            let supply = v_balance.saturating_mul(exchange_rate) / scale;

            let price = match oracle.getUnderlyingPrice(*vtoken).call().await {
                Ok(r) => r._0,
                Err(err) => {
                    warn!(%vtoken, ?err, "oracle.getUnderlyingPrice failed");
                    continue;
                }
            };

            let borrow_val = borrow.saturating_mul(price);
            let supply_val = supply.saturating_mul(price);
            total_borrow_val = total_borrow_val.saturating_add(borrow_val);

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

        // Health factor (1e18-scaled) derived from Comptroller's own
        // liquidity / shortfall values. Both are oracle-USD magnitudes.
        // HF = effective_collateral / total_borrow_val:
        //   shortfall > 0:  eff_coll = total_borrow_val - shortfall
        //   otherwise:      eff_coll = total_borrow_val + liquidity
        let health_factor = if total_borrow_val.is_zero() {
            // No debt priced this block → treat as healthy marker.
            scale.saturating_mul(U256::from(2u64))
        } else if shortfall > U256::ZERO {
            let eff = total_borrow_val.saturating_sub(shortfall);
            eff.saturating_mul(scale) / total_borrow_val
        } else {
            let eff = total_borrow_val.saturating_add(liquidity);
            eff.saturating_mul(scale) / total_borrow_val
        };

        // Liquidation bonus bps from live snapshot.
        // mantissa = 1e18 + bonus → bps = (mantissa - 1e18) / 1e14
        let incentive = snap.liquidation_incentive_mantissa;
        let bonus_1e18 = incentive.saturating_sub(scale);
        let one_e14 = U256::from(10u64).pow(U256::from(14u64));
        let liquidation_bonus_bps = u16::try_from(bonus_1e18 / one_e14)
            .unwrap_or(0);

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

    /// Fetch positions for every borrower concurrently via `FuturesUnordered`.
    /// Concurrency cap is the borrower count; each borrower still issues
    /// sequential per-vToken calls, which is the next optimization target
    /// (Multicall3 aggregate — follow-up).
    async fn fetch_positions(&self, borrowers: &[Address]) -> Result<Vec<Position>> {
        let mut futs = FuturesUnordered::new();
        for &borrower in borrowers {
            futs.push(async move {
                (borrower, self.fetch_position_inner(borrower).await)
            });
        }
        let mut out = Vec::with_capacity(borrowers.len());
        while let Some((borrower, res)) = futs.next().await {
            match res {
                Ok(Some(pos)) => out.push(pos),
                Ok(None) => {}
                Err(err) => warn!(%borrower, ?err, "Venus fetch_position failed, skipping"),
            }
        }
        Ok(out)
    }

    fn get_liquidation_params(&self, position: &Position) -> Result<LiquidationParams> {
        let snap = self
            .snapshot
            .try_read()
            .context("Venus: snapshot is being refreshed — retry")?;
        let collateral_vtoken = snap
            .underlying_to_vtoken
            .get(&position.collateral_token)
            .copied()
            .with_context(|| {
                format!(
                    "Venus: no vToken mapped for collateral underlying {}",
                    position.collateral_token
                )
            })?;
        let debt_vtoken = snap
            .underlying_to_vtoken
            .get(&position.debt_token)
            .copied()
            .with_context(|| {
                format!(
                    "Venus: no vToken mapped for debt underlying {}",
                    position.debt_token
                )
            })?;

        let scale = one_e18();
        let repay_amount = position
            .debt_amount
            .checked_mul(snap.close_factor_mantissa)
            .context("Venus: repay-amount overflow")?
            / scale;

        if repay_amount.is_zero() {
            anyhow::bail!("Venus: computed repay_amount is zero (debt or close_factor is zero)");
        }

        Ok(LiquidationParams::Venus {
            borrower: position.borrower,
            collateral_vtoken,
            debt_vtoken,
            repay_amount,
        })
    }

    fn build_liquidation_calldata(&self, params: &LiquidationParams) -> Result<Vec<u8>> {
        encode_liquidate_borrow_calldata(params)
    }
}

fn encode_liquidate_borrow_calldata(params: &LiquidationParams) -> Result<Vec<u8>> {
    let LiquidationParams::Venus {
        borrower,
        collateral_vtoken,
        debt_vtoken: _,
        repay_amount,
    } = params;

    let call = abi::IVToken::liquidateBorrowCall {
        borrower: *borrower,
        repayAmount: *repay_amount,
        vTokenCollateral: *collateral_vtoken,
    };
    Ok(call.abi_encode())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn liquidate_borrow_calldata_has_correct_selector() {
        let params = LiquidationParams::Venus {
            borrower: address!("1111111111111111111111111111111111111111"),
            collateral_vtoken: address!("2222222222222222222222222222222222222222"),
            debt_vtoken: address!("3333333333333333333333333333333333333333"),
            repay_amount: U256::from(42u64),
        };
        let data = encode_liquidate_borrow_calldata(&params).expect("encode");

        assert_eq!(
            &data[..4],
            &abi::IVToken::liquidateBorrowCall::SELECTOR,
            "selector mismatch — check ABI definition order"
        );
        assert_eq!(data.len(), 4 + 32 * 3);
    }
}
