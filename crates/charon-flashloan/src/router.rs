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
    /// Per-request ranking: each provider is asked once for
    /// `available_liquidity(token)`; the result feeds
    /// [`FlashLoanProvider::effective_fee_millionths`] to compute an
    /// amount-aware effective fee that reflects the source's
    /// utilisation curve (Aave V3 adapter overrides with a
    /// `amount * 1e6 / liquidity` penalty). Providers are then sorted
    /// by effective fee ascending; ties break on the static
    /// `fee_rate_millionths` and finally on advertised liquidity
    /// descending.
    ///
    /// Liquidity-probe failures used to be silently coerced to zero,
    /// which masked a transient RPC blip as "the pool is empty" and
    /// dropped the borrow. They now surface as a `warn` and the
    /// provider is pushed to the back of the rank with `liquidity =
    /// U256::MAX` for the static-fee fallback so the router still
    /// tries to quote against it; the `quote` call itself enforces
    /// the real coverage check.
    ///
    /// Per-provider quote failures (RPC error, insufficient liquidity,
    /// paused reserve) are logged and the walk continues — one dark
    /// source shouldn't block liquidation if a cheaper one can't
    /// cover but a pricier one can.
    pub async fn route(&self, token: Address, amount: U256) -> Option<FlashLoanQuote> {
        // Per-request liquidity probe + effective-fee ranking.
        let mut ranked: Vec<(Arc<dyn FlashLoanProvider>, u32, u32, U256)> =
            Vec::with_capacity(self.providers.len());
        for provider in &self.providers {
            let liquidity = match provider.available_liquidity(token).await {
                Ok(l) => l,
                Err(err) => {
                    warn!(
                        source = ?provider.source(),
                        token = %token,
                        ?err,
                        "available_liquidity failed — provider kept in rank with U256::MAX so a transient blip does not silently disqualify it"
                    );
                    U256::MAX
                }
            };
            let effective = provider.effective_fee_millionths(token, amount, liquidity);
            let static_fee = provider.fee_rate_millionths();
            ranked.push((provider.clone(), effective, static_fee, liquidity));
        }
        // Sort: effective fee ASC, static fee ASC, liquidity DESC.
        ranked.sort_by(|a, b| {
            a.1.cmp(&b.1)
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| b.3.cmp(&a.3))
        });

        for (provider, effective, static_fee, _liquidity) in ranked {
            let source = provider.source();
            match provider.quote(token, amount).await {
                Ok(Some(quote)) => {
                    info!(
                        source = ?source,
                        effective_fee_millionths = effective,
                        fee_rate_millionths = static_fee,
                        token = %token,
                        amount = %amount,
                        "flash-loan source selected"
                    );
                    return Some(quote);
                }
                Ok(None) => {
                    debug!(
                        source = ?source,
                        effective_fee_millionths = effective,
                        fee_rate_millionths = static_fee,
                        token = %token,
                        amount = %amount,
                        "source skipped: insufficient liquidity"
                    );
                }
                Err(err) => {
                    warn!(
                        source = ?source,
                        effective_fee_millionths = effective,
                        fee_rate_millionths = static_fee,
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
        /// When `true`, `available_liquidity` returns
        /// [`FlashLoanError::Rpc`] so the router tests can exercise
        /// the transient-blip fall-through.
        liquidity_errors: bool,
        /// Optional utilisation penalty: when `Some(num)`, override
        /// `effective_fee_millionths` with `fee_rate +
        /// amount * num / liquidity`. Lets a single test pick which
        /// provider models slippage without dragging in the Aave
        /// adapter for unit coverage.
        utilisation_num: Option<u64>,
    }

    impl Default for StubProvider {
        fn default() -> Self {
            Self {
                source: FlashLoanSource::AaveV3,
                fee_rate_millionths: 0,
                liquidity: U256::ZERO,
                chain: 56,
                liquidity_errors: false,
                utilisation_num: None,
            }
        }
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
        fn effective_fee_millionths(
            &self,
            _token: Address,
            amount: U256,
            liquidity: U256,
        ) -> u32 {
            match self.utilisation_num {
                Some(num) if !liquidity.is_zero() && liquidity != U256::MAX => {
                    let penalty = amount.saturating_mul(U256::from(num)) / liquidity;
                    let penalty_u32 = u32::try_from(penalty).unwrap_or(u32::MAX);
                    self.fee_rate_millionths.saturating_add(penalty_u32)
                }
                _ => self.fee_rate_millionths,
            }
        }
        async fn available_liquidity(&self, _t: Address) -> Result<U256, FlashLoanError> {
            if self.liquidity_errors {
                return Err(FlashLoanError::rpc("simulated transient blip"));
            }
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
            ..StubProvider::default()
        });
        // Pricier source: PancakeSwap V3 at the 25 bps pool tier (2500 millionths).
        let pancake = Arc::new(StubProvider {
            source: FlashLoanSource::PancakeSwapV3,
            fee_rate_millionths: 2500,
            liquidity: U256::from(1_000_000u64),
            chain: 56,
            ..StubProvider::default()
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
            ..StubProvider::default()
        });
        // Pricier fallback (PancakeSwap V3 at 25 bps) has deep liquidity.
        let pancake = Arc::new(StubProvider {
            source: FlashLoanSource::PancakeSwapV3,
            fee_rate_millionths: 2500,
            liquidity: U256::from(1_000_000u64),
            chain: 56,
            ..StubProvider::default()
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
            ..StubProvider::default()
        });
        let pancake = Arc::new(StubProvider {
            source: FlashLoanSource::PancakeSwapV3,
            fee_rate_millionths: 2500,
            liquidity: U256::from(100u64),
            chain: 56,
            ..StubProvider::default()
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
            ..StubProvider::default()
        });
        let deep = Arc::new(StubProvider {
            source: FlashLoanSource::PancakeSwapV3,
            fee_rate_millionths: 500,
            liquidity: U256::from(1_000_000u64),
            chain: 56,
            ..StubProvider::default()
        });
        let router = FlashLoanRouter::with_liquidity_tiebreaker(vec![shallow, deep], token()).await;
        assert_eq!(
            router.providers()[0].source(),
            FlashLoanSource::PancakeSwapV3
        );
        assert_eq!(router.providers()[1].source(), FlashLoanSource::AaveV3);
    }

    /// High-utilisation flip (#352): a low-fee but shallow source
    /// loses to a slightly higher-fee but deep source when the
    /// borrow drains a meaningful slice of the shallow pool.
    #[tokio::test]
    async fn high_utilisation_flips_selection_to_deeper_pool() {
        // Aave-like: 5 bps (500 millionths), liquidity 1_000.
        // Utilisation penalty num = 1_000_000 → at amount = 600 the
        // effective fee becomes 500 + (600 * 1_000_000 / 1_000) =
        // 500 + 600_000 = 600_500 millionths. Crushed.
        let shallow = Arc::new(StubProvider {
            source: FlashLoanSource::AaveV3,
            fee_rate_millionths: 500,
            liquidity: U256::from(1_000u64),
            chain: 56,
            utilisation_num: Some(1_000_000),
            ..StubProvider::default()
        });
        // PancakeSwap-like: 25 bps (2_500 millionths), liquidity 1M.
        // No utilisation penalty modelled; effective fee == 2_500.
        let deep = Arc::new(StubProvider {
            source: FlashLoanSource::PancakeSwapV3,
            fee_rate_millionths: 2_500,
            liquidity: U256::from(1_000_000u64),
            chain: 56,
            ..StubProvider::default()
        });
        let router = FlashLoanRouter::new(vec![shallow, deep]);
        let quote = router
            .route(token(), U256::from(600u64))
            .await
            .expect("route");
        assert_eq!(
            quote.source,
            FlashLoanSource::PancakeSwapV3,
            "deep pool must win when utilisation eats the shallow source"
        );
    }

    /// Transient `available_liquidity` errors must not silently
    /// disqualify the provider — the router should still try to
    /// quote against it. Pre-fix the router coerced the error to
    /// `0` and treated the provider as empty, dropping the borrow.
    #[tokio::test]
    async fn router_falls_through_when_liquidity_probe_errors() {
        // Lone provider — its liquidity probe errors but its quote
        // path succeeds (matching a real "RPC blip on read, but
        // node accepts the eth_call" scenario).
        let aave = Arc::new(StubProvider {
            source: FlashLoanSource::AaveV3,
            fee_rate_millionths: 500,
            liquidity: U256::from(10_000u64),
            chain: 56,
            liquidity_errors: true,
            ..StubProvider::default()
        });
        let router = FlashLoanRouter::new(vec![aave]);
        let quote = router
            .route(token(), U256::from(500u64))
            .await
            .expect("route still attempts quote despite probe error");
        assert_eq!(quote.source, FlashLoanSource::AaveV3);
    }
}
