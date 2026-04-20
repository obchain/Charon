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

pub mod venus;

pub use venus::VenusAdapter;
