//! Transaction construction, simulation, and (later) broadcast for Charon.
//!
//! Sits between the scanner / profit-calc / router pipeline and the
//! on-chain `CharonLiquidator.sol`. Callers hand in a
//! [`LiquidationOpportunity`](charon_core::LiquidationOpportunity) plus
//! the protocol-specific [`LiquidationParams`](charon_core::LiquidationParams)
//! the adapter produced; this crate encodes the outer
//! `executeLiquidation(...)` call, builds an EIP-1559 transaction,
//! signs it with the bot's hot wallet, and runs an `eth_call`
//! simulation gate before any broadcast can happen.
//!
//! For multi-opportunity batching, see [`batcher`] — it plans and
//! encodes `batchExecute(LiquidationParams[])` calldata for the
//! on-chain `CharonLiquidator.batchExecute` entrypoint.

pub mod batcher;
pub mod builder;
pub mod gas;
pub mod nonce;
pub mod simulation;
pub mod submit;

pub use batcher::{
    BSC_CHAIN_ID, Batcher, BatcherError, LiquidationBatch, MAX_BATCH_SIZE, SOLIDITY_MAX_BATCH_SIZE,
    SimulatedBatchCalldata, UnsimulatedBatchCalldata,
};
pub use builder::{BuilderError, ICharonLiquidator, TxBuilder};
pub use gas::{
    CHAINLINK_DECIMALS, GasDecision, GasError, GasOracle, GasParams, gas_cost_usd_cents,
};
pub use nonce::{NonceError, NonceManager};
pub use simulation::{SimulationError, Simulator};
pub use submit::{DEFAULT_SUBMIT_TIMEOUT, SubmitError, Submitter};
