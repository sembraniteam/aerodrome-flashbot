//! Concentrated-liquidity pool state and quoter bindings.
//!
//! Covers Aerodrome Slipstream (CL, identified by tick spacing, newer
//! versions possibly dynamic-fee) and Uniswap V3 (fee tiers
//! 100/500/3000/10000). Off-chain math here is integer-only (`U256`); it is
//! an *estimate* used for sizing and pre-filtering. The authoritative quote
//! is the on-chain QuoterV2 via `eth_call` (see [`crate::sim`]); any
//! mismatch is a bug until explained.

use alloy::primitives::{Address, U256};
use alloy::sol;

// ---- Contract bindings (verified Base addresses in `crate::config`) ----

sol! {
    /// Minimal CL pool surface shared by Slipstream and Uniswap V3.
    #[sol(rpc)]
    interface IClPool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function tickSpacing() external view returns (int24);
        /// Slipstream dynamic-fee pools expose the live fee; UniV3 static
        /// pools revert here (callers must fall back to the tier mapping).
        function fee() external view returns (uint24);
    }

    // Note: no shared QuoterV2 binding here on purpose. Aerodrome QuoterV2
    // keys pools by `tickSpacing` (`int24`) while Uniswap V3 QuoterV2 keys
    // them by `fee` (`uint24`), so a single 5-scalar `quoteExactInputSingle`
    // shape mismatches one venue's on-chain struct. Live quote paths use the
    // per-venue struct bindings in `crate::fork_check` (`IAeroQuoterV2` /
    // `IUniQuoterV2`), the only quoter calldata this codebase builds.
    /// Balancer V2 Vault flash-loan entry point (fee is 0% on the deployed
    /// Base Vault, but the code path keeps the fee term general).
    #[sol(rpc)]
    interface IBalancerVault {
        function flashLoan(
            address recipient,
            address[] memory tokens,
            uint256[] memory amounts,
            bytes memory userData
        ) external;
    }
}

/// Which DEX a pool belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolKind {
    Slipstream,
    UniV3,
}

/// Live CL pool snapshot used by the off-chain estimator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClPoolState {
    pub pool: Address,
    pub kind: PoolKind,
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
    /// Actual fee in hundredths of a bps (1e-6 units), read from `fee()` or
    /// the tier/tick-spacing map at simulation time.
    pub fee_ppm: u32,
    pub tick_spacing: i32,
}

impl ClPoolState {
    /// Fail-closed constructor: rejects zero liquidity / zero price.
    pub fn new(
        pool: Address,
        kind: PoolKind,
        sqrt_price_x96: U256,
        liquidity: u128,
        tick: i32,
        fee_ppm: u32,
        tick_spacing: i32,
    ) -> Option<Self> {
        if sqrt_price_x96.is_zero() || liquidity == 0 {
            return None;
        }
        Some(Self {
            pool,
            kind,
            sqrt_price_x96,
            liquidity,
            tick,
            fee_ppm,
            tick_spacing,
        })
    }
}

// ---- Fee schedules ----

/// Uniswap V3 static fee tiers on Base (ppm = parts per million).
pub const UNIV3_FEE_TIERS_PPM: [u32; 4] = [100, 500, 3000, 10_000];

/// Tick spacing -> static fee tier for Uniswap V3.
pub fn univ3_fee_for_tick_spacing(tick_spacing: i32) -> Option<u32> {
    match tick_spacing {
        1 => Some(100),
        10 => Some(500),
        60 => Some(3000),
        200 => Some(10_000),
        _ => None,
    }
}

/// Slipstream v1 static mapping by tick spacing. Newer Slipstream pools may
/// use **dynamic fees**: whenever `fee()` returns a value, it overrides this
/// map. Unknown spacings return `None` so callers fall back to the on-chain
/// `fee()` read instead of guessing.
///
/// MUST be verified against the deployed Slipstream factory/pool source:
/// Aerodrome has added finer spacings over time.
pub fn slipstream_fee_for_tick_spacing(tick_spacing: i32) -> Option<u32> {
    match tick_spacing {
        1 => Some(100),    // 0.01%
        50 => Some(500),   // 0.05%
        100 => Some(2000), // 0.20% (volatile)
        200 => Some(3000), // 0.30%
        _ => None,
    }
}

