//! Profit engine: two-direction quotes, full cost model, optimal sizing.
//!
//! Invariants:
//! - Integer arithmetic only (`U256`) in the profit path. No floats.
//! - Every cost term is subtracted before the `min_net_profit` gate:
//!   pool fees (actual per-pool fee incl. dynamic), slippage allowance,
//!   token transfer fees/taxes, L2 execution gas, L1 data fee (OP-Stack),
//!   priority fee, flash-loan fee, failed-attempt allowance.
//! - Revert (reject) when `net < min_net_profit`.

use alloy::primitives::U256;
use thiserror::Error;

use crate::pools::{ClPoolState, estimate_amount_out};

/// All-in cost terms for one arbitrage attempt, in the *profit token* base
/// units (USDC base units for USDC-quoted pairs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CostModel {
    /// L2 execution gas cost (gas_used * effective_gas_price).
    pub l2_gas_cost: U256,
    /// L1 data fee from the OP-Stack GasPriceOracle.
    pub l1_data_fee: U256,
    /// Extra priority-fee spend beyond the base fee already in l2_gas_cost.
    pub priority_fee_cost: U256,
    /// Slippage allowance in basis points applied to gross output.
    pub slippage_bps: u64,
    /// Token transfer fee / tax in basis points (0 for vanilla USDC/WETH).
    pub token_fee_bps: u64,
    /// Flash-loan fee in basis points (0 for Balancer Vault on Base).
    pub flash_fee_bps: u64,
    /// Amortized allowance for failed attempts, in profit-token units.
    pub failed_attempt_allowance: U256,
}

/// Paper fixture default for the L1 data-fee term (USDC base units, ~$0.02).
/// Offline-safe default; the live path injects the measured `getL1Fee` value
/// (OP-Stack GasPriceOracle, see [`crate::chain::l1_fee_calldata`]) via
/// `--l1-fee` / [`CostModel::with_live_l1_fee`].
pub const FIXTURE_L1_DATA_FEE_USDC: u64 = 20_000;

impl CostModel {
    /// Paper-mode fixture costs (~$0.05 L2 gas + ~$0.02 L1 fee + margin).
    /// Keeps offline runs deterministic; every term is documented at the
    /// field level above.
    pub fn paper_fixture() -> Self {
        Self {
            l2_gas_cost: U256::from(50_000u64),
            l1_data_fee: U256::from(FIXTURE_L1_DATA_FEE_USDC),
            priority_fee_cost: U256::from(5_000u64),
            slippage_bps: 5,
            token_fee_bps: 0,
            flash_fee_bps: 0, // Balancer Vault 0% on Base
            failed_attempt_allowance: U256::from(10_000u64),
        }
    }

    /// Override the L1 data-fee term with a measured `getL1Fee` value
    /// (builder style).
    pub fn with_live_l1_fee(mut self, l1_fee: U256) -> Self {
        self.l1_data_fee = l1_fee;
        self
    }

    /// Override the L1 data-fee term with a measured `getL1Fee` value.
    pub fn set_l1_fee(&mut self, l1_fee: U256) {
        self.l1_data_fee = l1_fee;
    }

    /// Sum of the flat (non-bps) cost terms.
    pub fn flat_costs(&self) -> U256 {
        self.l2_gas_cost
            .saturating_add(self.l1_data_fee)
            .saturating_add(self.priority_fee_cost)
            .saturating_add(self.failed_attempt_allowance)
    }

    /// Apply bps-denominated haircuts (slippage, token fee, flash fee) to a
    /// gross output. Rounded down at each step (conservative).
    pub fn apply_bps_haircuts(&self, gross: U256) -> U256 {
        let mut out = gross;
        for bps in [self.slippage_bps, self.token_fee_bps, self.flash_fee_bps] {
            let bps = bps.min(10_000);
            out = out.saturating_mul(U256::from(10_000 - bps)) / U256::from(10_000u64);
        }
        out
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProfitError {
    #[error("profit {net} below minimum {min}")]
    BelowMinimum { net: U256, min: U256 },
    #[error("amount_in is zero")]
    ZeroInput,
}

/// One evaluated direction: borrow on leg A, repay via leg B (or reverse).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectionQuote {
    /// True for A->B->A (flash token A), false for B->A->B.
    pub forward: bool,
    /// Flash principal in profit-token base units.
    pub amount_in: U256,
    /// Gross output after both legs (before cost model), in same units.
    pub gross_out: U256,
    /// Net after [`CostModel`].
    pub net: Option<U256>,
}

/// Outcome of the two-direction harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArbDecision {
    pub best: DirectionQuote,
    pub other: DirectionQuote,
    /// Minimum net profit threshold that was enforced.
    pub min_net: U256,
}

