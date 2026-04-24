//! `eth_call` simulation gate.
//!
//! Run a candidate liquidation against the latest block state without
//! sending a transaction. If the call would revert on-chain, the
//! simulation surfaces it as [`SimulationError::Reverted`] carrying
//! the 4-byte selector + full revert payload, and the caller drops
//! the opportunity. If the call succeeds, the gate is open — the
//! caller is expected to broadcast next.
//!
//! Zero gas spent on simulation. The hard rule, enforced by the
//! pipeline (not by this module), is **no broadcast without a
//! passing `simulate()`**.

use alloy::hex;
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::transports::{RpcError, TransportError};
use tracing::{debug, warn};

use crate::builder::TxBuilder;

/// Errors surfaced by [`Simulator::simulate`].
///
/// `#[non_exhaustive]` so we can grow the set (e.g. structured
/// selector decoding into known Venus / Aave / PancakeSwap errors)
/// without breaking downstream matches.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SimulationError {
    /// The on-chain call reverted. `selector_hex` is the 4-byte
    /// Solidity error selector formatted as `0xaabbccdd`, or
    /// `"0x"` when the revert payload is shorter than four bytes.
    /// `data_hex` carries the full raw payload for cross-reference.
    #[error("simulation reverted: selector={selector_hex} data={data_hex}")]
    Reverted {
        selector_hex: String,
        data_hex: String,
    },

    /// Transport / RPC failure before the node ever got to execute
    /// the call — network blip, auth failure, etc. Not a revert.
    #[error("provider error: {0}")]
    Provider(#[source] TransportError),
}

/// Stateless simulator — holds the sender + target contract address
/// so per-call construction stays trivial. The provider is passed in
/// per call so consumers can swap chain providers without rebuilding
/// the simulator.
///
/// The `sender` **must** match the `owner()` of the on-chain
/// `CharonLiquidator` (the bot's hot wallet). `executeLiquidation`
/// is `onlyOwner`, so a simulation with any other sender will revert
/// unconditionally and the opportunity will be dropped even when the
/// underlying liquidation would have been profitable. Prefer
/// [`Simulator::from_builder`] to keep this coupling tight.
#[derive(Debug, Clone, Copy)]
pub struct Simulator {
    sender: Address,
    liquidator: Address,
}

impl Simulator {
    /// Low-level constructor. Callers are responsible for ensuring
    /// `sender` matches the on-chain owner. Prefer
    /// [`Simulator::from_builder`] in production paths.
    pub fn new(sender: Address, liquidator: Address) -> Self {
        debug_assert!(
            sender != Address::ZERO,
            "Simulator sender must be the bot hot wallet, not the zero address"
        );
        Self { sender, liquidator }
    }

    /// Build a simulator whose `sender` is bound to the hot wallet
    /// inside the provided [`TxBuilder`]. This is the only way to
    /// guarantee the simulation sender matches the address that will
    /// eventually sign and broadcast.
    pub fn from_builder(builder: &TxBuilder, liquidator: Address) -> Self {
        Self::new(builder.signer_address(), liquidator)
    }

    /// Address the simulator will impersonate in `eth_call`. Must
    /// equal `CharonLiquidator.owner()` — see struct-level docs.
    pub fn sender(&self) -> Address {
        self.sender
    }

    /// Address of the target liquidator contract.
    pub fn liquidator(&self) -> Address {
        self.liquidator
    }

