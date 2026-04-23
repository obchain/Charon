//! `eth_call` simulation gate.
//!
//! Run a candidate liquidation against the latest block state without
//! sending a transaction. If the call would revert on-chain, the
//! simulation surfaces it as an `Err` carrying the revert reason and
//! the caller drops the opportunity. If the call succeeds, the gate
//! is open — the caller is expected to broadcast next.
//!
//! Zero gas spent on simulation. The hard rule, enforced by the
//! pipeline (not by this module), is **no broadcast without a passing
//! `simulate()`**.

use alloy::primitives::{Address, Bytes};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use anyhow::Result;
use charon_metrics::{endpoint_kind, record_rpc_error, rpc_error, rpc_method, time_rpc};
use tracing::{debug, warn};

/// Stateless simulator — holds the sender + target contract address
/// so per-call construction stays trivial. The provider is passed in
/// per call so consumers can swap chain providers without rebuilding
/// the simulator.
#[derive(Debug, Clone, Copy)]
pub struct Simulator {
    sender: Address,
    liquidator: Address,
}

impl Simulator {
    pub fn new(sender: Address, liquidator: Address) -> Self {
        Self { sender, liquidator }
    }

    /// Run an `eth_call` against the latest block. Returns `Ok` when
    /// the call would succeed, `Err` with the revert reason otherwise.
    /// The caller drops the opportunity on `Err`.
    ///
    /// The gas oracle isn't involved — we let the node use its
    /// default for `eth_call`, which is high enough that a real
    /// `eth_estimateGas` rarely disagrees.
    pub async fn simulate<P, T>(&self, provider: &P, calldata: Bytes) -> Result<()>
    where
        P: Provider<T>,
        T: alloy::transports::Transport + Clone,
    {
        let req = TransactionRequest::default()
            .from(self.sender)
            .to(self.liquidator)
            .input(calldata.into());

        // `time_rpc` owns the histogram sample — the error branch
        // below classifies the failure separately. The simulator
        // talks to the same chain RPC the scanner uses (no private
        // submission relay), so the endpoint kind is `public`.
        // `provider.call(..)` in alloy 0.8 returns an `EthCall`
        // builder (`IntoFuture`, not `Future`); wrap the `.await`
        // in a plain async block so `time_rpc` sees a `Future`.
        let outcome = time_rpc(rpc_method::ETH_CALL, endpoint_kind::PUBLIC, async {
            provider.call(&req).await
        })
        .await;

        match outcome {
            Ok(out) => {
                debug!(
                    sender = %self.sender,
                    target = %self.liquidator,
                    output_len = out.len(),
                    "eth_call simulation succeeded"
                );
                Ok(())
            }
            Err(err) => {
                let msg = format!("{err:#}");
                // A reverted simulation is a deterministic RPC-level
                // rejection (the node executed the call and returned
                // a failure), which is the textbook `rejected`
                // classification — not a transport-layer timeout or
                // connection drop.
                record_rpc_error(rpc_method::ETH_CALL, rpc_error::REJECTED);
                warn!(
                    sender = %self.sender,
                    target = %self.liquidator,
                    error = %msg,
                    "eth_call simulation reverted — opportunity dropped"
                );
                anyhow::bail!("simulation reverted: {msg}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn simulator_holds_addresses() {
        let s = Simulator::new(
            address!("1111111111111111111111111111111111111111"),
            address!("2222222222222222222222222222222222222222"),
        );
        assert_eq!(
            s.sender,
            address!("1111111111111111111111111111111111111111")
        );
        assert_eq!(
            s.liquidator,
            address!("2222222222222222222222222222222222222222")
        );
    }
}
