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
    pub async fn simulate<P>(&self, provider: &P, calldata: Bytes) -> Result<()>
    where
        P: Provider,
    {
        let req = TransactionRequest::default()
            .from(self.sender)
            .to(self.liquidator)
            .input(calldata.into());

        match provider.call(&req).await {
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