    /// Run an `eth_call` against the latest block. Returns `Ok` when
    /// the call would succeed, [`SimulationError::Reverted`] with
    /// the decoded selector + raw payload otherwise. The caller
    /// drops the opportunity on `Err`.
    ///
    /// `gas_limit` must match (or exceed) what the real broadcast
    /// will use — a simulation that fits in less gas than the
    /// broadcast can pass here and still revert on-chain as
    /// out-of-gas.
    pub async fn simulate<P, T>(
        &self,
        provider: &P,
        calldata: Bytes,
        gas_limit: u64,
    ) -> Result<(), SimulationError>
    where
        P: Provider<T>,
        T: alloy::transports::Transport + Clone,
    {
        let req = TransactionRequest::default()
            .with_from(self.sender)
            .with_to(self.liquidator)
            .with_input(calldata)
            .with_gas_limit(gas_limit);

        // `time_rpc` owns the latency histogram sample; the error
        // branch below classifies rejections/timeouts separately so
        // Grafana can pivot on `error_kind`. The simulator talks to
        // the same chain RPC the scanner uses (no private submission
        // relay) so the endpoint is `public`. `provider.call(..)` in
        // alloy 0.8 returns an `EthCall` builder that is
        // `IntoFuture`, not a `Future`; wrap the await in an async
        // block so `time_rpc` sees a real `Future`.
        let outcome = charon_metrics::time_rpc(
            charon_metrics::rpc_method::ETH_CALL,
            charon_metrics::endpoint_kind::PUBLIC,
            async { provider.call(&req).await },
        )
        .await;

        match outcome {
            Ok(out) => {
                debug!(
                    sender = %self.sender,
                    target = %self.liquidator,
                    gas_limit,
                    output_len = out.len(),
                    "eth_call simulation succeeded"
                );
                Ok(())
            }
            Err(err) => {
                let revert = extract_revert_data(&err);
                let (selector_hex, data_hex) = match revert.as_ref() {
                    Some(bytes) => format_revert(bytes),
                    None => ("0x".to_string(), "0x".to_string()),
                };
                warn!(
                    sender = %self.sender,
                    target = %self.liquidator,
                    gas_limit,
                    selector = %selector_hex,
                    data = %data_hex,
                    error = %format!("{err:#}"),
                    "eth_call simulation reverted — opportunity dropped"
                );

                // Distinguish a true revert (node replied with error
                // data) from a transport-level failure. `alloy`
                // surfaces both on the same error arm, so the
                // presence of returndata is the discriminator. The
                // RPC error counter is labelled `rejected` on a
                // deterministic node-side rejection vs
                // `connection_lost` on a transport blip — dashboards
                // pivot on that label to separate "upstream unstable"
                // from "our calldata keeps reverting".
                if revert.is_some() {
                    charon_metrics::record_rpc_error(
                        charon_metrics::rpc_method::ETH_CALL,
                        charon_metrics::rpc_error::REJECTED,
                    );
                    Err(SimulationError::Reverted {
                        selector_hex,
                        data_hex,
                    })
                } else {
                    charon_metrics::record_rpc_error(
                        charon_metrics::rpc_method::ETH_CALL,
                        charon_metrics::rpc_error::CONNECTION_LOST,
                    );
                    Err(SimulationError::Provider(err))
                }
            }
        }
    }
}

/// Try to extract the raw revert payload from a transport error.
///
/// BSC / geth-family nodes return it as a hex-encoded JSON string in
/// the `data` field of the JSON-RPC error object. We trim the
/// surrounding JSON quotes + optional `0x` prefix manually rather
/// than pulling in a JSON parser just for this path.
///
/// Known selectors worth cross-referencing when they show up in
/// logs (v0.1 scope, not hard-coded to avoid stale tables):
/// - Venus `VToken`: custom errors added in VIP-194+.
/// - Aave v3 `Pool`: the generic `Error(string)` selector
///   `0x08c379a0` is the most common; newer custom errors carry
///   their own selectors.
/// - PancakeSwap v3 router: slippage errors like `TooLittleReceived`.
fn extract_revert_data(err: &TransportError) -> Option<Bytes> {
    let RpcError::ErrorResp(resp) = err else {
        return None;
    };
    let raw = resp.data.as_ref()?.get();
    // `RawValue::get()` returns the literal JSON, so a string value
    // reads as `"\"0xdeadbeef\""`. Strip the surrounding quotes.
    let unquoted = raw.strip_prefix('"')?.strip_suffix('"')?;
    let hex_body = unquoted.strip_prefix("0x").unwrap_or(unquoted);
    hex::decode(hex_body).ok().map(Bytes::from)
}

/// Format `(selector_hex, data_hex)` from a raw revert payload.
fn format_revert(bytes: &Bytes) -> (String, String) {
    let data_hex = format!("0x{}", hex::encode(bytes));
    if bytes.len() >= 4 {
        let selector_hex = format!(
            "0x{:02x}{:02x}{:02x}{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3]
        );
        (selector_hex, data_hex)
    } else {
        ("0x".to_string(), data_hex)
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
            s.sender(),
            address!("1111111111111111111111111111111111111111")
        );
        assert_eq!(
            s.liquidator(),
            address!("2222222222222222222222222222222222222222")
        );
    }

    #[test]
    fn format_revert_extracts_selector() {
        let bytes = Bytes::from(vec![0x08, 0xc3, 0x79, 0xa0, 0xde, 0xad]);
        let (selector, data) = format_revert(&bytes);
        assert_eq!(selector, "0x08c379a0");
        assert_eq!(data, "0x08c379a0dead");
    }

    #[test]
    fn format_revert_short_payload_has_empty_selector() {
        let bytes = Bytes::from(vec![0x01, 0x02]);
        let (selector, data) = format_revert(&bytes);
        assert_eq!(selector, "0x");
        assert_eq!(data, "0x0102");
    }

    #[test]
    #[should_panic(expected = "Simulator sender must be the bot hot wallet")]
    fn zero_sender_trips_debug_assert() {
        // debug_assert is only active in debug builds (cargo test
        // runs in debug by default), so this panic is reachable.
        let _ = Simulator::new(
            Address::ZERO,
            address!("2222222222222222222222222222222222222222"),
        );
    }
}
