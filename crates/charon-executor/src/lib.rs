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

pub mod batcher;
pub mod builder;
pub mod gas;
pub mod nonce;
pub mod simulation;
pub mod submit;

pub use batcher::{
    BSC_CHAIN_ID, Batcher, BatcherError, LiquidationBatch, MAX_BATCH_SIZE, SOLIDITY_MAX_BATCH_SIZE,
};
pub use builder::{ICharonLiquidator, TxBuilder};
pub use gas::{GasOracle, GasParams, gas_cost_usd_cents};
pub use nonce::NonceManager;
pub use simulation::Simulator;
pub use submit::{DEFAULT_SUBMIT_TIMEOUT, Submitter};
