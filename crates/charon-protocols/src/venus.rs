//! Venus Protocol adapter (BNB Chain).
//!
//! Venus is a Compound V2 fork running on BSC. Underwater accounts are
//! surfaced via `Comptroller.getAccountLiquidity(borrower)` which returns
//! a `(errorCode, liquidity, shortfall)` tuple; a non-zero `shortfall`
//! means the account is liquidatable. The adapter translates that shape
//! into the shared `Position` type and encodes liquidation calls through
//! `VToken.liquidateBorrow(borrower, repayAmount, vTokenCollateral)`.
//!
//! All view calls accept a `BlockNumberOrTag` so the scanner can pin a
//! snapshot to an observed head and avoid oracle/exchange-rate drift
//! between reads. Internally we convert to `alloy::eips::BlockId` which
//! is the argument type the sol!-generated call builder expects.

use std::collections::HashMap;
use std::sync::Arc;

use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::primitives::{Address, U256, address};
use alloy::providers::{Provider, RootProvider};
use alloy::pubsub::PubSubFrontend;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};
use async_trait::async_trait;
use charon_core::{
    LendingProtocol, LendingProtocolError, LendingResult, LiquidationParams, Position, ProtocolId,
};
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

/// Map any internal `anyhow::Error` produced inside helper paths to the
/// `LendingProtocolError::Rpc` variant. RPC failures dominate this adapter's
/// error surface; callers that need finer distinctions should construct
/// `LendingProtocolError` directly.
fn rpc_err<E: std::fmt::Display>(e: E) -> LendingProtocolError {
    LendingProtocolError::Rpc(e.to_string())
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
    /// Underlying ERC-20 addresses currently known to the adapter.
    /// Used by `TokenMetaCache::build` to discover the set of tokens
    /// the profit gate will need metadata for. The returned vector
    /// is a point-in-time snapshot; callers should rebuild if they
    /// run past a `refresh()` boundary.
    pub async fn underlying_tokens(&self) -> Vec<Address> {
        self.snapshot
            .read()
            .await
            .underlying_to_vtoken
            .keys()
            .copied()
            .collect()
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

    /// Fetch one borrower's largest debt/collateral pair, if any, with every
    /// sub-call anchored to `block` so oracle price, exchange rate, borrow
    /// balance, and liquidity are read from the same chain state.
    ///
    /// Walks `getAssetsIn(borrower)`, reads per-vToken borrow + supply
    /// balances and oracle prices through pure view methods only
    /// (`balanceOf * exchangeRateStored / 1e18`; never `balanceOfUnderlying`
    /// which triggers `accrueInterest` and breaks on view-only endpoints).
    async fn fetch_position_inner(
        &self,
        borrower: Address,
        block: BlockNumberOrTag,
    ) -> Result<Option<Position>> {
        let block_id: BlockId = block.into();
        let snap = self.snapshot.read().await.clone();
        let comp = abi::IVenusComptroller::new(self.comptroller, self.provider.clone());

        let liq = comp
            .getAccountLiquidity(borrower)
            .block(block_id)
            .call()
            .await
            .with_context(|| format!("getAccountLiquidity({borrower}) failed"))?;
        let liquidity = liq._1;
        let shortfall = liq._2;

        let assets = comp
            .getAssetsIn(borrower)
            .block(block_id)
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

            let borrow = match vt.borrowBalanceStored(borrower).block(block_id).call().await {
                Ok(r) => r._0,
                Err(err) => {
                    warn!(%vtoken, %borrower, ?err, "borrowBalanceStored failed");
                    continue;
                }
            };
            // View-only underlying balance: vToken shares × exchangeRate / 1e18.
            let v_balance = match vt.balanceOf(borrower).block(block_id).call().await {
                Ok(r) => r._0,
                Err(err) => {
                    warn!(%vtoken, %borrower, ?err, "balanceOf failed");
                    continue;
                }
            };
            let exchange_rate = match vt.exchangeRateStored().block(block_id).call().await {
                Ok(r) => r._0,
                Err(err) => {
                    warn!(%vtoken, ?err, "exchangeRateStored failed");
                    continue;
                }
            };
            let supply = v_balance.saturating_mul(exchange_rate) / scale;

            let price = match oracle
                .getUnderlyingPrice(*vtoken)
                .block(block_id)
                .call()
                .await
            {
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
        let health_factor = compute_health_factor(total_borrow_val, liquidity, shortfall, scale);

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

/// Compute the 1e18-scaled health factor from Comptroller account-liquidity
/// values, factored out so `get_health_factor` can reuse it without walking
/// the full position-building path.
fn compute_health_factor(
    total_borrow_val: U256,
    liquidity: U256,
    shortfall: U256,
    scale: U256,
) -> U256 {
    if total_borrow_val.is_zero() {
        // No debt priced this block → treat as healthy marker.
        return scale.saturating_mul(U256::from(2u64));
    }
    if shortfall > U256::ZERO {
        let eff = total_borrow_val.saturating_sub(shortfall);
        eff.saturating_mul(scale) / total_borrow_val
    } else {
        let eff = total_borrow_val.saturating_add(liquidity);
        eff.saturating_mul(scale) / total_borrow_val
    }
}

#[async_trait]
impl LendingProtocol for VenusAdapter {
    fn id(&self) -> ProtocolId {
        ProtocolId::Venus
    }

    /// Fetch positions for every borrower concurrently via `FuturesUnordered`,
    /// with every sub-call anchored to `block` for snapshot consistency.
    /// Concurrency cap is the borrower count; each borrower still issues
    /// sequential per-vToken calls, which is the next optimization target
    /// (Multicall3 aggregate — follow-up).
    async fn fetch_positions(
        &self,
        borrowers: &[Address],
        block: BlockNumberOrTag,
    ) -> LendingResult<Vec<Position>> {
        let mut futs = FuturesUnordered::new();
        for &borrower in borrowers {
            futs.push(async move {
                (borrower, self.fetch_position_inner(borrower, block).await)
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

    /// Return the 1e18-scaled health factor for `borrower` at `block`.
    ///
    /// Uses the Comptroller's `getAccountLiquidity` directly: that call
    /// already aggregates oracle-USD collateral and borrow values across
    /// every asset the borrower has entered, so one round-trip is enough
    /// for a gating decision. No oracle / vToken fan-out is needed.
    async fn get_health_factor(
        &self,
        borrower: Address,
        block: BlockNumberOrTag,
    ) -> LendingResult<U256> {
        let block_id: BlockId = block.into();
        let comp = abi::IVenusComptroller::new(self.comptroller, self.provider.clone());
        let liq = comp
            .getAccountLiquidity(borrower)
            .block(block_id)
            .call()
            .await
            .map_err(|e| {
                LendingProtocolError::Rpc(format!("getAccountLiquidity({borrower}): {e}"))
            })?;
        let err_code = liq._0;
        let liquidity = liq._1;
        let shortfall = liq._2;
        if !err_code.is_zero() {
            return Err(LendingProtocolError::ProtocolState(format!(
                "Comptroller.getAccountLiquidity returned non-zero error code {err_code} for {borrower}"
            )));
        }

        // `getAccountLiquidity` reports only net liquidity/shortfall, not
        // total borrow value. Without the latter the HF ratio is not
        // uniquely defined, so a single aggregate call is not sufficient
        // for an oracle-exact HF. Reuse the full position walker, which
        // anchors every sub-call to `block`, and derive HF from the same
        // totals the scanner would compute.
        //
        // OPTIMIZATION: a Multicall3 batch of (borrowBalanceStored ×
        // getUnderlyingPrice) per entered market would cut this to one
        // RPC round-trip. Out of scope for this change.
        let _ = (liquidity, shortfall);
        let pos = self
            .fetch_position_inner(borrower, block)
            .await
            .map_err(rpc_err)?;
        match pos {
            Some(p) => Ok(p.health_factor),
            None => {
                // No debt → treat as very healthy (2e18). Matches the
                // convention used inside `fetch_position_inner`.
                Ok(one_e18().saturating_mul(U256::from(2u64)))
            }
        }
    }

    /// Close factor on Venus is a **global** Comptroller parameter
    /// (`closeFactorMantissa`), not per-market. We ignore `market` and
    /// return the cached value from the latest snapshot.
    ///
    /// Returns `LendingProtocolError::ProtocolState` if the snapshot is
    /// currently being refreshed (write-locked); the caller should retry.
    /// This keeps the method synchronous as the trait requires while
    /// avoiding a spinning busy-wait.
    fn get_close_factor(&self, _market: Address) -> LendingResult<U256> {
        match self.snapshot.try_read() {
            Ok(snap) => Ok(snap.close_factor_mantissa),
            Err(_) => Err(LendingProtocolError::ProtocolState(
                "Venus snapshot is being refreshed — retry".into(),
            )),
        }
    }

    /// Liquidation incentive on Venus is also a **global** Comptroller
    /// parameter (`liquidationIncentiveMantissa`), not per-market.
    /// `collateral_market` is accepted to match the trait shape.
    async fn get_liquidation_incentive(
        &self,
        _collateral_market: Address,
    ) -> LendingResult<U256> {
        Ok(self.snapshot.read().await.liquidation_incentive_mantissa)
    }

    fn get_liquidation_params(&self, position: &Position) -> LendingResult<LiquidationParams> {
        let snap = self.snapshot.try_read().map_err(|_| {
            LendingProtocolError::ProtocolState("Venus snapshot is being refreshed — retry".into())
        })?;
        let collateral_vtoken = snap
            .underlying_to_vtoken
            .get(&position.collateral_token)
            .copied()
            .ok_or_else(|| {
                LendingProtocolError::UnsupportedAsset(position.collateral_token)
            })?;
        let debt_vtoken = snap
            .underlying_to_vtoken
            .get(&position.debt_token)
            .copied()
            .ok_or_else(|| LendingProtocolError::UnsupportedAsset(position.debt_token))?;

        let scale = one_e18();
        let repay_amount = position
            .debt_amount
            .checked_mul(snap.close_factor_mantissa)
            .ok_or_else(|| {
                LendingProtocolError::ProtocolState("Venus: repay-amount overflow".into())
            })?
            / scale;

        if repay_amount.is_zero() {
            return Err(LendingProtocolError::InvalidPosition(
                "Venus: computed repay_amount is zero (debt or close_factor is zero)".into(),
            ));
        }

        Ok(LiquidationParams::Venus {
            borrower: position.borrower,
            collateral_vtoken,
            debt_vtoken,
            repay_amount,
        })
    }

    fn build_liquidation_calldata(&self, params: &LiquidationParams) -> LendingResult<Vec<u8>> {
        encode_liquidate_borrow_calldata(params)
    }
}

fn encode_liquidate_borrow_calldata(params: &LiquidationParams) -> LendingResult<Vec<u8>> {
    // `LiquidationParams` is `#[non_exhaustive]` so the pattern is refutable
    // from a downstream crate even though `Venus` is the only variant today.
    // Any non-Venus variant is a caller bug — route through the router that
    // pairs a `Venus` params struct with this adapter.
    let LiquidationParams::Venus {
        borrower,
        collateral_vtoken,
        debt_vtoken: _,
        repay_amount,
    } = params
    else {
        return Err(LendingProtocolError::ProtocolState(
            "encode_liquidate_borrow_calldata called with non-Venus LiquidationParams".into(),
        ));
    };

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
