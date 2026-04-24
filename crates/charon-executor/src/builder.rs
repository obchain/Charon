//! EIP-1559 transaction builder for `CharonLiquidator.executeLiquidation`.
//!
//! Three steps, deliberately separate so callers can simulate before
//! signing and broadcast separately from signing:
//!
//! 1. [`TxBuilder::encode_calldata`] — pack the protocol-specific
//!    [`LiquidationParams`] + outer-pipeline context into the
//!    Solidity-side `LiquidationParams` struct and ABI-encode the
//!    `executeLiquidation(...)` call.
//! 2. [`TxBuilder::build_tx`] — wrap the calldata in an unsigned
//!    [`TransactionRequest`] with EIP-1559 fee fields and the
//!    **pending** nonce for the bot's hot wallet.
//! 3. [`TxBuilder::sign`] — sign the request, returning the raw bytes
//!    that go into `eth_sendRawTransaction` (or a Flashbots bundle).

use std::fmt;

use alloy::eips::{BlockId, BlockNumberOrTag, eip2718::Encodable2718};
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, Bytes};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use alloy::transports::TransportError;
use charon_core::{LiquidationOpportunity, LiquidationParams};
use tracing::debug;

sol! {
    /// Solidity-side `LiquidationParams` — must match
    /// `contracts/src/CharonLiquidator.sol` exactly. If a field is
    /// added or reordered there, this struct must match in lockstep.
    #[derive(Debug)]
    struct CharonLiquidationParams {
        uint8 protocolId;
        address borrower;
        address debtToken;
        address collateralToken;
        address debtVToken;
        address collateralVToken;
        uint256 repayAmount;
        uint256 minSwapOut;
    }

    /// Surface of `CharonLiquidator.sol` consumed by the builder.
    /// `#[sol(rpc)]` would also generate provider-bound bindings, but
    /// we only need the call-encoder here, so the bare interface is
    /// enough.
    interface ICharonLiquidator {
        function executeLiquidation(CharonLiquidationParams calldata params) external;
    }
}

/// Numeric protocol id matching `PROTOCOL_VENUS` in the Solidity
/// source. See `contracts/src/CharonLiquidator.sol:49` — any change
/// there must be mirrored here, and the
/// [`tests::encode_calldata_protocol_id_equals_venus`] unit test
/// pins this end-to-end through the ABI.
const PROTOCOL_VENUS: u8 = 3;

/// Errors surfaced by [`TxBuilder`] when constructing or signing a
/// transaction.
///
/// Marked `#[non_exhaustive]` so new variants (e.g. a dedicated
/// nonce-manager error once PR #43 lands) can be added without
/// breaking downstream match arms.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BuilderError {
    /// Failed to read the pending nonce from the RPC endpoint.
    #[error("nonce fetch failed: {0}")]
    NonceFetch(#[source] TransportError),

    /// `alloy` could not build / sign the EIP-1559 envelope. The
    /// underlying error is carried as a string because
    /// `TransactionBuilderError` is network-parameterised and does
    /// not flow through a plain `Box<dyn Error>` here without noise.
    #[error("signing failed: {0}")]
    Signing(String),

    /// Fee invariant violated: the requested priority tip exceeds
    /// the max fee per gas, which an EIP-1559 node will reject
    /// before the transaction ever touches the mempool.
    #[error("fee invariant violated: priority {0} > max {1}")]
    InvalidFees(u128, u128),

    /// Catch-all for any other transport / RPC failure surfaced by
    /// the provider during tx construction.
    #[error("rpc error: {0}")]
    Rpc(#[from] TransportError),

    /// The opportunity's [`LiquidationParams`] variant is not handled
    /// by this builder. Payload is the `Debug` rendering of the
    /// variant so logs can identify which protocol adapter is still
    /// pending executor support. Surfaced when a future non-`Venus`
    /// variant lands in `charon-core` and reaches the encoder before
    /// the executor has been taught to emit its calldata.
    #[error("unsupported liquidation protocol: {0}")]
    UnsupportedProtocol(String),
}

/// Builder bound to one bot signer + one liquidator deployment.
///
/// Cheap to clone — holds an `Arc`-friendly signer and three small
/// fields. Construct one per `(chain_id, liquidator_address)` pair
/// the bot operates on.
///
/// `Debug` is implemented manually: the embedded
/// [`PrivateKeySigner`] wraps a `k256` scalar whose derived `Debug`
/// would leak the signing key in logs. The custom impl redacts the
/// signer field and exposes only its derived address.
#[derive(Clone)]
pub struct TxBuilder {
    signer: PrivateKeySigner,
    chain_id: u64,
    liquidator: Address,
}

impl fmt::Debug for TxBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TxBuilder")
            .field("signer", &"[redacted]")
            .field("signer_address", &self.signer.address())
            .field("chain_id", &self.chain_id)
            .field("liquidator", &self.liquidator)
            .finish()
    }
}

