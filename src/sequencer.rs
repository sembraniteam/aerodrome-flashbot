//! Chainlink L2 Sequencer Uptime Feed gate for Base.
//!
//! Any `execute` submission path MUST consult this gate first and refuse to
//! send when the sequencer is down or still inside the post-recovery grace
//! period. Default is fail-closed: unknown status (feed missing, RPC error,
//! unconfigured network) means "do not send".
//!
//! Addresses (verify against <https://docs.chain.link/data-feeds/l2-sequencer-feeds>
//! before relying on them; Chainlink may rotate proxies):
//!
//! - Base mainnet (chain id 8453):
//!   `0xBCF85224fc0756B9Fa45aA7892530B47e10b6433`
//!   (`AggregatorProxy` -> `OptimismSequencerUptimeFeed`, answer `0` = up,
//!   `1` = down).
//! - Base Sepolia (chain id 84532): NO Chainlink sequencer feed is published
//!   in the docs page above (mainnets only). Treat as UNCONFIGURED and fail
//!   closed until an official address is verified; the Sepolia runbook drill
//!   must re-check the docs before wiring any address.
//!
//! Grace period: 3600 s (1 h), the Chainlink-documented standard. After the
//! sequencer flips back to up, on-chain data may still be stale while it
//! catches up, so submissions stay refused until
//! `now - startedAt >= GRACE_PERIOD_SECS`.
//!
//! This module is pure decision logic plus one read-only `latestRoundData`
//! query. No keys, no signing, no broadcast.

use alloy::primitives::{Address, I256};
use alloy::providers::Provider;
use alloy::sol;
use std::future::IntoFuture as _;
use std::time::Duration;

use crate::config::addresses;

sol! {
    /// Chainlink `AggregatorV2V3Interface` subset: the sequencer uptime feed
    /// MUST be read through `latestRoundData` (NOT a plain `latestAnswer`).
    #[sol(rpc)]
    interface ISequencerUptimeFeed {
        function latestRoundData() external view returns (
            uint80 roundId,
            int256 answer,
            uint256 startedAt,
            uint256 updatedAt,
            uint80 answeredInRound
        );
    }
}

/// Chainlink L2 Sequencer Uptime Feed proxy on Base mainnet.
/// Source: <https://docs.chain.link/data-feeds/l2-sequencer-feeds>.
pub const SEQUENCER_FEED_BASE_MAINNET: Address =
    address_from_hex_const("0xBCF85224fc0756B9Fa45aA7892530B47e10b6433");

/// Post-recovery grace period in seconds (Chainlink standard: 1 hour).
pub const GRACE_PERIOD_SECS: u64 = 3600;

/// Minimal `const` hex parser (same rationale as `config::addresses`: fail
/// closed at compile time on malformed input).
const fn address_from_hex_const(s: &str) -> Address {
    let bytes = s.as_bytes();
    if bytes.len() != 42 || bytes[0] != b'0' || bytes[1] != b'x' {
        panic!("sequencer feed must be 0x-prefixed 40 hex chars");
    }
    let mut out = [0u8; 20];
    let mut i = 0;
    while i < 20 {
        let hi = hex_val_const(bytes[2 + i * 2]);
        let lo = hex_val_const(bytes[2 + i * 2 + 1]);
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    Address::new(out)
}

const fn hex_val_const(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("invalid hex char in sequencer feed address"),
    }
}

/// Feed to consult for a chain. `None` = unconfigured -> fail closed.
pub fn feed_for_chain(chain_id: u64) -> Option<Address> {
    match chain_id {
        8453 => Some(SEQUENCER_FEED_BASE_MAINNET),
        _ => None,
    }
}

/// Latest-round snapshot of the sequencer feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequencerStatus {
    /// `0` = sequencer up, `1` = down (Chainlink convention).
    pub answer: I256,
    /// Timestamp of the last status flip.
    pub started_at: u64,
    /// When this snapshot was taken (chain time for live reads).
    pub observed_at: u64,
}

/// Pure gate: true only when the sequencer reports up AND the post-recovery
/// grace period has fully elapsed. Anything else (down, grace, future
/// timestamps, negative answers) is "do not send".
pub fn sequencer_up(status: &SequencerStatus, grace_secs: u64) -> bool {
    if status.answer != I256::ZERO {
        return false;
    }
    let Some(elapsed) = status.observed_at.checked_sub(status.started_at) else {
        return false;
    };
    elapsed >= grace_secs
}

