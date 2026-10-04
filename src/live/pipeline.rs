//! L5 pre-submit pipeline: every step fail-closed, in fixed order.
//!
//! 1. Kill switch disengaged (file flag AND on-chain pause state as read).
//! 2. Sequencer uptime gate + grace (real [`crate::sequencer`] predicate).
//! 3. Head freshness + two-provider agreement.
//! 4. Operator float cap (continuous: startup AND every submit).
//! 5. Opportunity estimate in integer base units with the measured L1 fee;
//!    net must exceed `min_net + safety_margin` (real [`crate::profit`]).
//! 6. `sim::verify_quote` against the quoter within tolerance.
//! 7. Risk gate (real [`crate::risk::RiskState::check_trade`]).
//! 8. Final simulation decision + gas/fee caps (real [`crate::sim`]).
//! 9. Persist the intent to the ledger BEFORE approval. Only an
//!    [`ApprovedIntent`] (single-use: moved into the sender) authorizes a
//!    submit; a retry re-runs this pipeline for a fresh approval.
//!
//! Any `Err`, timeout, or ambiguity rejects with the failing [`Gate`]. No
//! `unwrap_or(true)`-style defaults anywhere.

use alloy::primitives::U256;
use thiserror::Error;

use crate::profit::{CostModel, net_profit};
use crate::risk::{RiskError, RiskState};
use crate::sequencer::{SequencerStatus, should_send_execute};
use crate::sim::{self, SimulationResult};

use super::float_cap::{FloatCapError, check_float_cap};

/// Which gate rejected the opportunity (stable machine-readable names for
/// the ledger and evidence rejections map).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    KillSwitch,
    Sequencer,
    Freshness,
    Providers,
    FloatCap,
    Estimate,
    Quote,
    Risk,
    Simulation,
    GasCaps,
    Ledger,
}