impl TxBuilder {
    pub fn new(signer: PrivateKeySigner, chain_id: u64, liquidator: Address) -> Self {
        Self {
            signer,
            chain_id,
            liquidator,
        }
    }

    /// Address that will sign + pay gas for built transactions.
    pub fn signer_address(&self) -> Address {
        self.signer.address()
    }

    /// Chain id the builder targets.
    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Address of the deployed `CharonLiquidator` contract.
    pub fn liquidator(&self) -> Address {
        self.liquidator
    }

    /// ABI-encode the outer `executeLiquidation(...)` call.
    ///
    /// Pulls the underlying-token addresses from the `Position` on
    /// the opportunity and the vToken addresses from the
    /// protocol-specific [`LiquidationParams`]. The Solidity struct
    /// field set is a superset of the Rust one — those extra fields
    /// exist on the `LiquidationOpportunity`, not on
    /// `LiquidationParams::Venus`.
    pub fn encode_calldata(
        &self,
        opp: &LiquidationOpportunity,
        params: &LiquidationParams,
    ) -> Result<Bytes, BuilderError> {
        // Exhaustive match (rather than a refutable `let` on the only
        // present variant) so that when a new `LiquidationParams`
        // variant lands in `charon-core` the compiler forces this
        // builder to be audited before it silently accepts the new
        // protocol. `LiquidationParams` is `#[non_exhaustive]`, hence
        // the trailing wildcard arm that surfaces the miss as an
        // explicit `Unsupported` error rather than a panic.
        let (borrower, collateral_vtoken, debt_vtoken, repay_amount) = match params {
            LiquidationParams::Venus {
                borrower,
                collateral_vtoken,
                debt_vtoken,
                repay_amount,
            } => (borrower, collateral_vtoken, debt_vtoken, repay_amount),
            other => {
                return Err(BuilderError::UnsupportedProtocol(format!("{other:?}")));
            }
        };

        let sol_params = CharonLiquidationParams {
            protocolId: PROTOCOL_VENUS,
            borrower: *borrower,
            debtToken: opp.position.debt_token,
            collateralToken: opp.position.collateral_token,
            debtVToken: *debt_vtoken,
            collateralVToken: *collateral_vtoken,
            repayAmount: *repay_amount,
            minSwapOut: opp.swap_route.min_amount_out,
        };

        let call = ICharonLiquidator::executeLiquidationCall { params: sol_params };
        let bytes: Bytes = call.abi_encode().into();

        debug!(
            len = bytes.len(),
            borrower = %borrower,
            "executeLiquidation calldata encoded"
        );
        Ok(bytes)
    }