/// Submission gate used by the runner: `None` (unknown/unreadable status)
/// refuses, mirroring [`crate::risk`] fail-closed behaviour. There is no
/// "send anyway" path.
pub fn should_send_execute(status: Option<&SequencerStatus>, grace_secs: u64) -> bool {
    status.is_some_and(|s| sequencer_up(s, grace_secs))
}

/// Read-only `latestRoundData` query against the configured feed.
/// `Ok(None)` = chain has no verified feed (fail closed downstream).
/// `Err` = RPC problem (fail closed downstream, never default to "up").
pub async fn query_status<P>(
    provider: &P,
    chain_id: u64,
    now_secs: u64,
) -> anyhow::Result<Option<SequencerStatus>>
where
    P: Provider + Clone,
{
    let Some(feed) = feed_for_chain(chain_id) else {
        return Ok(None);
    };
    debug_assert_eq!(
        feed,
        addresses::SEQUENCER_UPTIME_FEED_BASE,
        "sequencer feed drifted from the address registry"
    );
    let ret = tokio::time::timeout(
        Duration::from_secs(15),
        ISequencerUptimeFeed::new(feed, provider.clone())
            .latestRoundData()
            .call()
            .into_future(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("sequencer feed query timed out"))?
    .map_err(|e| anyhow::anyhow!("sequencer feed query failed: {e:#}"))?;
    let started_at = u64::try_from(ret.startedAt).unwrap_or(u64::MAX);
    Ok(Some(SequencerStatus {
        answer: ret.answer,
        started_at,
        observed_at: now_secs,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up_snapshot(elapsed_secs: u64) -> SequencerStatus {
        SequencerStatus {
            answer: I256::ZERO,
            started_at: 1_700_000_000,
            observed_at: 1_700_000_000 + elapsed_secs,
        }
    }

    #[test]
    fn up_after_grace_sends() {
        assert!(sequencer_up(
            &up_snapshot(GRACE_PERIOD_SECS),
            GRACE_PERIOD_SECS
        ));
        assert!(sequencer_up(
            &up_snapshot(GRACE_PERIOD_SECS + 1),
            GRACE_PERIOD_SECS
        ));
        assert!(should_send_execute(
            Some(&up_snapshot(GRACE_PERIOD_SECS)),
            GRACE_PERIOD_SECS
        ));
    }

    #[test]
    fn down_answer_refuses() {
        let s = SequencerStatus {
            answer: I256::ONE,
            started_at: 1_700_000_000,
            observed_at: 1_700_000_000 + 10 * GRACE_PERIOD_SECS,
        };
        assert!(!sequencer_up(&s, GRACE_PERIOD_SECS));
        assert!(!should_send_execute(Some(&s), GRACE_PERIOD_SECS));
    }

    #[test]
    fn grace_period_refuses() {
        // Just recovered: up answer but grace not elapsed -> still refuse.
        assert!(!sequencer_up(
            &up_snapshot(GRACE_PERIOD_SECS - 1),
            GRACE_PERIOD_SECS
        ));
        assert!(!should_send_execute(
            Some(&up_snapshot(0)),
            GRACE_PERIOD_SECS
        ));
    }

    #[test]
    fn unknown_status_fails_closed() {
        // No snapshot (unconfigured chain, RPC error): never send.
        assert!(!should_send_execute(None, GRACE_PERIOD_SECS));
        // Clock skew (startedAt in the future): never send.
        let skewed = SequencerStatus {
            answer: I256::ZERO,
            started_at: 1_700_000_100,
            observed_at: 1_700_000_000,
        };
        assert!(!sequencer_up(&skewed, GRACE_PERIOD_SECS));
    }

    #[test]
    fn feed_wiring_matches_registry() {
        assert_eq!(
            feed_for_chain(addresses::BASE_CHAIN_ID),
            Some(addresses::SEQUENCER_UPTIME_FEED_BASE)
        );
        assert_eq!(
            feed_for_chain(84532),
            None,
            "sepolia feed unconfigured: fail closed"
        );
        assert_eq!(feed_for_chain(1), None);
    }

    #[test]
    fn feed_address_is_documented_value() {
        let expected: Address = "0xBCF85224fc0756B9Fa45aA7892530B47e10b6433"
            .parse()
            .unwrap();
        assert_eq!(SEQUENCER_FEED_BASE_MAINNET, expected);
    }
}
