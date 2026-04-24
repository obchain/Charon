//! Flash-loan router.
//!
//! Walks configured [`FlashLoanProvider`]s in ascending fee-rate order
//! (Aave V3 0.05% -> PancakeSwap V3 pool fee tier). Returns the first
//! source that can cover the requested borrow. If none can, returns
//! `None` — the caller skips the liquidation rather than sourcing
//! capital from elsewhere.
//!
//! BSC-only for v0.1: the two entries are Aave V3 (`flashLoanSimple`,
//! 5 bps) and PancakeSwap V3 (flash-swap, pool-tier fee). The provider
//! list is built from config so onboarding additional sources or chains
//! is a config change, not a refactor.

use std::sync::Arc;

use alloy::primitives::{Address, U256};
use charon_core::{FlashLoanProvider, FlashLoanQuote};
use tracing::{debug, info, warn};

/// Fee-priority flash-loan router.
///
/// Built once from a pre-built list of provider handles. Cloning the
/// router is cheap — providers sit behind `Arc<dyn ...>`.
///
/// Providers are sorted at construction by `fee_rate_millionths`
/// ascending; ties are broken by *best-known* available liquidity
/// descending at sort time. The liquidity snapshot is pulled once
/// during construction via [`sort_by_liquidity`] — callers that need
/// live-weighted ordering should rebuild the router.
pub struct FlashLoanRouter {
    providers: Vec<Arc<dyn FlashLoanProvider>>,
}

impl FlashLoanRouter {
    /// Construct a router, sorting providers by `fee_rate_millionths`
    /// ascending so the cheapest source is tried first.
    ///
    /// No tiebreaker is applied here because that would require an
    /// async liquidity probe at construction; see
    /// [`Self::with_liquidity_tiebreaker`] for an async constructor
    /// that breaks fee-rate ties by live available liquidity desc.
    pub fn new(mut providers: Vec<Arc<dyn FlashLoanProvider>>) -> Self {
        providers.sort_by_key(|p| p.fee_rate_millionths());
        Self { providers }
    }

    /// Like [`Self::new`] but probes each provider for
    /// `available_liquidity(token)` and uses the result as a
    /// tiebreaker when two providers advertise the same fee rate.
    /// Providers that error out are treated as zero-liquidity and
    /// sorted last within their fee-rate bucket.
    ///
    /// Order: `fee_rate_millionths ASC, available_liquidity DESC`.
    pub async fn with_liquidity_tiebreaker(
        providers: Vec<Arc<dyn FlashLoanProvider>>,
        token: Address,
    ) -> Self {
        let mut rated: Vec<(Arc<dyn FlashLoanProvider>, U256)> =
            Vec::with_capacity(providers.len());
        for p in providers {
            let liq = p.available_liquidity(token).await.unwrap_or(U256::ZERO);
            rated.push((p, liq));
        }
        rated.sort_by(|a, b| {
            a.0.fee_rate_millionths()
                .cmp(&b.0.fee_rate_millionths())
                .then_with(|| b.1.cmp(&a.1))
        });
        Self {
            providers: rated.into_iter().map(|(p, _)| p).collect(),
        }
    }

    /// Providers the router will consider, in the order it tries them.
    pub fn providers(&self) -> &[Arc<dyn FlashLoanProvider>] {
        &self.providers
    }