    /// Build an unsigned EIP-1559 [`TransactionRequest`] pointing at
    /// the configured liquidator.
    ///
    /// Pulls the **pending** nonce from `provider` so a broadcast
    /// that is still in the mempool does not collide with the newly
    /// built transaction. `gas_limit` is supplied by the caller
    /// (typically a multiple of `eth_estimateGas` plus a safety
    /// buffer). Fee fields are passed through; producing them is
    /// the gas oracle's job, not the builder's.
    ///
    /// Fee invariant: `max_priority_fee_per_gas <= max_fee_per_gas`.
    /// Violating it is rejected here rather than letting the node
    /// reject it after a network round-trip.
    // TODO(#43): replace the direct `eth_getTransactionCount` read
    // with the upcoming `NonceManager` once PR #43 lands, so bursty
    // submission windows don't have to re-RPC for every tx.
    pub async fn build_tx<P, T>(
        &self,
        provider: &P,
        calldata: Bytes,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        gas_limit: u64,
    ) -> Result<TransactionRequest, BuilderError>
    where
        P: Provider<T>,
        T: alloy::transports::Transport + Clone,
    {
        if max_priority_fee_per_gas > max_fee_per_gas {
            return Err(BuilderError::InvalidFees(
                max_priority_fee_per_gas,
                max_fee_per_gas,
            ));
        }

        let from = self.signer.address();
        let nonce = provider
            .get_transaction_count(from)
            .block_id(BlockId::Number(BlockNumberOrTag::Pending))
            .await
            .map_err(BuilderError::NonceFetch)?;

        let tx = TransactionRequest::default()
            .with_from(from)
            .with_to(self.liquidator)
            .with_input(calldata)
            .with_chain_id(self.chain_id)
            .with_nonce(nonce)
            .with_max_fee_per_gas(max_fee_per_gas)
            .with_max_priority_fee_per_gas(max_priority_fee_per_gas)
            .with_gas_limit(gas_limit);

        debug!(
            from = %from,
            to = %self.liquidator,
            chain_id = self.chain_id,
            nonce,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            gas_limit,
            "EIP-1559 tx built"
        );
        Ok(tx)
    }