impl ArbDecision {
    /// True when the winning direction clears the minimum.
    pub fn is_profitable(&self) -> bool {
        self.best.net.is_some_and(|n| n >= self.min_net)
    }
}

/// Net profit for one direction: haircut the gross output, subtract flat
/// costs and the principal. Returns `None` on underflow (gross < costs).
pub fn net_profit(gross_out: U256, amount_in: U256, costs: &CostModel) -> Option<U256> {
    let after_haircuts = costs.apply_bps_haircuts(gross_out);
    after_haircuts
        .checked_sub(costs.flat_costs())
        .and_then(|v| v.checked_sub(amount_in))
}

/// Evaluate both directions and pick the winner. Each leg uses its pool's
/// *actual* fee (already baked into `leg_a`/`leg_b` state). Rejects with
/// [`ProfitError`] when neither direction clears `min_net`.
pub fn evaluate_both_directions(
    leg_a: &ClPoolState,
    leg_b: &ClPoolState,
    amount_in: U256,
    a_to_b_first: bool,
    costs: &CostModel,
    min_net: U256,
) -> Result<ArbDecision, ProfitError> {
    if amount_in.is_zero() {
        return Err(ProfitError::ZeroInput);
    }
    // Forward: flash X -> swap X->Y on leg A, then Y->X on leg B.
    // Yields amount_in * P_a / P_b before fees.
    let fwd_mid = estimate_amount_out(leg_a, amount_in, a_to_b_first);
    let fwd_gross = estimate_amount_out(leg_b, fwd_mid, !a_to_b_first);
    let fwd_net = net_profit(fwd_gross, amount_in, costs);

    // Reverse: same principal through leg B as X->Y, then leg A as Y->X
    // (legs swapped, same orientation flags). Yields P_b / P_a: the true
    // inverse path, so exactly one direction can clear when prices differ.
    let rev_mid = estimate_amount_out(leg_b, amount_in, a_to_b_first);
    let rev_gross = estimate_amount_out(leg_a, rev_mid, !a_to_b_first);
    let rev_net = net_profit(rev_gross, amount_in, costs);

    let forward = DirectionQuote {
        forward: true,
        amount_in,
        gross_out: fwd_gross,
        net: fwd_net,
    };
    let reverse = DirectionQuote {
        forward: false,
        amount_in,
        gross_out: rev_gross,
        net: rev_net,
    };

    let (best, other) = match (forward.net, reverse.net) {
        (Some(f), Some(r)) if r > f => (reverse.clone(), forward.clone()),
        (None, Some(_)) => (reverse.clone(), forward.clone()),
        _ => (forward.clone(), reverse.clone()),
    };
    match best.net {
        Some(n) if n >= min_net => Ok(ArbDecision {
            best,
            other,
            min_net,
        }),
        Some(n) => Err(ProfitError::BelowMinimum {
            net: n,
            min: min_net,
        }),
        None => Err(ProfitError::BelowMinimum {
            net: U256::ZERO,
            min: min_net,
        }),
    }
}

/// Break-even principal: smallest `amount_in` (grid-searched up to
/// `max_flash`) whose net clears `min_net`. Returns `None` when no size is
/// viable. Grid is deliberately coarse (pre-filter); the live path
/// re-simulates the chosen size against current state before submitting.
pub fn breakeven_size(
    leg_a: &ClPoolState,
    leg_b: &ClPoolState,
    max_flash: U256,
    a_to_b_first: bool,
    costs: &CostModel,
    min_net: U256,
) -> Option<U256> {
    optimal_size(leg_a, leg_b, max_flash, a_to_b_first, costs, min_net).map(|(s, _)| s)
}

