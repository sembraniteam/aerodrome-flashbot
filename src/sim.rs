//! Dry-run simulation: `eth_call` / `estimateGas`, never broadcast.
//!
//! The live path (provider-backed) is behind an explicit `dry_run == false`
//! flag owned by the user; the paper binary forces dry-run and only uses the
//! pure verification helpers below plus recorded/fork state.

use alloy::primitives::U256;

/// Result of a dry-run simulation of one arbitrage attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulationResult {
    /// Quoted output from the `eth_call` quoter (authoritative).
    pub onchain_out: U256,
    /// Off-chain estimate for the same input (pre-filter).
    pub offchain_out: U256,
    /// Gas used by `estimateGas`.
    pub gas_estimate: u64,
    /// Whether the `eth_call` itself would revert.
    pub would_revert: bool,
}

/// Compare off-chain math against the on-chain quoter result.
/// Returns `true` when within `tolerance_bps` (relative). Any systematic
/// mismatch is a bug until explained: do not trade on diverging math.
pub fn verify_quote(offchain_out: U256, onchain_out: U256, tolerance_bps: u64) -> bool {
    if onchain_out.is_zero() {
        return offchain_out.is_zero();
    }
    let diff = offchain_out.abs_diff(onchain_out);
    // diff_bps = diff * 10000 / onchain, all integer.
    let diff_bps = diff.saturating_mul(U256::from(10_000u64)) / onchain_out.max(U256::from(1u8));
    diff_bps <= U256::from(tolerance_bps)
}

/// Decide from a dry-run: revert (reject) on simulation revert, on quote
/// divergence beyond tolerance, or on profit below minimum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SimDecision {
    /// Would submit (live path only; paper mode logs instead).
    Submit { net: U256 },
    /// Reject: cheap revert, no gas wasted beyond the call.
    Reject { reason: RejectReason },
}

/// Machine-readable rejection reason. Callers match on the variant (never on
/// substrings); logs render the same human text as before via [`Display`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The `eth_call` itself would revert.
    WouldRevert,
    /// Off-chain estimate and on-chain quoter disagree beyond tolerance.
    QuoteDiverged,
    /// Flat costs plus principal exceed the quoted output.
    CostsExceedOutput,
    /// Net clears costs but not the minimum-profit gate.
    BelowMinimum,
}

impl RejectReason {
    /// Human text for logs/CSV (stable strings, kept identical to the
    /// previous `&'static str` reasons).
    pub fn as_str(&self) -> &'static str {
        match self {
            RejectReason::WouldRevert => "eth_call would revert",
            RejectReason::QuoteDiverged => "offchain/onchain quote diverged",
            RejectReason::CostsExceedOutput => "costs exceed output",
            RejectReason::BelowMinimum => "net below minimum",
        }
    }
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

pub fn decide_from_simulation(
    sim: &SimulationResult,
    amount_in: U256,
    flat_costs: U256,
    min_net: U256,
    tolerance_bps: u64,
) -> SimDecision {
    if sim.would_revert {
        return SimDecision::Reject {
            reason: RejectReason::WouldRevert,
        };
    }
    if !verify_quote(sim.offchain_out, sim.onchain_out, tolerance_bps) {
        return SimDecision::Reject {
            reason: RejectReason::QuoteDiverged,
        };
    }
    let Some(net) = sim
        .onchain_out
        .checked_sub(flat_costs)
        .and_then(|v| v.checked_sub(amount_in))
    else {
        return SimDecision::Reject {
            reason: RejectReason::CostsExceedOutput,
        };
    };
    if net < min_net {
        return SimDecision::Reject {
            reason: RejectReason::BelowMinimum,
        };
    }
    SimDecision::Submit { net }
}

/// Gas-limit helper: simulation gas plus margin, capped. Never retry blindly:
/// callers must re-simulate and re-check limits before resubmission.
pub fn gas_limit_with_margin(gas_estimate: u64, margin_bps: u64, cap: u64) -> u64 {
    let with_margin =
        (u128::from(gas_estimate) * u128::from(10_000 + margin_bps.min(10_000))) / 10_000;
    with_margin.min(u128::from(cap)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_within_tolerance() {
        assert!(verify_quote(U256::from(10050u64), U256::from(10000u64), 50));
        assert!(!verify_quote(
            U256::from(10100u64),
            U256::from(10000u64),
            50
        ));
    }

    #[test]
    fn verify_zero_handling() {
        assert!(verify_quote(U256::ZERO, U256::ZERO, 10));
        assert!(!verify_quote(U256::from(1u64), U256::ZERO, 10));
    }

    #[test]
    fn revert_always_rejects() {
        let sim = SimulationResult {
            onchain_out: U256::from(2_000_000u64),
            offchain_out: U256::from(2_000_000u64),
            gas_estimate: 200_000,
            would_revert: true,
        };
        assert_eq!(
            decide_from_simulation(&sim, U256::from(1u64), U256::ZERO, U256::ZERO, 50),
            SimDecision::Reject {
                reason: RejectReason::WouldRevert
            }
        );
    }

    #[test]
    fn divergence_rejects_before_profit_check() {
        let sim = SimulationResult {
            onchain_out: U256::from(1_000_000u64),
            offchain_out: U256::from(2_000_000u64),
            gas_estimate: 200_000,
            would_revert: false,
        };
        assert_eq!(
            decide_from_simulation(&sim, U256::from(1u64), U256::ZERO, U256::ZERO, 50),
            SimDecision::Reject {
                reason: RejectReason::QuoteDiverged
            }
        );
    }

    #[test]
    fn thin_profit_rejects() {
        let sim = SimulationResult {
            onchain_out: U256::from(1_000_000u64),
            offchain_out: U256::from(1_000_000u64),
            gas_estimate: 200_000,
            would_revert: false,
        };
        assert_eq!(
            decide_from_simulation(
                &sim,
                U256::from(999_000u64),
                U256::ZERO,
                U256::from(5_000u64),
                50
            ),
            SimDecision::Reject {
                reason: RejectReason::BelowMinimum
            }
        );
    }

    #[test]
    fn reject_reason_text_is_stable() {
        // Logs/CSV keep the historical strings even though callers now match
        // on the enum.
        assert_eq!(RejectReason::WouldRevert.as_str(), "eth_call would revert");
        assert_eq!(
            RejectReason::QuoteDiverged.as_str(),
            "offchain/onchain quote diverged"
        );
        assert_eq!(
            RejectReason::CostsExceedOutput.as_str(),
            "costs exceed output"
        );
        assert_eq!(RejectReason::BelowMinimum.as_str(), "net below minimum");
        assert_eq!(
            format!("{}", RejectReason::QuoteDiverged),
            "offchain/onchain quote diverged"
        );
    }

    #[test]
    fn good_sim_submits() {
        let sim = SimulationResult {
            onchain_out: U256::from(1_020_000u64),
            offchain_out: U256::from(1_020_000u64),
            gas_estimate: 200_000,
            would_revert: false,
        };
        assert_eq!(
            decide_from_simulation(
                &sim,
                U256::from(1_000_000u64),
                U256::from(1_000u64),
                U256::from(5_000u64),
                50
            ),
            SimDecision::Submit {
                net: U256::from(19_000u64)
            }
        );
    }

    #[test]
    fn gas_margin_and_cap() {
        assert_eq!(gas_limit_with_margin(200_000, 2000, 1_000_000), 240_000);
        assert_eq!(gas_limit_with_margin(900_000, 2000, 1_000_000), 1_000_000);
    }
}
