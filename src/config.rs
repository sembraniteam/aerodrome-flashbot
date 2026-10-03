//! Runtime configuration: testnet-safe defaults, dry-run enforced.
//!
//! All monetary limits are expressed in USDC base units (6 decimals) as
//! integers. No floats appear in the profit/risk path.

use alloy::primitives::Address;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Verified Base mainnet (chain id 8453) contract addresses.
pub mod addresses {
    use alloy::primitives::Address;

    /// Accepts `0x`-prefixed hex at compile time via `const fn`-friendly parse.
    /// We keep this tiny on purpose; callers use the constants below.
    pub const BALANCER_VAULT: Address =
        address_from_hex("0xBA12222222228d8Ba445958a75a0704d566BF2C8");
    pub const AERO_SLIPSTREAM_FACTORY_V1: Address =
        address_from_hex("0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A");
    pub const AERO_QUOTER_V2: Address =
        address_from_hex("0x254cF9E1E6e233aa1AC962CB9B05b2cfeAaE15b0");
    pub const AERO_SWAP_ROUTER: Address =
        address_from_hex("0xBE6D8f0d05cC4be24d5167a3eF062215bE6D18a5");
    pub const UNIV3_FACTORY_BASE: Address =
        address_from_hex("0x33128a8fC17869897dcE68Ed026d694621f6FDfD");
    pub const UNIV3_QUOTER_V2: Address =
        address_from_hex("0x3d4e44Eb1374240CE5F1B871ab261CD16335B76a");
    pub const UNIV3_SWAP_ROUTER02: Address =
        address_from_hex("0x2626664c2603336E57B271c5C0b26F421741e481");
    pub const UNIV3_UNIVERSAL_ROUTER: Address =
        address_from_hex("0x6fF5693b99212Da76ad316178A184AB56D299b43");

    pub const USDC_NATIVE: Address = address_from_hex("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
    pub const WETH: Address = address_from_hex("0x4200000000000000000000000000000000000006");
    pub const AERO: Address = address_from_hex("0x940181a94A35A4569E4529A3CDfB74e38FD98631");

    /// Chainlink L2 Sequencer Uptime Feed proxy on Base mainnet (answer 0 =
    /// up, 1 = down; 3600 s grace). Source:
    /// <https://docs.chain.link/data-feeds/l2-sequencer-feeds>.
    /// Canonical single source of truth for the feed address; the
    /// [`crate::sequencer`] module asserts equality with its own constant so
    /// drift breaks the build. No Sepolia feed is published there: Sepolia
    /// lookups fail closed (see [`crate::sequencer::feed_for_chain`]).
    pub const SEQUENCER_UPTIME_FEED_BASE: Address =
        address_from_hex("0xBCF85224fc0756B9Fa45aA7892530B47e10b6433");

    /// Base chain id.
    pub const BASE_CHAIN_ID: u64 = 8453;

    /// Minimal `const` hex parser for `0x`-prefixed 20-byte addresses.
    /// Panics at compile time on malformed input (fail closed).
    const fn address_from_hex(s: &str) -> Address {
        let bytes = s.as_bytes();
        // "0x" + 40 hex chars
        if bytes.len() != 42 || bytes[0] != b'0' || bytes[1] != b'x' {
            panic!("address must be 0x-prefixed 40 hex chars");
        }
        let mut out = [0u8; 20];
        let mut i = 0;
        while i < 20 {
            let hi = hex_val(bytes[2 + i * 2]);
            let lo = hex_val(bytes[2 + i * 2 + 1]);
            out[i] = (hi << 4) | lo;
            i += 1;
        }
        Address::new(out)
    }

    const fn hex_val(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => panic!("invalid hex char in address"),
        }
    }
}

