//! Live execution path (ADR-002). Compiled only with `--features live`.
//!
//! The paper (default) build never contains this module: dry-run can never
//! reach [`sender::OnchainExecutor`]. Live startup is triple-locked
//! ([`lock`]: build + config + arm + manifest) and strictly ordered:
//! L1 lock -> role read-back ([`roles`]) -> float-cap check
//! ([`float_cap`]) -> [`signer`] construction last.
//!
//! Every stage fails closed; every refusal and rejection has an offline test
//! (L10). No network calls happen inside these modules' pure cores -- the
//! `live` binary performs read-only RPC at the boundary (P4 wires writes).

pub mod breaker;
pub mod float_cap;
pub mod ledger;
pub mod lock;
pub mod manifest;
pub mod pipeline;
pub mod profile;
pub mod roles;
pub mod sender;
pub mod signer;