    /// Pick the cheapest provider that can cover `amount` of `token`.
    ///
    /// Per-provider failures (RPC error, insufficient liquidity, paused
    /// reserve) are logged and the walk continues — one dark source
    /// shouldn't block liquidation if a cheaper one can't cover but a
    /// pricier one can.
    pub async fn route(&self, token: Address, amount: U256) -> Option<FlashLoanQuote> {
        for provider in &self.providers {
            let source = provider.source();
            let fee_rate_millionths = provider.fee_rate_millionths();
            match provider.quote(token, amount).await {
                Ok(Some(quote)) => {
                    info!(
                        source = ?source,
                        fee_rate_millionths,
                        token = %token,
                        amount = %amount,
                        "flash-loan source selected"
                    );
                    return Some(quote);
                }
                Ok(None) => {
                    debug!(
                        source = ?source,
                        fee_rate_millionths,
                        token = %token,
                        amount = %amount,
                        "source skipped: insufficient liquidity"
                    );
                }
                Err(err) => {
                    warn!(
                        source = ?source,
                        fee_rate_millionths,
                        token = %token,
                        ?err,
                        "source skipped: quote failed"
                    );
                }
            }
        }
        warn!(
            token = %token,
            amount = %amount,
            provider_count = self.providers.len(),
            "no flash-loan source could cover the borrow"
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use async_trait::async_trait;
    use charon_core::{FlashLoanError, FlashLoanSource};

    /// In-memory provider for router tests — skips all RPC.
    struct StubProvider {
        source: FlashLoanSource,
        fee_rate_millionths: u32,
        liquidity: U256,
        chain: u64,
    }

    #[async_trait]
    impl FlashLoanProvider for StubProvider {
        fn source(&self) -> FlashLoanSource {
            self.source
        }
        fn chain_id(&self) -> u64 {
            self.chain
        }
        fn fee_rate_millionths(&self) -> u32 {
            self.fee_rate_millionths
        }
        async fn available_liquidity(&self, _t: Address) -> Result<U256, FlashLoanError> {
            Ok(self.liquidity)
        }
        async fn quote(
            &self,
            token: Address,
            amount: U256,
        ) -> Result<Option<FlashLoanQuote>, FlashLoanError> {
            if self.liquidity < amount {
                return Ok(None);
            }
            let fee = amount * U256::from(self.fee_rate_millionths) / U256::from(1_000_000u64);
            Ok(Some(FlashLoanQuote {
                source: self.source,
                chain_id: self.chain,
                token,
                amount,
                fee,
                fee_rate_millionths: self.fee_rate_millionths,
                pool_address: Address::ZERO,
            }))
        }
        fn build_flashloan_calldata(
            &self,
            _q: &FlashLoanQuote,
            inner: &[u8],
        ) -> Result<Vec<u8>, FlashLoanError> {
            if inner.is_empty() {
                return Err(FlashLoanError::other("empty liquidation_params"));
            }
            Ok(inner.to_vec())
        }
    }

    fn token() -> Address {
        address!("1111111111111111111111111111111111111111")
    }

    #[tokio::test]
    async fn picks_cheapest_source_with_sufficient_liquidity() {
        // Cheaper source: Aave V3 at 5 bps (500 millionths).
        let aave = Arc::new(StubProvider {
            source: FlashLoanSource::AaveV3,
            fee_rate_millionths: 500,
            liquidity: U256::from(1_000u64),
            chain: 56,
        });
        // Pricier source: PancakeSwap V3 at the 25 bps pool tier (2500 millionths).
        let pancake = Arc::new(StubProvider {
            source: FlashLoanSource::PancakeSwapV3,
            fee_rate_millionths: 2500,
            liquidity: U256::from(1_000_000u64),
            chain: 56,
        });

        // Pass them in reverse order; router should sort internally.
        let router = FlashLoanRouter::new(vec![pancake, aave]);
        let quote = router
            .route(token(), U256::from(500u64))
            .await
            .expect("route");
        assert_eq!(quote.source, FlashLoanSource::AaveV3);
        assert_eq!(quote.fee_rate_millionths, 500);
    }

    #[tokio::test]
    async fn falls_through_to_next_source_when_cheaper_has_no_liquidity() {
        // Cheaper source (Aave at 5 bps) cannot cover the ask.
        let aave = Arc::new(StubProvider {
            source: FlashLoanSource::AaveV3,
            fee_rate_millionths: 500,
            liquidity: U256::from(10u64), // too small
            chain: 56,
        });
        // Pricier fallback (PancakeSwap V3 at 25 bps) has deep liquidity.
        let pancake = Arc::new(StubProvider {
            source: FlashLoanSource::PancakeSwapV3,
            fee_rate_millionths: 2500,
            liquidity: U256::from(1_000_000u64),
            chain: 56,
        });
        let router = FlashLoanRouter::new(vec![aave, pancake]);
        let quote = router
            .route(token(), U256::from(500u64))
            .await
            .expect("route");
        assert_eq!(quote.source, FlashLoanSource::PancakeSwapV3);
    }

    #[tokio::test]
    async fn returns_none_when_no_source_has_liquidity() {
        let aave = Arc::new(StubProvider {
            source: FlashLoanSource::AaveV3,
            fee_rate_millionths: 500,
            liquidity: U256::ZERO,
            chain: 56,
        });
        let pancake = Arc::new(StubProvider {
            source: FlashLoanSource::PancakeSwapV3,
            fee_rate_millionths: 2500,
            liquidity: U256::from(100u64),
            chain: 56,
        });
        let router = FlashLoanRouter::new(vec![aave, pancake]);
        assert!(router.route(token(), U256::from(10_000u64)).await.is_none());
    }

    #[tokio::test]
    async fn returns_none_for_empty_router() {
        let router = FlashLoanRouter::new(Vec::new());
        assert!(router.route(token(), U256::from(1u64)).await.is_none());
    }

    #[tokio::test]
    async fn liquidity_tiebreaker_prefers_deeper_pool_at_same_fee() {
        // Two sources advertising the same fee-rate; tiebreaker should
        // push the deeper-liquidity one to the front.
        let shallow = Arc::new(StubProvider {
            source: FlashLoanSource::AaveV3,
            fee_rate_millionths: 500,
            liquidity: U256::from(1_000u64),
            chain: 56,
        });
        let deep = Arc::new(StubProvider {
            source: FlashLoanSource::PancakeSwapV3,
            fee_rate_millionths: 500,
            liquidity: U256::from(1_000_000u64),
            chain: 56,
        });
        let router = FlashLoanRouter::with_liquidity_tiebreaker(vec![shallow, deep], token()).await;
        assert_eq!(router.providers()[0].source(), FlashLoanSource::PancakeSwapV3);
        assert_eq!(router.providers()[1].source(), FlashLoanSource::AaveV3);
    }
}