/// One allowlisted trading pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairConfig {
    /// Human label, e.g. "WETH/USDC".
    pub name: String,
    /// First token of the pair.
    pub token_a: Address,
    /// Second token of the pair.
    pub token_b: Address,
    /// Base-unit decimals of token_a (e.g. 18 for WETH).
    pub decimals_a: u8,
    /// Base-unit decimals of token_b (e.g. 6 for USDC).
    pub decimals_b: u8,
    /// If true this pair is evaluated by the paper harness.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_deadline_secs() -> u64 {
    30
}

fn default_max_in_flight() -> u32 {
    1
}

/// Default tolerated head age in blocks for the staleness gate.
fn default_head_staleness_blocks() -> u64 {
    5
}

/// Full bot configuration. Deserialized from `config/default.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    /// Master safety switch. Live execution paths refuse to run unless an
    /// explicit, separate user flag flips this AND the binary is built with
    /// the live path enabled. Paper binary forces `true`.
    pub dry_run: bool,
    /// Expected chain id (8453 = Base mainnet, 84532 = Base Sepolia).
    pub chain_id: u64,
    /// WebSocket RPC endpoint. The value itself is secret-adjacent (may
    /// contain provider keys): never log it in full, see
    /// [`crate::chain::redact_url`].
    pub rpc_ws_url: String,
    /// Which block-feed mode to use: "flashblocks" (pending-tag
    /// preconfirmations, if still available) or "canonical" (200 ms blocks
    /// post-Denim). Selected by config, never scattered as literals.
    pub feed_mode: FeedMode,
    /// Max flash-loan principal per trade, in USDC base units (6 decimals).
    /// Default $500 -> 500_000_000.
    pub max_flash_usdc: u64,
    /// Minimum acceptable net profit per trade, in USDC base units.
    /// Default $5 -> 5_000_000.
    pub min_net_profit_usdc: u64,
    /// Max tolerated slippage in basis points (100 bps = 1%).
    pub max_slippage_bps: u64,
    /// Daily net-loss halt threshold, in USDC base units.
    pub daily_loss_cap_usdc: u64,
    /// Consecutive failures before the circuit breaker halts trading.
    pub max_consecutive_failures: u32,
    /// Optional private-relay RPC endpoint for submissions (e.g. a
    /// Flashbots-Protect-style private mempool URL). Read-only paper mode
    /// never touches it; the value is secret-adjacent (may embed a key) so
    /// it is only ever logged redacted. Empty/absent = no private relay.
    #[serde(default)]
    pub private_rpc_url: Option<String>,
    /// Submission deadline horizon in seconds from quote time. The executor
    /// reverts past `deadline`, so this bounds exposure to stale quotes and
    /// front-running. Default 30 s, hard range 1..=300 s.
    #[serde(default = "default_deadline_secs")]
    pub deadline_secs: u64,
    /// Max simultaneous in-flight transactions. Pinned to 1: overlapping
    /// executes share nonce/state and break the revert-not-lose accounting.
    /// The config accepts no other value (fail closed).
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight: u32,
    /// Tolerated head age in blocks for the risk staleness gate
    /// ([`crate::risk::RiskState::check_trade`]): a candidate whose head is
    /// older than `last_head - head_staleness_blocks` is rejected. Default 5,
    /// hard range 1..=1024.
    #[serde(default = "default_head_staleness_blocks")]
    pub head_staleness_blocks: u64,
    /// Allowlisted pairs only. Anything else is rejected before simulation.
    pub pairs: Vec<PairConfig>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum FeedMode {
    /// Canonical 200 ms blocks (Denim upgrade and later). DEFAULT: Denim
    /// replaces Flashblocks 200 ms sub-blocks with canonical 200 ms blocks
    /// (targeted ~Oct 2026, not final -- re-verify against `docs.base.org`),
    /// so the `pending`-tag preconfirmation feed is no longer the safe
    /// assumption. Select `Flashblocks` explicitly only while pre-Denim
    /// behavior is confirmed live.
    #[default]
    Canonical,
    /// Flashblocks / pending-tag preconfirmation feed (pre-Denim only).
    Flashblocks,
}

