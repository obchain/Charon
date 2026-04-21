//! Flash-loan router.
//!
//! Walks configured [`FlashLoanProvider`]s in ascending fee-rate order
//! (Balancer 0% → Aave 0.05% → Uniswap pool fee). Returns the first
//! source that can cover the requested borrow. If none can, returns
//! `None` — the caller skips the liquidation rather than sourcing
//! capital from elsewhere.
//!
//! Single-source-on-BSC today means Aave V3 is the only entry in the
//! provider list; the abstraction is here so adding Balancer / Uniswap
//! on a second chain is a config change, not a refactor.

use std::sync::Arc;

use alloy::primitives::{Address, U256};
use charon_core::{FlashLoanProvider, FlashLoanQuote};
use tracing::{debug, info, warn};

/// Fee-priority flash-loan router.
///
/// Built once from a pre-built list of provider handles. Cloning the
/// router is cheap — providers sit behind `Arc<dyn …>`.
pub struct FlashLoanRouter {
    providers: Vec<Arc<dyn FlashLoanProvider>>,
}

impl FlashLoanRouter {
    /// Construct a router, sorting providers by `fee_rate_bps` ascending
    /// so the cheapest source is tried first.
    pub fn new(mut providers: Vec<Arc<dyn FlashLoanProvider>>) -> Self {
        providers.sort_by_key(|p| p.fee_rate_bps());
        Self { providers }
    }

    /// Providers the router will consider, in the order it tries them.
    pub fn providers(&self) -> &[Arc<dyn FlashLoanProvider>] {
        &self.providers
    }

    /// Pick the cheapest provider that can cover `amount` of `token`.
    ///
    /// Per-provider failures (RPC error, insufficient liquidity) are
    /// logged and the walk continues — one dark source shouldn't block
    /// liquidation if a cheaper one can't cover but a pricier one can.
    pub async fn route(&self, token: Address, amount: U256) -> Option<FlashLoanQuote> {
        for provider in &self.providers {
            let source = provider.source();
            let fee_bps = provider.fee_rate_bps();
            match provider.quote(token, amount).await {
                Ok(Some(quote)) => {
                    info!(
                        source = ?source,
                        fee_bps,
                        token = %token,
                        amount = %amount,
                        "flash-loan source selected"
                    );
                    return Some(quote);
                }
                Ok(None) => {
                    debug!(
                        source = ?source,
                        fee_bps,
                        token = %token,
                        amount = %amount,
                        "source skipped: insufficient liquidity"
                    );
                }
                Err(err) => {
                    warn!(
                        source = ?source,
                        fee_bps,
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
    use anyhow::Result;
    use async_trait::async_trait;
    use charon_core::FlashLoanSource;

    /// In-memory provider for router tests — skips all RPC.
    struct StubProvider {
        source: FlashLoanSource,
        fee_bps: u16,
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
        fn fee_rate_bps(&self) -> u16 {
            self.fee_bps
        }
        async fn available_liquidity(&self, _t: Address) -> Result<U256> {
            Ok(self.liquidity)
        }
        async fn quote(&self, token: Address, amount: U256) -> Result<Option<FlashLoanQuote>> {
            if self.liquidity < amount {
                return Ok(None);
            }
            let fee = amount * U256::from(self.fee_bps) / U256::from(10_000u64);
            Ok(Some(FlashLoanQuote {
                source: self.source,
                chain_id: self.chain,
                token,
                amount,
                fee,
                fee_bps: self.fee_bps,
                pool_address: Address::ZERO,
            }))
        }
        fn build_flashloan_calldata(&self, _q: &FlashLoanQuote, _inner: &[u8]) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
    }

    fn token() -> Address {
        address!("1111111111111111111111111111111111111111")
    }

    #[tokio::test]
    async fn picks_cheapest_source_with_sufficient_liquidity() {
        let balancer = Arc::new(StubProvider {
            source: FlashLoanSource::BalancerV2,
            fee_bps: 0,
            liquidity: U256::from(1_000u64),
            chain: 56,
        });
        let aave = Arc::new(StubProvider {
            source: FlashLoanSource::AaveV3,
            fee_bps: 5,
            liquidity: U256::from(1_000_000u64),
            chain: 56,
        });

        // Pass them in reverse order; router should sort internally.
        let router = FlashLoanRouter::new(vec![aave, balancer]);
        let quote = router
            .route(token(), U256::from(500u64))
            .await
            .expect("route");
        assert_eq!(quote.source, FlashLoanSource::BalancerV2);
        assert_eq!(quote.fee_bps, 0);
    }

    #[tokio::test]
    async fn falls_through_to_next_source_when_cheaper_has_no_liquidity() {
        let balancer = Arc::new(StubProvider {
            source: FlashLoanSource::BalancerV2,
            fee_bps: 0,
            liquidity: U256::from(10u64), // too small
            chain: 56,
        });
        let aave = Arc::new(StubProvider {
            source: FlashLoanSource::AaveV3,
            fee_bps: 5,
            liquidity: U256::from(1_000_000u64),
            chain: 56,
        });
        let router = FlashLoanRouter::new(vec![balancer, aave]);
        let quote = router
            .route(token(), U256::from(500u64))
            .await
            .expect("route");
        assert_eq!(quote.source, FlashLoanSource::AaveV3);
    }

    #[tokio::test]
    async fn returns_none_when_no_source_has_liquidity() {
        let balancer = Arc::new(StubProvider {
            source: FlashLoanSource::BalancerV2,
            fee_bps: 0,
            liquidity: U256::ZERO,
            chain: 56,
        });
        let aave = Arc::new(StubProvider {
            source: FlashLoanSource::AaveV3,
            fee_bps: 5,
            liquidity: U256::from(100u64),
            chain: 56,
        });
        let router = FlashLoanRouter::new(vec![balancer, aave]);
        assert!(router.route(token(), U256::from(10_000u64)).await.is_none());
    }

    #[tokio::test]
    async fn returns_none_for_empty_router() {
        let router = FlashLoanRouter::new(Vec::new());
        assert!(router.route(token(), U256::from(1u64)).await.is_none());
    }
}
