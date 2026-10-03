//! Shared library for the paper binary (`src/main.rs`) and the Discord ops
//! CLI (`src/bin/discord-bot.rs`).
//!
//! Both binaries link these modules from here so there is exactly one copy
//! of the control-plane and trading logic (no `#[path]` duplication).
//! All modules are dry-run safe: no live execution, no key handling.

pub mod alerts;
pub mod chain;
pub mod config;
pub mod discord;
pub mod fork_check;
pub mod metrics;
pub mod pools;
pub mod profit;
pub mod risk;
pub mod sequencer;
pub mod sim;