/// Testnet-safe defaults: dry-run on, $500 max flash, $5 min profit.
impl Default for BotConfig {
    fn default() -> Self {
        Self {
            dry_run: true,
            chain_id: addresses::BASE_CHAIN_ID,
            rpc_ws_url: "ws://127.0.0.1:8545".to_string(),
            feed_mode: FeedMode::Canonical,
            max_flash_usdc: 500_000_000,      // $500
            min_net_profit_usdc: 5_000_000,   // $5
            max_slippage_bps: 50,             // 0.50%
            daily_loss_cap_usdc: 100_000_000, // $100
            max_consecutive_failures: 3,
            private_rpc_url: None,
            deadline_secs: default_deadline_secs(),
            max_in_flight: default_max_in_flight(),
            head_staleness_blocks: default_head_staleness_blocks(),
            pairs: default_pairs(),
        }
    }
}

/// Allowlist: 4 pairs. cbETH/ETH legs are placeholders for a follow-up that
/// adds the real cbETH address after verification; they ship disabled so the
/// allowlist gate can be unit-tested without touching unverified addresses.
pub fn default_pairs() -> Vec<PairConfig> {
    use addresses::{AERO, USDC_NATIVE, WETH};
    vec![
        PairConfig {
            name: "WETH/USDC".to_string(),
            token_a: WETH,
            token_b: USDC_NATIVE,
            decimals_a: 18,
            decimals_b: 6,
            enabled: true,
        },
        PairConfig {
            name: "AERO/USDC".to_string(),
            token_a: AERO,
            token_b: USDC_NATIVE,
            decimals_a: 18,
            decimals_b: 6,
            enabled: true,
        },
        PairConfig {
            name: "AERO/WETH".to_string(),
            token_a: AERO,
            token_b: WETH,
            decimals_a: 18,
            decimals_b: 18,
            enabled: true,
        },
        // Placeholder leg: zero address, disabled. Flip on only after the
        // verified cbETH address is added to `addresses` + allowlist review.
        PairConfig {
            name: "cbETH/ETH-DISABLED".to_string(),
            token_a: Address::ZERO,
            token_b: WETH,
            decimals_a: 18,
            decimals_b: 18,
            enabled: false,
        },
    ]
}

