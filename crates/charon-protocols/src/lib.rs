//! Charon protocol adapters.
//!
//! One module per lending protocol. Each module defines a struct that
//! implements the [`LendingProtocol`](charon_core::LendingProtocol) trait
//! — the scanner and executor talk to these structs instead of protocol-
//! specific RPCs directly, so adding a new protocol is a self-contained
//! change here with no scanner edits required.
//!
//! For v0.1 only the Venus adapter is wired up; Aave / Compound / Morpho
//! adapters land in later milestones.

pub mod multicall;
pub mod venus;

pub use multicall::{InnerCall, InnerResult, MAX_CALLS_PER_BATCH, MULTICALL3_ADDRESS, chunk_calls};
pub use venus::VenusAdapter;
