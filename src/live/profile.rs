//! L2 chain profiles: `config/sepolia.toml` (84532) and
//! `config/mainnet-canary.toml` (8453).
//!
//! Each profile pins `chain_id`, executor/Vault/router/token/quoter
//! addresses, the expected on-chain role matrix, and the compiled `hard_max`
//! set it may not exceed. Mainnet addresses never appear in the Sepolia
//! profile and vice versa (S8; asserted by an offline disjointness test).
//!
//! Monetary units are USDC base units (6 decimals) as integers. No floats.

use alloy::primitives::Address;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use thiserror::Error;

use crate::config::BotConfig;

/// Base Sepolia chain id.
pub const SEPOLIA_CHAIN_ID: u64 = 84532;
/// Base mainnet chain id.
pub const MAINNET_CHAIN_ID: u64 = 8453;

/// Operator float ceiling, USD-equivalent: gas only, <= $20 (ADR-002 §11).
pub const MAX_OPERATOR_FLOAT_USDC: u64 = 20_000_000;

/// The `[live]` section of a chain profile. Everything else reuses
/// [`BotConfig`] (which already enforces `size <= max <= hard_max`,
/// `max_in_flight == 1`, and the deadline range).
#[derive(Debug, Clone, Deserialize)]
pub struct LiveSection {
    /// Config lock (L1): the chosen profile must set this to `true`.
    /// `config/default.toml` has no live section at all.
    pub enabled: bool,
    /// Minimum readiness stage that may be attempted under this profile
    /// (`"G3"` on Sepolia, `"G4"` on mainnet canary). The manifest's
    /// `attempt_stage` must equal it while `stage_ready` stays strictly
    /// below it (attempt-authorization, never a completion claim).
    pub stage_required: String,
    /// Executor contract this profile may trade through.
    pub executor: Address,
    /// Expected on-chain roles (L4 read-back target).
    pub owner: Address,
    pub operator: Address,
    pub pauser: Address,
    /// Expected on-chain `maxFlashUSDC` (L4 read-back target, base units).
    pub onchain_max_flash_usdc: u64,
    /// Flash lender (Balancer Vault on mainnet; placeholder pre-deploy).
    pub vault: Address,
    /// Routers the profile may touch (never the UniversalRouter, S4).
    pub routers: Vec<Address>,
    /// Tokens the profile may touch (USDC-only execution scope).
    pub tokens: Vec<Address>,
    /// Quoters the profile verifies against.
    pub quoters: Vec<Address>,
    /// Sequencer feed for this chain. Zero address = unconfigured: the gate
    /// fails closed until a verified feed is published (Sepolia today).
    pub sequencer_feed: Address,
    /// Off-chain ceilings; each must sit at or below its compiled/on-chain
    /// counterpart (never loosened without a code change + re-pin).
    pub hard_max_flash_usdc: u64,
    pub hard_daily_loss_cap_usdc: u64,
    pub hard_max_consecutive_failures: u32,
    /// Operator float ceiling, USD-equivalent (gas only).
    pub operator_float_cap_usdc: u64,
    /// Receipts book only after this many confirmations (L6).
    pub confirmation_depth: u64,
    /// Kill-switch flag file: must exist and contain exactly `OK` to allow
    /// submission; missing/unreadable/other content means stopped (L7).
    pub kill_switch_file: PathBuf,
    /// Directory holding the hash-chained JSONL ledger (L8).
    pub ledger_dir: PathBuf,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProfileError {
    #[error("unknown stage_required '{0}' (want G0..=G6)")]
    BadStage(String),
    #[error("stage_required {0} below the minimum {1} for chain {2}")]
    StageBelowChainMinimum(String, String, u64),
    #[error("live.chain_id {0} != base chain_id {1}")]
    ChainMismatch(u64, u64),
    #[error("zero address in field '{0}'")]
    ZeroAddress(&'static str),
    #[error("empty list in field '{0}'")]
    EmptyList(&'static str),
    #[error("duplicate address in field '{0}'")]
    DuplicateAddress(&'static str),
    #[error("roles must be three distinct addresses")]
    RolesNotDistinct,
    #[error("hard_max_flash_usdc {0} exceeds compiled HARD_MAX {1}")]
    HardFlashAboveCompiled(u64, u64),
    #[error("max_flash_usdc {0} exceeds profile hard_max_flash_usdc {1}")]
    BaseFlashAboveHard(u64, u64),
    #[error("profile hard_max_flash_usdc {0} exceeds on-chain maxFlashUSDC {1}")]
    HardAboveOnchain(u64, u64),
    #[error("daily_loss_cap_usdc {0} exceeds profile hard cap {1}")]
    BaseLossAboveHard(u64, u64),
    #[error("max_consecutive_failures {0} exceeds profile hard cap {1}")]
    BaseFailuresAboveHard(u32, u32),
    #[error("operator_float_cap_usdc {0} exceeds the gas-only ceiling {1}")]
    FloatCapTooHigh(u64, u64),
    #[error("confirmation_depth {0} outside 1..=256")]
    BadConfirmationDepth(u64),
    #[error("empty path in field '{0}'")]
    EmptyPath(&'static str),
    #[error("mainnet profile must pin {0}")]
    MainnetPin(&'static str),
    #[error("mainnet profile must not list the UniversalRouter as a router")]
    UniversalRouterListed,
    #[error("sepolia sequencer_feed must stay unconfigured (zero) until a feed is verified")]
    SepoliaFeedConfigured,
}

/// A validated chain profile: base config plus the `[live]` section.
#[derive(Debug, Clone)]
pub struct LiveProfile {
    pub base: BotConfig,
    pub live_section: LiveSection,
    /// Absolute path of the file this profile was loaded from (the manifest
    /// lock recomputes its hash; it does not trust the manifest blindly).
    pub path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ProfileFile {
    #[serde(flatten)]
    base: BotConfig,
    live: LiveSection,
}

/// Numeric order of a `G0`..=`G6` stage label. `None` = unknown label.
pub fn stage_order(stage: &str) -> Option<u8> {
    match stage {
        "G0" => Some(0),
        "G1" => Some(1),
        "G2" => Some(2),
        "G3" => Some(3),
        "G4" => Some(4),
        "G5" => Some(5),
        "G6" => Some(6),
        _ => None,
    }
}

/// Canonical label for a numeric rung. `None` = out of range.
pub fn stage_label(order: u8) -> Option<&'static str> {
    match order {
        0 => Some("G0"),
        1 => Some("G1"),
        2 => Some("G2"),
        3 => Some("G3"),
        4 => Some("G4"),
        5 => Some("G5"),
        6 => Some("G6"),
        _ => None,
    }
}

impl LiveProfile {
    /// Load and fail-closed validate a profile file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let file: ProfileFile = toml::from_str(&raw)?;
        // `BotConfig` validation already ran inside `toml::from_str`? No:
        // deserialization does not call `validate`. Run it explicitly.
        file.base.validate()?;
        let profile = Self {
            base: file.base,
            live_section: file.live,
            path: path.to_path_buf(),
        };
        profile.validate()?;
        Ok(profile)
    }

    /// Fail-closed validation of the live section and its cross-checks
    /// against the base config and the compiled constants.
    pub fn validate(&self) -> Result<(), ProfileError> {
        use ProfileError as E;
        let l = &self.live_section;
        if self.base.chain_id != SEPOLIA_CHAIN_ID && self.base.chain_id != MAINNET_CHAIN_ID {
            // `BotConfig::validate` already rejects other ids; keep the live
            // error ladder total (defence in depth, same verdict).
            return Err(E::ChainMismatch(self.base.chain_id, self.base.chain_id));
        }
        let order =
            stage_order(&l.stage_required).ok_or_else(|| E::BadStage(l.stage_required.clone()))?;
        let minimum: u8 = if self.base.chain_id == SEPOLIA_CHAIN_ID {
            3
        } else {
            4
        };
        if order < minimum {
            let want = if minimum == 3 { "G3" } else { "G4" };
            return Err(E::StageBelowChainMinimum(
                l.stage_required.clone(),
                want.to_string(),
                self.base.chain_id,
            ));
        }
        for (field, addr) in [
            ("executor", l.executor),
            ("owner", l.owner),
            ("operator", l.operator),
            ("pauser", l.pauser),
            ("vault", l.vault),
        ] {
            if addr.is_zero() {
                return Err(E::ZeroAddress(field));
            }
        }
        for (field, list) in [
            ("routers", &l.routers),
            ("tokens", &l.tokens),
            ("quoters", &l.quoters),
        ] {
            if list.is_empty() {
                return Err(E::EmptyList(field));
            }
            if list.iter().any(|a| a.is_zero()) {
                return Err(E::ZeroAddress(field));
            }
            let uniq: BTreeSet<_> = list.iter().collect();
            if uniq.len() != list.len() {
                return Err(E::DuplicateAddress(field));
            }
        }
        if l.owner == l.operator || l.owner == l.pauser || l.operator == l.pauser {
            return Err(E::RolesNotDistinct);
        }
        if l.hard_max_flash_usdc == 0 || l.hard_max_flash_usdc > crate::risk::HARD_MAX_FLASH_USDC {
            return Err(E::HardFlashAboveCompiled(
                l.hard_max_flash_usdc,
                crate::risk::HARD_MAX_FLASH_USDC,
            ));
        }
        if self.base.max_flash_usdc > l.hard_max_flash_usdc {
            return Err(E::BaseFlashAboveHard(
                self.base.max_flash_usdc,
                l.hard_max_flash_usdc,
            ));
        }
        if l.hard_max_flash_usdc > l.onchain_max_flash_usdc {
            return Err(E::HardAboveOnchain(
                l.hard_max_flash_usdc,
                l.onchain_max_flash_usdc,
            ));
        }
        if self.base.daily_loss_cap_usdc > l.hard_daily_loss_cap_usdc
            || l.hard_daily_loss_cap_usdc == 0
        {
            return Err(E::BaseLossAboveHard(
                self.base.daily_loss_cap_usdc,
                l.hard_daily_loss_cap_usdc,
            ));
        }
        if self.base.max_consecutive_failures > l.hard_max_consecutive_failures
            || l.hard_max_consecutive_failures == 0
        {
            return Err(E::BaseFailuresAboveHard(
                self.base.max_consecutive_failures,
                l.hard_max_consecutive_failures,
            ));
        }
        if l.operator_float_cap_usdc == 0 || l.operator_float_cap_usdc > MAX_OPERATOR_FLOAT_USDC {
            return Err(E::FloatCapTooHigh(
                l.operator_float_cap_usdc,
                MAX_OPERATOR_FLOAT_USDC,
            ));
        }
        if l.confirmation_depth < 1 || l.confirmation_depth > 256 {
            return Err(E::BadConfirmationDepth(l.confirmation_depth));
        }
        if l.kill_switch_file.as_os_str().is_empty() {
            return Err(E::EmptyPath("kill_switch_file"));
        }
        if l.ledger_dir.as_os_str().is_empty() {
            return Err(E::EmptyPath("ledger_dir"));
        }
        if self.base.chain_id == MAINNET_CHAIN_ID {
            use crate::config::addresses as a;
            if l.vault != a::BALANCER_VAULT {
                return Err(E::MainnetPin("balancer vault"));
            }
            if !l.tokens.contains(&a::USDC_NATIVE) {
                return Err(E::MainnetPin("native USDC in tokens"));
            }
            if l.routers.contains(&a::UNIV3_UNIVERSAL_ROUTER) {
                return Err(E::UniversalRouterListed);
            }
            if l.sequencer_feed != a::SEQUENCER_UPTIME_FEED_BASE {
                return Err(E::MainnetPin("chainlink sequencer feed"));
            }
        } else {
            // Sepolia: no Chainlink sequencer feed is published -- the only
            // safe profile value is unconfigured (the gate fails closed).
            if !l.sequencer_feed.is_zero() {
                return Err(E::SepoliaFeedConfigured);
            }
        }
        Ok(())
    }

    /// Every non-zero address this profile pins (for the S8 disjointness
    /// test: Sepolia and mainnet profiles must share none).
    pub fn pinned_addresses(&self) -> BTreeSet<Address> {
        let l = &self.live_section;
        let mut out = BTreeSet::new();
        for a in [
            l.executor,
            l.owner,
            l.operator,
            l.pauser,
            l.vault,
            l.sequencer_feed,
        ] {
            if !a.is_zero() {
                out.insert(a);
            }
        }
        for a in l
            .routers
            .iter()
            .chain(l.tokens.iter())
            .chain(l.quoters.iter())
        {
            if !a.is_zero() {
                out.insert(*a);
            }
        }
        for p in &self.base.pairs {
            for a in [p.token_a, p.token_b] {
                if !a.is_zero() {
                    out.insert(a);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sepolia() -> LiveProfile {
        LiveProfile::load(Path::new("config/sepolia.toml")).expect("sepolia profile loads")
    }

    fn canary() -> LiveProfile {
        LiveProfile::load(Path::new("config/mainnet-canary.toml")).expect("canary profile loads")
    }

    #[test]
    fn both_profiles_load_and_validate() {
        let s = sepolia();
        assert_eq!(s.base.chain_id, SEPOLIA_CHAIN_ID);
        assert!(s.live_section.enabled);
        assert_eq!(s.live_section.stage_required, "G3");
        let c = canary();
        assert_eq!(c.base.chain_id, MAINNET_CHAIN_ID);
        assert!(c.live_section.enabled);
        assert_eq!(c.live_section.stage_required, "G4");
    }

    #[test]
    fn profiles_share_no_addresses() {
        // S8: mainnet Vault/router/token addresses have no code on Sepolia;
        // reusing one is a misconfiguration that must fail loudly here.
        let s = sepolia().pinned_addresses();
        let c = canary().pinned_addresses();
        let overlap: Vec<_> = s.intersection(&c).collect();
        assert!(
            overlap.is_empty(),
            "profile address overlap (S8 violation): {overlap:?}"
        );
    }

    #[test]
    fn stage_ordering_is_total() {
        for (label, want) in [("G0", 0), ("G3", 3), ("G4", 4), ("G6", 6)] {
            assert_eq!(stage_order(label), Some(want));
        }
        assert_eq!(stage_order("G7"), None);
        assert_eq!(stage_order("g3"), None);
        assert_eq!(stage_order(""), None);
    }

    #[test]
    fn stage_label_roundtrips_stage_order() {
        for n in 0..=6 {
            let label = stage_label(n).expect("G0..=G6 labels exist");
            assert_eq!(stage_order(label), Some(n));
        }
        assert_eq!(stage_label(7), None);
    }

    #[test]
    fn rejects_loosened_stage_minimum() {
        let mut s = sepolia();
        s.live_section.stage_required = "G2".to_string();
        assert_eq!(
            s.validate(),
            Err(ProfileError::StageBelowChainMinimum(
                "G2".to_string(),
                "G3".to_string(),
                SEPOLIA_CHAIN_ID
            ))
        );
        let mut c = canary();
        c.live_section.stage_required = "G3".to_string();
        assert!(matches!(
            c.validate(),
            Err(ProfileError::StageBelowChainMinimum(..))
        ));
        let mut bad = sepolia();
        bad.live_section.stage_required = "nope".to_string();
        assert_eq!(
            bad.validate(),
            Err(ProfileError::BadStage("nope".to_string()))
        );
    }

    #[test]
    fn rejects_hard_max_above_compiled() {
        let mut s = sepolia();
        s.live_section.hard_max_flash_usdc = crate::risk::HARD_MAX_FLASH_USDC + 1;
        assert!(matches!(
            s.validate(),
            Err(ProfileError::HardFlashAboveCompiled(..))
        ));
    }

    #[test]
    fn rejects_base_above_profile_hard() {
        let mut s = sepolia();
        s.live_section.hard_max_flash_usdc = s.base.max_flash_usdc - 1;
        assert!(matches!(
            s.validate(),
            Err(ProfileError::BaseFlashAboveHard(..))
        ));
    }

    #[test]
    fn rejects_float_cap_above_gas_only_ceiling() {
        let mut s = sepolia();
        s.live_section.operator_float_cap_usdc = MAX_OPERATOR_FLOAT_USDC + 1;
        assert!(matches!(
            s.validate(),
            Err(ProfileError::FloatCapTooHigh(..))
        ));
    }

    #[test]
    fn rejects_universal_router_and_bad_depth() {
        use crate::config::addresses as a;
        let mut c = canary();
        c.live_section.routers.push(a::UNIV3_UNIVERSAL_ROUTER);
        assert_eq!(c.validate(), Err(ProfileError::UniversalRouterListed));
        let mut c = canary();
        c.live_section.confirmation_depth = 0;
        assert_eq!(c.validate(), Err(ProfileError::BadConfirmationDepth(0)));
    }

    #[test]
    fn base_config_load_ignores_live_section() {
        // The paper binary reads profiles as plain `BotConfig` (dry-run
        // analysis); the `[live]` table must not break that path.
        for f in ["config/sepolia.toml", "config/mainnet-canary.toml"] {
            let cfg = BotConfig::load(Path::new(f)).expect("base load works");
            assert!(cfg.validate().is_ok());
        }
    }
}