impl BotConfig {
    /// Load from a TOML file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Fail-closed validation of every safety-relevant field.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.chain_id == 8453 || self.chain_id == 84532,
            "unexpected chain_id"
        );
        anyhow::ensure!(self.max_flash_usdc > 0, "max_flash_usdc must be > 0");
        anyhow::ensure!(
            self.max_flash_usdc <= crate::risk::HARD_MAX_FLASH_USDC,
            "max_flash_usdc exceeds the hard ceiling (control plane can never raise it)"
        );
        anyhow::ensure!(
            self.min_net_profit_usdc > 0,
            "min_net_profit_usdc must be > 0"
        );
        anyhow::ensure!(
            self.max_slippage_bps <= 10_000,
            "max_slippage_bps must be <= 10000"
        );
        anyhow::ensure!(
            self.max_consecutive_failures > 0,
            "max_consecutive_failures must be > 0"
        );
        anyhow::ensure!(
            self.deadline_secs >= 1 && self.deadline_secs <= 300,
            "deadline_secs must be 1..=300"
        );
        anyhow::ensure!(
            self.max_in_flight == 1,
            "max_in_flight is pinned to 1 (no overlapping executes)"
        );
        anyhow::ensure!(
            self.head_staleness_blocks >= 1 && self.head_staleness_blocks <= 1024,
            "head_staleness_blocks must be 1..=1024"
        );
        if let Some(url) = self.private_rpc_url.as_deref() {
            anyhow::ensure!(!url.trim().is_empty(), "private_rpc_url must not be blank");
        }
        anyhow::ensure!(!self.pairs.is_empty(), "pair allowlist must not be empty");
        anyhow::ensure!(
            self.pairs.iter().any(|p| p.enabled),
            "at least one pair must be enabled"
        );
        for p in &self.pairs {
            anyhow::ensure!(!p.name.is_empty(), "pair name must not be empty");
            if p.enabled {
                anyhow::ensure!(
                    p.token_a != Address::ZERO && p.token_b != Address::ZERO,
                    "enabled pair '{}' has zero address",
                    p.name
                );
                anyhow::ensure!(p.token_a != p.token_b, "pair '{}' tokens identical", p.name);
            }
        }
        Ok(())
    }

    /// Look up an enabled pair by name.
    pub fn pair(&self, name: &str) -> Option<&PairConfig> {
        self.pairs.iter().find(|p| p.name == name && p.enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_dry_run_and_safe() {
        let cfg = BotConfig::default();
        assert!(cfg.dry_run, "dry-run must default to true");
        assert_eq!(cfg.chain_id, 8453);
        assert_eq!(cfg.feed_mode, FeedMode::Canonical);
        assert_eq!(cfg.head_staleness_blocks, 5);
        assert_eq!(cfg.max_flash_usdc, 500_000_000);
        assert_eq!(cfg.min_net_profit_usdc, 5_000_000);
        assert_eq!(cfg.deadline_secs, 30);
        assert_eq!(cfg.max_in_flight, 1);
        assert_eq!(cfg.private_rpc_url, None);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn sequencer_feed_registry_matches_docs() {
        let expected: Address = "0xBCF85224fc0756B9Fa45aA7892530B47e10b6433"
            .parse()
            .unwrap();
        assert_eq!(addresses::SEQUENCER_UPTIME_FEED_BASE, expected);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn rejects_bad_deadline() {
        let mut cfg = BotConfig::default();
        cfg.deadline_secs = 0;
        assert!(cfg.validate().is_err());
        cfg.deadline_secs = 301;
        assert!(cfg.validate().is_err());
        cfg.deadline_secs = 30;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn in_flight_pinned_to_one() {
        let mut cfg = BotConfig::default();
        cfg.max_in_flight = 2;
        assert!(cfg.validate().is_err());
        cfg.max_in_flight = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn blank_private_url_rejected() {
        let mut cfg = BotConfig::default();
        cfg.private_rpc_url = Some("   ".to_string());
        assert!(cfg.validate().is_err());
        cfg.private_rpc_url = Some("https://protect.example/rpc".to_string());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn max_flash_above_hard_ceiling_rejected() {
        let mut cfg = BotConfig::default();
        cfg.max_flash_usdc = crate::risk::HARD_MAX_FLASH_USDC + 1;
        assert!(cfg.validate().is_err());
        cfg.max_flash_usdc = crate::risk::HARD_MAX_FLASH_USDC;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn rejects_bad_head_staleness() {
        let mut cfg = BotConfig::default();
        cfg.head_staleness_blocks = 0;
        assert!(cfg.validate().is_err());
        cfg.head_staleness_blocks = 1025;
        assert!(cfg.validate().is_err());
        cfg.head_staleness_blocks = 5;
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn known_addresses_parse() {
        // Spot-check the const parser against string parsing.
        let expected: Address = "0xBA12222222228d8Ba445958a75a0704d566BF2C8"
            .parse()
            .unwrap();
        assert_eq!(addresses::BALANCER_VAULT, expected);
        let usdc: Address = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"
            .parse()
            .unwrap();
        assert_eq!(addresses::USDC_NATIVE, usdc);
    }

    #[test]
    fn rejects_empty_allowlist() {
        let mut cfg = BotConfig::default();
        cfg.pairs.clear();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_all_disabled() {
        let mut cfg = BotConfig::default();
        for p in cfg.pairs.iter_mut() {
            p.enabled = false;
        }
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_enabled_zero_address() {
        let mut cfg = BotConfig::default();
        cfg.pairs.push(PairConfig {
            name: "BAD".to_string(),
            token_a: Address::ZERO,
            token_b: addresses::WETH,
            decimals_a: 18,
            decimals_b: 18,
            enabled: true,
        });
        assert!(cfg.validate().is_err());
    }
}