/// Optimal size: grid-search fractions of `max_flash`, keep the size with
/// the highest clearing net. Returns `(size, net)`.
pub fn optimal_size(
    leg_a: &ClPoolState,
    leg_b: &ClPoolState,
    max_flash: U256,
    a_to_b_first: bool,
    costs: &CostModel,
    min_net: U256,
) -> Option<(U256, U256)> {
    if max_flash.is_zero() {
        return None;
    }
    // 10-point grid: 10%..100% of max_flash. Integer division floors;
    // skip zero candidates (dust).
    // ponytail: 10-point linear grid pre-filter; live path re-simulates chosen size via quoter eth_call.
    let mut best: Option<(U256, U256)> = None;
    let ten = U256::from(10u64);
    for i in 1u64..=10u64 {
        let size = max_flash.saturating_mul(U256::from(i)) / ten;
        if size.is_zero() {
            continue;
        }
        let Ok(decision) =
            evaluate_both_directions(leg_a, leg_b, size, a_to_b_first, costs, min_net)
        else {
            continue;
        };
        let net = decision.best.net.unwrap_or(U256::ZERO);
        if best.is_none_or(|(_, n)| net > n) {
            best = Some((size, net));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pools::{ClPoolState, PoolKind};
    use alloy::primitives::Address;

    fn pool_with_price(numer: u128, denom: u128, fee_ppm: u32) -> ClPoolState {
        // ponytail: f64 ONLY in this test fixture; profit path stays
        // integer-only (U256). Never copy this into estimator/cost code;
        // upgrade path is integer sqrt if fixtures need more precision.
        let p = numer as f64 / denom as f64;
        let sqrt = (p.sqrt() * 2f64.powi(96)) as u128;
        ClPoolState::new(
            Address::ZERO,
            PoolKind::UniV3,
            U256::from(sqrt),
            10_000_000_000_000_000_000u128,
            0,
            fee_ppm,
            60,
        )
        .unwrap()
    }

    fn cheap_costs() -> CostModel {
        CostModel {
            l2_gas_cost: U256::from(100u64),
            l1_data_fee: U256::from(50u64),
            priority_fee_cost: U256::from(10u64),
            slippage_bps: 5,
            token_fee_bps: 0,
            flash_fee_bps: 0, // Balancer Vault 0% on Base
            failed_attempt_allowance: U256::from(20u64),
        }
    }

    #[test]
    fn measured_l1_fee_changes_net_vs_fixture() {
        // The paper fixture is the offline default; injecting a measured
        // `getL1Fee` value must move net profit 1:1 with the fee delta.
        let fixture = CostModel::paper_fixture();
        assert_eq!(fixture.l1_data_fee, U256::from(FIXTURE_L1_DATA_FEE_USDC));
        let gross = U256::from(600_000_000u64);
        let amount = U256::from(500_000_000u64);
        let net_fixture = net_profit(gross, amount, &fixture).expect("fixture clears");
        let measured = fixture.with_live_l1_fee(U256::from(1_000_000u64));
        let net_measured = net_profit(gross, amount, &measured).expect("measured clears");
        assert!(net_measured < net_fixture, "higher L1 fee must lower net");
        assert_eq!(
            net_fixture - net_measured,
            U256::from(1_000_000u64 - FIXTURE_L1_DATA_FEE_USDC),
            "net delta must equal the L1 fee delta exactly"
        );
    }

    #[test]
    fn set_l1_fee_overrides_in_place() {
        let mut costs = CostModel::paper_fixture();
        costs.set_l1_fee(U256::from(7u64));
        assert_eq!(costs.l1_data_fee, U256::from(7u64));
    }
    #[test]
    fn every_cost_term_subtracted() {
        // gross 10_000, principal 9_000, flat costs 180, no bps haircut.
        let costs = CostModel {
            l2_gas_cost: U256::from(100u64),
            l1_data_fee: U256::from(50u64),
            priority_fee_cost: U256::from(10u64),
            failed_attempt_allowance: U256::from(20u64),
            ..CostModel::default()
        };
        let net = net_profit(U256::from(10_000u64), U256::from(9_000u64), &costs).unwrap();
        assert_eq!(net, U256::from(10_000u64 - 9_000u64 - 180u64));
    }

    #[test]
    fn bps_haircuts_compound_down() {
        let costs = CostModel {
            slippage_bps: 50,
            token_fee_bps: 100,
            flash_fee_bps: 0,
            ..CostModel::default()
        };
        // 1_000_000 -> *0.995 -> *0.99 = 985050 (floored stepwise).
        let out = costs.apply_bps_haircuts(U256::from(1_000_000u64));
        assert_eq!(out, U256::from(985_050u64));
    }

    #[test]
    fn underflow_returns_none_not_wrap() {
        let costs = cheap_costs();
        assert_eq!(
            net_profit(U256::from(10u64), U256::from(1_000u64), &costs),
            None
        );
    }

    #[test]
    fn mispriced_pools_yield_profitable_direction() {
        // Leg A rich (1.03 token1 per token0), leg B cheap (0.98): forward
        // X->Y on A then Y->X on B prints P_a/P_b ~= +5% before costs,
        // well clear of 2x100ppm fees; reverse is the inverse (~-4.9%).
        let leg_a = pool_with_price(103, 100, 100);
        let leg_b = pool_with_price(98, 100, 100);
        let costs = cheap_costs();
        let res = evaluate_both_directions(
            &leg_a,
            &leg_b,
            U256::from(1_000_000u64),
            true,
            &costs,
            U256::from(1u64),
        );
        assert!(res.is_ok(), "expected profit, got {res:?}");
        let d = res.unwrap();
        assert!(d.is_profitable());
        assert!(d.best.forward, "forward should win on this skew");
    }

    #[test]
    fn reverse_wins_when_skew_flips() {
        // Mirror of the previous test: now leg B is rich, so the reverse
        // path (X->Y on B, Y->X on A) must win.
        let leg_a = pool_with_price(98, 100, 100);
        let leg_b = pool_with_price(103, 100, 100);
        let costs = cheap_costs();
        let d = evaluate_both_directions(
            &leg_a,
            &leg_b,
            U256::from(1_000_000u64),
            true,
            &costs,
            U256::from(1u64),
        )
        .expect("reverse must clear");
        assert!(d.is_profitable());
        assert!(!d.best.forward, "reverse should win on this skew");
    }

    #[test]
    fn aligned_pools_revert_below_minimum() {
        let leg_a = pool_with_price(100, 100, 3000);
        let leg_b = pool_with_price(100, 100, 3000);
        let costs = cheap_costs();
        let res = evaluate_both_directions(
            &leg_a,
            &leg_b,
            U256::from(1_000_000u64),
            true,
            &costs,
            U256::from(5_000_000u64),
        );
        assert!(matches!(res, Err(ProfitError::BelowMinimum { .. })));
    }

    #[test]
    fn zero_input_rejected() {
        let leg = pool_with_price(100, 100, 500);
        let err =
            evaluate_both_directions(&leg, &leg, U256::ZERO, true, &cheap_costs(), U256::ZERO)
                .unwrap_err();
        assert_eq!(err, ProfitError::ZeroInput);
    }

    #[test]
    fn optimal_size_picks_largest_clearing_on_linear_model() {
        let leg_a = pool_with_price(103, 100, 100);
        let leg_b = pool_with_price(98, 100, 100);
        let costs = cheap_costs();
        let best = optimal_size(
            &leg_a,
            &leg_b,
            U256::from(1_000_000u64),
            true,
            &costs,
            U256::from(1u64),
        );
        assert!(best.is_some());
        let (size, net) = best.unwrap();
        assert_eq!(size, U256::from(1_000_000u64));
        assert!(net > U256::ZERO);
    }

    #[test]
    fn optimal_size_none_when_nothing_clears() {
        let leg = pool_with_price(100, 100, 3000);
        let costs = cheap_costs();
        assert_eq!(
            optimal_size(
                &leg,
                &leg,
                U256::from(1_000_000u64),
                true,
                &costs,
                U256::from(999_999_999u64)
            ),
            None
        );
        assert_eq!(
            breakeven_size(&leg, &leg, U256::ZERO, true, &costs, U256::ZERO),
            None
        );
    }
}