impl Gate {
    pub fn as_str(&self) -> &'static str {
        match self {
            Gate::KillSwitch => "kill-switch",
            Gate::Sequencer => "sequencer",
            Gate::Freshness => "freshness",
            Gate::Providers => "providers",
            Gate::FloatCap => "float-cap",
            Gate::Estimate => "estimate",
            Gate::Quote => "quote",
            Gate::Risk => "risk",
            Gate::Simulation => "simulation",
            Gate::GasCaps => "gas-caps",
            Gate::Ledger => "ledger",
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
#[error("{gate}: {reason}")]
pub struct PresubmitReject {
    pub gate: &'static str,
    pub reason: String,
}

impl PresubmitReject {
    fn at(gate: Gate, reason: impl Into<String>) -> Self {
        Self {
            gate: gate.as_str(),
            reason: reason.into(),
        }
    }
}

/// Ledger sink the pipeline persists the intent to BEFORE approval.
/// Production adapter wraps [`super::ledger::Ledger`]; tests use a fake.
pub trait PresubmitLedger {
    fn record_intent(&mut self, body: serde_json::Value) -> Result<u64, String>;
}

/// All inputs for one pre-submit evaluation. Providers/adapters fill these
/// from live RPC; a retry fills them fresh (never reuses a stale approval).
pub struct PresubmitInputs<'a> {
    pub kill_switch_engaged: bool,
    pub sequencer: Option<&'a SequencerStatus>,
    pub sequencer_grace_secs: u64,
    pub head: Option<u64>,
    pub last_head: Option<u64>,
    pub max_head_age: u64,
    pub providers_agree: bool,
    pub float_balance_equiv: Option<u64>,
    pub float_cap: u64,
    pub gross_out: U256,
    pub amount_in: U256,
    pub costs: &'a CostModel,
    pub min_net: U256,
    pub safety_margin: U256,
    pub offchain_out: U256,
    pub onchain_out: U256,
    pub tolerance_bps: u64,
    pub risk: &'a RiskState,
    pub pair_name: &'a str,
    pub allowlisted: bool,
    pub size_u64: u64,
    pub slippage_bps: u64,
    pub sim: &'a SimulationResult,
    pub gas_margin_bps: u64,
    pub gas_cap: u64,
    pub max_fee: u128,
    pub fee_cap: u128,
    pub priority_fee: u128,
    pub priority_cap: u128,
    pub l1_fee: u128,
    pub l1_cap: u128,
    pub ledger: &'a mut dyn PresubmitLedger,
}

/// Single-use submit authorization. Moved (not copied) into the sender; a
/// retry needs a fresh pipeline run (the compiler enforces it).
#[derive(Debug)]
pub struct ApprovedIntent {
    pub size: U256,
    pub net: U256,
    pub gas_limit: u64,
    pub ledger_seq: u64,
}

/// Run the L5 pipeline. `Ok` = every gate passed and the intent is
/// persisted; `Err` names the rejecting gate.
pub fn run_presubmit(inputs: PresubmitInputs<'_>) -> Result<ApprovedIntent, PresubmitReject> {
    if inputs.kill_switch_engaged {
        return Err(PresubmitReject::at(Gate::KillSwitch, "kill switch engaged"));
    }
    if !should_send_execute(inputs.sequencer, inputs.sequencer_grace_secs) {
        return Err(PresubmitReject::at(
            Gate::Sequencer,
            "sequencer down/in grace/unknown",
        ));
    }
    match (inputs.head, inputs.last_head) {
        (Some(h), Some(last)) if h.saturating_add(inputs.max_head_age) >= last => {}
        (Some(_), None) => {}
        _ => {
            return Err(PresubmitReject::at(
                Gate::Freshness,
                "head missing or stale",
            ));
        }
    }
    if !inputs.providers_agree {
        return Err(PresubmitReject::at(
            Gate::Providers,
            "provider disagreement",
        ));
    }
    check_float_cap(inputs.float_balance_equiv, inputs.float_cap).map_err(|e| {
        PresubmitReject::at(
            Gate::FloatCap,
            match e {
                FloatCapError::UnknownBalance => "float balance unknown/stale".to_string(),
                FloatCapError::OverCap(b, c) => format!("float {b} over cap {c}"),
            },
        )
    })?;
    let Some(net) = net_profit(inputs.gross_out, inputs.amount_in, inputs.costs) else {
        return Err(PresubmitReject::at(Gate::Estimate, "costs exceed output"));
    };
    if net < inputs.min_net.saturating_add(inputs.safety_margin) {
        return Err(PresubmitReject::at(
            Gate::Estimate,
            format!("net {net} below min + margin"),
        ));
    }
    if !sim::verify_quote(
        inputs.offchain_out,
        inputs.onchain_out,
        inputs.tolerance_bps,
    ) {
        return Err(PresubmitReject::at(
            Gate::Quote,
            "offchain/onchain quote diverged",
        ));
    }
    // Risk gate on the real state machine (saturating u64 conversion mirrors
    // the paper runner; Oversize/BelowMin fail closed downstream anyway).
    let net_u64: u64 = net.try_into().unwrap_or(u64::MAX);
    inputs
        .risk
        .check_trade(
            inputs.pair_name,
            inputs.allowlisted,
            inputs.size_u64,
            Some(net_u64),
            inputs.slippage_bps,
            inputs.head,
            inputs.max_head_age,
        )
        .map_err(|e: RiskError| PresubmitReject::at(Gate::Risk, e.to_string()))?;
    let approved_net = match sim::decide_from_simulation(
        inputs.sim,
        inputs.amount_in,
        inputs.costs,
        inputs.min_net,
        inputs.tolerance_bps,
    ) {
        sim::SimDecision::Submit { net } => net,
        sim::SimDecision::Reject { reason } => {
            return Err(PresubmitReject::at(Gate::Simulation, reason.as_str()));
        }
    };
    // Gas limit = estimate + margin, computed uncapped first: the uncapped
    // value must fit the per-attempt cap (the helper's internal cap is a
    // backstop, not the check).
    let uncapped = (u128::from(inputs.sim.gas_estimate)
        * u128::from(10_000 + inputs.gas_margin_bps.min(10_000)))
        / 10_000;
    if uncapped > u128::from(inputs.gas_cap) {
        return Err(PresubmitReject::at(
            Gate::GasCaps,
            "gas estimate + margin over cap",
        ));
    }
    if inputs.max_fee > inputs.fee_cap {
        return Err(PresubmitReject::at(Gate::GasCaps, "max fee over cap"));
    }
    if inputs.priority_fee > inputs.priority_cap {
        return Err(PresubmitReject::at(Gate::GasCaps, "priority fee over cap"));
    }
    if inputs.l1_fee > inputs.l1_cap {
        return Err(PresubmitReject::at(Gate::GasCaps, "L1 fee over cap"));
    }
    let gas_limit = sim::gas_limit_with_margin(
        inputs.sim.gas_estimate,
        inputs.gas_margin_bps,
        inputs.gas_cap,
    );
    // Intent persisted BEFORE approval: no approval exists without it.
    let ledger_seq = inputs
        .ledger
        .record_intent(serde_json::json!({
            "size": inputs.size_u64,
            "net": approved_net.to_string(),
            "gas_limit": gas_limit,
        }))
        .map_err(|e| PresubmitReject::at(Gate::Ledger, format!("intent persist failed: {e}")))?;
    Ok(ApprovedIntent {
        size: U256::from(inputs.size_u64),
        net: approved_net,
        gas_limit,
        ledger_seq,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk::{RiskLimits, RiskState};
    use crate::sequencer::SequencerStatus;
    use alloy::primitives::I256;

    struct FakeLedger {
        intents: u64,
        fail: bool,
    }

    impl PresubmitLedger for FakeLedger {
        fn record_intent(&mut self, _body: serde_json::Value) -> Result<u64, String> {
            if self.fail {
                return Err("disk full".to_string());
            }
            self.intents += 1;
            Ok(self.intents)
        }
    }

    fn sequencer_up() -> SequencerStatus {
        SequencerStatus {
            answer: I256::ZERO,
            started_at: 1_000,
            observed_at: 1_000 + crate::sequencer::GRACE_PERIOD_SECS,
        }
    }

    /// Baseline inputs where EVERY gate passes (each rejection test mutates
    /// one field). Amounts: gross 60M, principal 50M, fixture-ish costs.
    fn passing_inputs<'a>(
        risk: &'a RiskState,
        ledger: &'a mut FakeLedger,
        seq: &'a SequencerStatus,
        costs: &'a CostModel,
        sim: &'a SimulationResult,
    ) -> PresubmitInputs<'a> {
        PresubmitInputs {
            kill_switch_engaged: false,
            sequencer: Some(seq),
            sequencer_grace_secs: crate::sequencer::GRACE_PERIOD_SECS,
            head: Some(100),
            last_head: Some(100),
            max_head_age: 5,
            providers_agree: true,
            float_balance_equiv: Some(1_000_000),
            float_cap: 20_000_000,
            gross_out: U256::from(60_000_000u64),
            amount_in: U256::from(50_000_000u64),
            costs,
            min_net: U256::from(5_000_000u64),
            safety_margin: U256::from(1_000_000u64),
            offchain_out: U256::from(60_000_000u64),
            onchain_out: U256::from(60_000_000u64),
            tolerance_bps: 50,
            risk,
            pair_name: "WETH/USDC",
            allowlisted: true,
            size_u64: 50_000_000,
            slippage_bps: 5,
            sim,
            gas_margin_bps: 2000,
            gas_cap: 1_000_000,
            max_fee: 1,
            fee_cap: 100,
            priority_fee: 1,
            priority_cap: 100,
            l1_fee: 20_000,
            l1_cap: 1_000_000,
            ledger,
        }
    }

    fn fixture() -> (RiskState, CostModel, SimulationResult) {
        let mut risk = RiskState::new(RiskLimits {
            max_flash: 50_000_000,
            min_net_profit: 1_000_000,
            ..RiskLimits::default()
        });
        risk.observe_head(100);
        let costs = CostModel {
            l2_gas_cost: U256::from(50_000u64),
            l1_data_fee: U256::from(20_000u64),
            priority_fee_cost: U256::from(5_000u64),
            slippage_bps: 5,
            token_fee_bps: 0,
            flash_fee_bps: 0,
            failed_attempt_allowance: U256::from(10_000u64),
        };
        let sim = SimulationResult {
            onchain_out: U256::from(60_000_000u64),
            offchain_out: U256::from(60_000_000u64),
            gas_estimate: 250_000,
            would_revert: false,
        };
        (risk, costs, sim)
    }

    #[test]
    fn approves_when_every_gate_passes() {
        let (risk, costs, sim) = fixture();
        let seq = sequencer_up();
        let mut ledger = FakeLedger {
            intents: 0,
            fail: false,
        };
        let approved = run_presubmit(passing_inputs(&risk, &mut ledger, &seq, &costs, &sim))
            .expect("all gates pass");
        assert_eq!(ledger.intents, 1, "intent persisted before approval");
        assert_eq!(approved.ledger_seq, 1);
        assert_eq!(approved.gas_limit, 300_000);
    }

    /// Each L5 step has a rejection test: mutate one input, assert the gate.
    /// One assertion block per gate (each names its gate, so a failure
    /// points at the broken step, not at a table row).
    #[test]
    fn each_gate_rejects() {
        let (risk, costs, sim) = fixture();
        let seq = sequencer_up();

        // Kill switch.
        {
            let mut ledger = FakeLedger {
                intents: 0,
                fail: false,
            };
            let mut i = passing_inputs(&risk, &mut ledger, &seq, &costs, &sim);
            i.kill_switch_engaged = true;
            assert_gate(i, "kill-switch");
        }
        // Sequencer unknown/down.
        {
            let mut ledger = FakeLedger {
                intents: 0,
                fail: false,
            };
            let mut i = passing_inputs(&risk, &mut ledger, &seq, &costs, &sim);
            i.sequencer = None;
            assert_gate(i, "sequencer");
        }
        // Stale head.
        {
            let mut ledger = FakeLedger {
                intents: 0,
                fail: false,
            };
            let mut i = passing_inputs(&risk, &mut ledger, &seq, &costs, &sim);
            i.head = Some(50);
            assert_gate(i, "freshness");
        }
        // Provider disagreement.
        {
            let mut ledger = FakeLedger {
                intents: 0,
                fail: false,
            };
            let mut i = passing_inputs(&risk, &mut ledger, &seq, &costs, &sim);
            i.providers_agree = false;
            assert_gate(i, "providers");
        }
        // Float cap: over and unknown (continuous check).
        {
            let mut ledger = FakeLedger {
                intents: 0,
                fail: false,
            };
            let mut i = passing_inputs(&risk, &mut ledger, &seq, &costs, &sim);
            i.float_balance_equiv = Some(20_000_001);
            assert_gate(i, "float-cap");
        }
        {
            let mut ledger = FakeLedger {
                intents: 0,
                fail: false,
            };
            let mut i = passing_inputs(&risk, &mut ledger, &seq, &costs, &sim);
            i.float_balance_equiv = None;
            assert_gate(i, "float-cap");
        }
        // Estimate below min + margin.
        {
            let mut ledger = FakeLedger {
                intents: 0,
                fail: false,
            };
            let mut i = passing_inputs(&risk, &mut ledger, &seq, &costs, &sim);
            i.gross_out = U256::from(51_000_000u64);
            i.offchain_out = U256::from(51_000_000u64);
            i.onchain_out = U256::from(51_000_000u64);
            let mut sim2 = sim.clone();
            sim2.onchain_out = U256::from(51_000_000u64);
            sim2.offchain_out = U256::from(51_000_000u64);
            i.sim = &sim2;
            assert_gate(i, "estimate");
        }
        // Quote diverged.
        {
            let mut ledger = FakeLedger {
                intents: 0,
                fail: false,
            };
            let mut i = passing_inputs(&risk, &mut ledger, &seq, &costs, &sim);
            i.onchain_out = U256::from(50_000_000u64);
            assert_gate(i, "quote");
        }
        // Risk halted.
        {
            let mut halted = RiskState::new(RiskLimits {
                max_flash: 50_000_000,
                min_net_profit: 1_000_000,
                ..RiskLimits::default()
            });
            halted.observe_head(100);
            halted.kill();
            let mut ledger = FakeLedger {
                intents: 0,
                fail: false,
            };
            let i = passing_inputs(&halted, &mut ledger, &seq, &costs, &sim);
            assert_gate(i, "risk");
        }
        // Simulation revert.
        {
            let mut sim_revert = sim.clone();
            sim_revert.would_revert = true;
            let mut ledger = FakeLedger {
                intents: 0,
                fail: false,
            };
            let mut i = passing_inputs(&risk, &mut ledger, &seq, &costs, &sim);
            i.sim = &sim_revert;
            assert_gate(i, "simulation");
        }
        // Gas over cap.
        {
            let mut sim_gas = sim.clone();
            sim_gas.gas_estimate = 900_000;
            let mut ledger = FakeLedger {
                intents: 0,
                fail: false,
            };
            let mut i = passing_inputs(&risk, &mut ledger, &seq, &costs, &sim);
            i.sim = &sim_gas;
            assert_gate(i, "gas-caps");
        }
        // Fee over cap.
        {
            let mut ledger = FakeLedger {
                intents: 0,
                fail: false,
            };
            let mut i = passing_inputs(&risk, &mut ledger, &seq, &costs, &sim);
            i.max_fee = 10_000;
            assert_gate(i, "gas-caps");
        }
        // L1 fee over cap.
        {
            let mut ledger = FakeLedger {
                intents: 0,
                fail: false,
            };
            let mut i = passing_inputs(&risk, &mut ledger, &seq, &costs, &sim);
            i.l1_fee = 99_000_000;
            assert_gate(i, "gas-caps");
        }
        // Ledger persist failure: no approval without a persisted intent.
        {
            let mut ledger = FakeLedger {
                intents: 0,
                fail: true,
            };
            let i = passing_inputs(&risk, &mut ledger, &seq, &costs, &sim);
            assert_gate(i, "ledger");
            assert_eq!(ledger.intents, 0);
        }
    }

    fn assert_gate(inputs: PresubmitInputs<'_>, gate: &str) {
        match run_presubmit(inputs) {
            Err(e) => assert_eq!(e.gate, gate, "wrong gate: {e}"),
            Ok(_) => panic!("expected rejection at gate {gate}"),
        }
    }

    #[test]
    fn sim_fail_means_no_submit_and_no_intent() {
        // L10: failed simulation => no submission. The pipeline rejects
        // before touching the ledger, so a sender gated on approval has
        // nothing to submit.
        let (risk, costs, sim) = fixture();
        let seq = sequencer_up();
        let mut sim_revert = sim.clone();
        sim_revert.would_revert = true;
        let mut ledger = FakeLedger {
            intents: 0,
            fail: false,
        };
        let mut i = passing_inputs(&risk, &mut ledger, &seq, &costs, &sim);
        i.sim = &sim_revert;
        assert!(run_presubmit(i).is_err());
        assert_eq!(ledger.intents, 0, "no intent persisted on sim failure");
    }
}