/// Resolve the effective fee: live `fee()` reading wins (dynamic pools),
/// otherwise the static map for the pool kind. `None` means "do not trade:
/// fee unknown".
pub fn resolve_fee(kind: PoolKind, tick_spacing: i32, live_fee: Option<u32>) -> Option<u32> {
    if let Some(f) = live_fee {
        return Some(f);
    }
    match kind {
        PoolKind::Slipstream => slipstream_fee_for_tick_spacing(tick_spacing),
        PoolKind::UniV3 => univ3_fee_for_tick_spacing(tick_spacing),
    }
}

// ---- Integer-only off-chain math ----

/// Apply a ppm fee to an amount: `amount * (1e6 - fee_ppm) / 1e6`, rounded
/// down (matches on-chain rounding direction: quoter output is a floor).
pub fn apply_fee(amount: U256, fee_ppm: u32) -> U256 {
    debug_assert!(fee_ppm <= 1_000_000);
    let complement = U256::from(1_000_000u64 - fee_ppm.min(1_000_000) as u64);
    (amount * complement) / U256::from(1_000_000u64)
}

/// Single-tick constant-product estimate of `amountOut` for `amountIn`.
///
/// Derivation: within one tick, `x * y = L^2` with price `P =
/// (sqrtPriceX96 / 2^96)^2` (token1 per token0). For token0 -> token1:
/// `out = amountIn * P`, for token1 -> token0: `out = amountIn / P`,
/// then the pool fee is applied. This ignores tick crossings and is only a
/// pre-filter; the quoter `eth_call` is authoritative.
///
/// The token0 -> token1 leg evaluates divided-first
/// (`((in * sqrt) / 2^96) * sqrt / 2^96`): the naive `in * sqrt * sqrt`
/// overflows `U256` (saturates to `MAX`, yielding a constant output) at
/// realistic 18dp/6dp price scales — fork-measured on AERO/USDC. The extra
/// intermediate flooring costs at most dust versus tolerance.
pub fn estimate_amount_out(state: &ClPoolState, amount_in: U256, zero_for_one: bool) -> U256 {
    if amount_in.is_zero() {
        return U256::ZERO;
    }
    // P = (sqrtPriceX96 / 2^96)^2 (token1 per token0) as a rational.
    let q96 = U256::from(1u128 << 96);
    let sqrt = state.sqrt_price_x96;
    let gross = if zero_for_one {
        // out1 = in0 * sqrt^2 / 2^192, divided-first to avoid U256 overflow.
        let step = amount_in.saturating_mul(sqrt) / q96;
        step.saturating_mul(sqrt) / q96
    } else {
        // out0 = in1 * 2^192 / sqrt^2
        amount_in.saturating_mul(q96).saturating_mul(q96)
            / sqrt.saturating_mul(sqrt).max(U256::from(1u8))
    };
    apply_fee(gross, state.fee_ppm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn weth_usdc_state() -> ClPoolState {
        // ~$3000/ETH: price 3000 USDC(6dp) per WETH(18dp) as token1/token0.
        // Encode sqrtPriceX96 = sqrt(3000 * 1e6 / 1e18 * 2^192)... instead use
        // a hand-computed fixture: sqrt(3000e-12) * 2^96.
        // sqrt(3e-9) ~= 5.4772255e-5; * 7.92281625e28 ~= 4.3395e24.
        let sqrt: U256 = "4339500000000000000000000".parse().unwrap();
        ClPoolState::new(
            "0x0000000000000000000000000000000000000001"
                .parse()
                .unwrap(),
            PoolKind::Slipstream,
            sqrt,
            1_000_000_000_000_000_000,
            100_000,
            500,
            50,
        )
        .unwrap()
    }

    #[test]
    fn fee_maps_cover_known_tiers() {
        assert_eq!(univ3_fee_for_tick_spacing(1), Some(100));
        assert_eq!(univ3_fee_for_tick_spacing(10), Some(500));
        assert_eq!(univ3_fee_for_tick_spacing(60), Some(3000));
        assert_eq!(univ3_fee_for_tick_spacing(200), Some(10_000));
        assert_eq!(univ3_fee_for_tick_spacing(7), None);
    }

    #[test]
    fn live_fee_overrides_static_map() {
        assert_eq!(resolve_fee(PoolKind::UniV3, 60, Some(1234)), Some(1234));
        assert_eq!(resolve_fee(PoolKind::UniV3, 60, None), Some(3000));
        assert_eq!(resolve_fee(PoolKind::Slipstream, 999, None), None);
    }

    #[test]
    fn constructor_rejects_empty_state() {
        assert!(
            ClPoolState::new(Address::ZERO, PoolKind::UniV3, U256::ZERO, 1, 0, 500, 10).is_none()
        );
        assert!(
            ClPoolState::new(
                Address::ZERO,
                PoolKind::UniV3,
                U256::from(1u8),
                0,
                0,
                500,
                10
            )
            .is_none()
        );
    }

    #[test]
    fn apply_fee_rounds_down_like_quoter() {
        // 1000 units, 0.05% fee -> 999.5 floored to 999.
        assert_eq!(apply_fee(U256::from(1000u64), 500), U256::from(999u64));
        assert_eq!(apply_fee(U256::from(1000u64), 0), U256::from(1000u64));
    }

    #[test]
    fn estimator_monotone_and_fee_ordered() {
        let base = weth_usdc_state();
        // USDC-side input (zero_for_one=false): $1 and $2 in 6dp units.
        // (WETH-side dust of a few base units correctly floors to zero.)
        assert_eq!(
            estimate_amount_out(&base, U256::from(1000u64), true),
            U256::ZERO
        );
        let small = estimate_amount_out(&base, U256::from(1_000_000u64), false);
        let big = estimate_amount_out(&base, U256::from(2_000_000u64), false);
        assert!(
            small > U256::ZERO,
            "fixture input must clear the dust floor"
        );
        assert!(big > small, "output must grow with input");
        // Single-tick model is linear up to integer flooring.
        assert_eq!(big / small, U256::from(2u8));
        // Higher fee -> lower output.
        let mut pricey = base.clone();
        pricey.fee_ppm = 3000;
        let out_pricey = estimate_amount_out(&pricey, U256::from(1_000_000u64), false);
        assert!(out_pricey < small);
    }

    #[test]
    fn estimator_zero_in_zero_out() {
        let s = weth_usdc_state();
        assert_eq!(estimate_amount_out(&s, U256::ZERO, true), U256::ZERO);
    }

    #[test]
    fn estimator_no_overflow_at_aero_scale() {
        // Regression (fork-measured on AERO/USDC): at 18dp/6dp price scales
        // the naive `in * sqrt * sqrt` saturated to `U256::MAX`, so every
        // size returned the same constant. Divided-first evaluation must stay
        // linear in the input.
        let sqrt: U256 = "89000000000000000000000000000000000".parse().unwrap();
        let s = ClPoolState::new(
            Address::ZERO,
            PoolKind::Slipstream,
            sqrt,
            u128::MAX / 2,
            0,
            500,
            50,
        )
        .unwrap();
        let small = estimate_amount_out(&s, U256::from(100_000_000u64), true);
        let big = estimate_amount_out(&s, U256::from(200_000_000u64), true);
        assert!(small > U256::ZERO, "must clear the dust floor");
        assert_eq!(
            big / small,
            U256::from(2u8),
            "must stay linear, not saturated (got {small} vs {big})"
        );
    }
}
