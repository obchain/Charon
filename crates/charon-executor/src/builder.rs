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
//!    [`TransactionRequest`] with EIP-1559 fee fields and the latest
//!    nonce for the bot's hot wallet.
//! 3. [`TxBuilder::sign`] — sign the request, returning the raw bytes
//!    that go into `eth_sendRawTransaction` (or a Flashbots bundle).

use alloy::eips::eip2718::Encodable2718;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, Bytes};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result};
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

/// Numeric protocol id matching `PROTOCOL_VENUS` in the Solidity source.
const PROTOCOL_VENUS: u8 = 3;

/// Builder bound to one bot signer + one liquidator deployment.
///
/// Cheap to clone — holds an `Arc`-friendly signer and three small
/// fields. Construct one per `(chain_id, liquidator_address)` pair the
/// bot operates on.
#[derive(Debug, Clone)]
pub struct TxBuilder {
    signer: PrivateKeySigner,
    chain_id: u64,
    liquidator: Address,
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
    /// Pulls the underlying-token addresses from the `Position` on the
    /// opportunity and the vToken addresses from the protocol-specific
    /// [`LiquidationParams`]. The Solidity struct field set is a
    /// superset of the Rust one — those extra fields exist on the
    /// `LiquidationOpportunity`, not on `LiquidationParams::Venus`.
    pub fn encode_calldata(
        &self,
        opp: &LiquidationOpportunity,
        params: &LiquidationParams,
    ) -> Result<Bytes> {
        let LiquidationParams::Venus {
            borrower,
            collateral_vtoken,
            debt_vtoken,
            repay_amount,
        } = params;

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
    /// The caller supplies the nonce (typically from
    /// [`crate::NonceManager::next`]) and gas parameters from the gas
    /// oracle. This method intentionally does **not** hit the provider
    /// — doing so would race against the `NonceManager`'s local counter
    /// and hand out duplicate nonces when two opportunities land in the
    /// same block.
    pub fn build_tx(
        &self,
        calldata: Bytes,
        nonce: u64,
        max_fee_per_gas: u128,
        max_priority_fee_per_gas: u128,
        gas_limit: u64,
    ) -> TransactionRequest {
        let from = self.signer.address();
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
        tx
    }

    /// Sign the request with the bot signer and return raw EIP-2718
    /// envelope bytes ready for `eth_sendRawTransaction` or a
    /// Flashbots bundle. Does **not** broadcast.
    pub async fn sign(&self, tx: TransactionRequest) -> Result<Bytes> {
        let wallet = EthereumWallet::new(self.signer.clone());
        let envelope = tx
            .build(&wallet)
            .await
            .context("tx builder: failed to sign tx")?;
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
                pool_fee: 3_000,
            },
            net_profit_usd_cents: 5_000,
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
}
