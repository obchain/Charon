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

pub mod builder;
pub mod simulation;

pub use builder::{BuilderError, ICharonLiquidator, TxBuilder};
pub use simulation::{SimulationError, Simulator};