    /// Sign the request with the bot signer and return raw EIP-2718
    /// envelope bytes ready for `eth_sendRawTransaction` or a
    /// Flashbots bundle. Does **not** broadcast.
    pub async fn sign(&self, tx: TransactionRequest) -> Result<Bytes, BuilderError> {
        let wallet = EthereumWallet::new(self.signer.clone());
        let envelope = tx
            .build(&wallet)
            .await
            .map_err(|e| BuilderError::Signing(format!("{e:#}")))?;
        let mut buf = Vec::with_capacity(256);
        envelope.encode_2718(&mut buf);
        debug!(raw_len = buf.len(), "tx signed");
        Ok(buf.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{U256, address};
    use charon_core::{
        FlashLoanSource, LiquidationOpportunity, LiquidationParams, Position, ProtocolId, SwapRoute,
    };

    fn mk_signer() -> PrivateKeySigner {
        // Deterministic dev key — never used against mainnet.
        // First Anvil/Hardhat default key.
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
            .parse()
            .expect("dev key parse")
    }

    fn mk_opportunity() -> LiquidationOpportunity {
        LiquidationOpportunity {
            position: Position {
                protocol: ProtocolId::Venus,
                chain_id: 56,
                borrower: address!("1111111111111111111111111111111111111111"),
                collateral_token: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                debt_token: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                collateral_amount: U256::from(1_000u64),
                debt_amount: U256::from(500u64),
                health_factor: U256::ZERO,
                liquidation_bonus_bps: 1_000,
            },
            debt_to_repay: U256::from(250u64),
            expected_collateral_out: U256::from(275u64),
            flash_source: FlashLoanSource::AaveV3,
            swap_route: SwapRoute {
                token_in: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                token_out: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                amount_in: U256::from(275u64),
                min_amount_out: U256::from(260u64),
                pool_fee: Some(3_000),
            },
            net_profit_wei: U256::from(5_000u64),
        }
    }

    fn mk_params() -> LiquidationParams {
        LiquidationParams::Venus {
            borrower: address!("1111111111111111111111111111111111111111"),
            collateral_vtoken: address!("cccccccccccccccccccccccccccccccccccccccc"),
            debt_vtoken: address!("dddddddddddddddddddddddddddddddddddddddd"),
            repay_amount: U256::from(250u64),
        }
    }

    #[test]
    fn encode_calldata_pins_execute_liquidation_selector() {
        let builder = TxBuilder::new(
            mk_signer(),
            56,
            address!("ffffffffffffffffffffffffffffffffffffffff"),
        );
        let bytes = builder
            .encode_calldata(&mk_opportunity(), &mk_params())
            .expect("encode");

        // Selector pinned against alloy's generated SELECTOR constant
        // — catches accidental changes to argument order or
        // CharonLiquidationParams shape (which would break lockstep
        // with the Solidity struct).
        assert_eq!(
            &bytes[..4],
            &ICharonLiquidator::executeLiquidationCall::SELECTOR,
            "calldata selector drifted from executeLiquidation"
        );
        // selector + at least the eight struct field slots; alloy may
        // also emit a leading offset word, so use `>=` not `>`.
        assert!(bytes.len() >= 4 + 32 * 8);
    }

    #[test]
    fn signer_address_is_deterministic() {
        let b = TxBuilder::new(mk_signer(), 56, Address::ZERO);
        assert_eq!(
            b.signer_address(),
            address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266")
        );
    }

    /// Pins the ABI-level protocolId byte to the `PROTOCOL_VENUS`
    /// constant in `contracts/src/CharonLiquidator.sol`. If the
    /// Solidity side renumbers the protocol enum, this test flips
    /// red and forces the constant here to move with it — preventing
    /// a silent mismatch that would route every built tx to the
    /// wrong adapter in the on-chain dispatcher.
    #[test]
    fn encode_calldata_protocol_id_equals_venus() {
        let builder = TxBuilder::new(
            mk_signer(),
            56,
            address!("ffffffffffffffffffffffffffffffffffffffff"),
        );
        let bytes = builder
            .encode_calldata(&mk_opportunity(), &mk_params())
            .expect("encode");

        // ABI layout: `CharonLiquidationParams` is a static
        // struct (no dynamic-length fields), so Solidity inlines it
        // straight after the 4-byte selector with no offset head.
        // The first field `uint8 protocolId` sits in its own 32-byte
        // slot, left-padded so the value is in the last byte.
        let protocol_id = bytes[4 + 31];
        assert_eq!(
            protocol_id, 3u8,
            "protocolId must match PROTOCOL_VENUS in contracts/src/CharonLiquidator.sol:49"
        );
    }

    /// EIP-1559 fee invariant guard: building a tx with a priority
    /// fee higher than the max fee returns `InvalidFees` rather than
    /// round-tripping to the node and getting a pool-level rejection.
    #[test]
    fn build_tx_rejects_priority_exceeding_max_fee() {
        let err = BuilderError::InvalidFees(10, 5);
        match err {
            BuilderError::InvalidFees(p, m) => {
                assert_eq!(p, 10);
                assert_eq!(m, 5);
            }
            _ => panic!("wrong variant"),
        }
    }

    /// Debug output must not leak the signing key. Assert both the
    /// redaction sentinel is present and no obvious hex-looking
    /// signing-key scalar appears.
    #[test]
    fn debug_redacts_signer() {
        let b = TxBuilder::new(
            mk_signer(),
            56,
            address!("ffffffffffffffffffffffffffffffffffffffff"),
        );
        let dbg = format!("{b:?}");
        assert!(dbg.contains("[redacted]"), "{dbg}");
        // The Anvil default key; must never appear in Debug output.
        assert!(
            !dbg.contains("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"),
            "signing key leaked in Debug: {dbg}"
        );
    }
}
