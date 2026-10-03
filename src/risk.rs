//! Risk controls: hard limits, circuit breaker, kill switch.
//!
//! Non-negotiable defaults: dry-run, max size per trade, max loss per trade,
//! daily loss cap, max in-flight, allowlist gate, min net-profit gate.
//! Fail closed: any uncertainty (stale data, halted state) rejects the trade.

use thiserror::Error;

/// Hard ceiling for the flash principal per trade (profit-token base units,
/// $500 in USDC units). The control plane can never raise limits above this;
/// [`crate::config::BotConfig::validate`] enforces it on config load and
/// [`RiskState::check_trade`] enforces `size <= max_flash <= hard_max_flash`
/// on every candidate.
pub const HARD_MAX_FLASH_USDC: u64 = 500_000_000;

#[derive(Debug, Clone)]
pub struct RiskLimits {
    /// Max flash principal per trade (profit-token base units).
    pub max_flash: u64,
    /// Max acceptable loss on a single trade (base units).
    pub max_loss_per_trade: u64,
    /// Daily net-loss cap (base units); breaching halts trading.
    pub daily_loss_cap: u64,
    /// Max simultaneous in-flight transactions.
    pub max_in_flight: u32,
    /// Minimum net profit (base units) after ALL costs.
    pub min_net_profit: u64,
    /// Max slippage in bps.
    pub max_slippage_bps: u64,
    /// Consecutive failures before the circuit breaker trips.
    pub max_consecutive_failures: u32,
    /// Hard ceiling: control plane can never raise limits above these.
    pub hard_max_flash: u64,
}

impl Default for RiskLimits {
    fn default() -> Self {
        Self {
            max_flash: 500_000_000,         // $500
            max_loss_per_trade: 10_000_000, // $10
            daily_loss_cap: 100_000_000,    // $100
            max_in_flight: 1,
            min_net_profit: 5_000_000, // $5
            max_slippage_bps: 50,
            max_consecutive_failures: 3,
            hard_max_flash: HARD_MAX_FLASH_USDC,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RiskError {
    #[error("killed: manual kill switch engaged")]
    Killed,
    #[error("halted: circuit breaker tripped")]
    Halted,
    #[error("pair '{0}' not allowlisted")]
    NotAllowlisted(String),
    #[error("size {0} exceeds max_flash {1}")]
    Oversize(u64, u64),
    #[error("misconfigured limits: max_flash {0} exceeds hard_max_flash {1}")]
    MaxExceedsHard(u64, u64),
    #[error("net {0} below min_net_profit {1}")]
    BelowMinProfit(u64, u64),
    #[error("slippage {0}bps exceeds max {1}bps")]
    SlippageTooHigh(u64, u64),
    #[error("in-flight {0} at max {1}")]
    TooManyInFlight(u32, u32),
    #[error("daily loss cap breached")]
    DailyCapBreached,
    #[error("stale data: refuse to trade")]
    StaleData,
}

/// Mutable risk state: PnL accounting, failure streak, halt flags.
#[derive(Debug, Default)]
pub struct RiskState {
    pub limits: RiskLimits,
    /// Cumulative net PnL today (signed, base units; negative = loss).
    pub daily_net: i128,
    pub consecutive_failures: u32,
    pub consecutive_reverts: u32,
    pub in_flight: u32,
    pub halted: bool,
    pub killed: bool,
    /// Last observed head number, for staleness checks.
    pub last_head: Option<u64>,
}

impl RiskState {
    pub fn new(limits: RiskLimits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }

    /// Manual kill switch: works even if everything else hangs (plain bool,
    /// no async, no locks beyond the caller's).
    pub fn kill(&mut self) {
        self.killed = true;
        self.halted = true;
    }

    /// Gate a candidate trade. Fails closed on every uncertainty.
    #[allow(clippy::too_many_arguments)]
    pub fn check_trade(
        &self,
        pair_name: &str,
        allowlisted: bool,
        size: u64,
        net: Option<u64>,
        slippage_bps: u64,
        head: Option<u64>,
        max_head_age: u64,
    ) -> Result<(), RiskError> {
        if self.killed {
            return Err(RiskError::Killed);
        }
        if self.halted {
            return Err(RiskError::Halted);
        }
        if !allowlisted {
            return Err(RiskError::NotAllowlisted(pair_name.to_string()));
        }
        // Hard-cap ladder: the configured max must never exceed the hard
        // ceiling (misconfiguration fails closed), and the size must clear
        // both rungs: size <= max_flash <= hard_max_flash.
        if self.limits.max_flash > self.limits.hard_max_flash {
            return Err(RiskError::MaxExceedsHard(
                self.limits.max_flash,
                self.limits.hard_max_flash,
            ));
        }
        if size > self.limits.max_flash {
            return Err(RiskError::Oversize(size, self.limits.max_flash));
        }
        if size > self.limits.hard_max_flash {
            return Err(RiskError::Oversize(size, self.limits.hard_max_flash));
        }
        match net {
            Some(n) if n >= self.limits.min_net_profit => {}
            Some(n) => return Err(RiskError::BelowMinProfit(n, self.limits.min_net_profit)),
            None => return Err(RiskError::BelowMinProfit(0, self.limits.min_net_profit)),
        }
        if slippage_bps > self.limits.max_slippage_bps {
            return Err(RiskError::SlippageTooHigh(
                slippage_bps,
                self.limits.max_slippage_bps,
            ));
        }
        if self.in_flight >= self.limits.max_in_flight {
            return Err(RiskError::TooManyInFlight(
                self.in_flight,
                self.limits.max_in_flight,
            ));
        }
        if self.daily_net <= -(self.limits.daily_loss_cap as i128) {
            return Err(RiskError::DailyCapBreached);
        }
        // Staleness: no head at all, or head older than tolerated.
        match (head, self.last_head) {
            (Some(h), Some(last)) if h.saturating_add(max_head_age) >= last => {}
            (Some(_), Some(_)) => return Err(RiskError::StaleData),
            (None, _) => return Err(RiskError::StaleData),
            (Some(h), None) => {
                // First observation: accept but the caller should record it.
                let _ = h;
            }
        }
        Ok(())
    }

    /// Record a finished attempt. Wins reset the failure streak; losses and
    /// reverts increment it and may trip the breaker or the daily cap.
    pub fn record_result(&mut self, net: i128, reverted: bool) {
        self.daily_net = self.daily_net.saturating_add(net);
        if reverted {
            self.consecutive_reverts += 1;
            self.consecutive_failures += 1;
        } else if net < 0 {
            self.consecutive_failures += 1;
        } else {
            self.consecutive_failures = 0;
            self.consecutive_reverts = 0;
        }
        if self.consecutive_failures >= self.limits.max_consecutive_failures {
            self.halted = true;
        }
        if self.daily_net <= -(self.limits.daily_loss_cap as i128) {
            self.halted = true;
        }
    }

    pub fn track_submit(&mut self) {
        self.in_flight = self.in_flight.saturating_add(1);
    }

    pub fn track_settle(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    pub fn observe_head(&mut self, head: u64) {
        self.last_head = Some(head);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> RiskState {
        let mut s = RiskState::new(RiskLimits::default());
        s.observe_head(100);
        s
    }

    #[test]
    fn happy_path_passes() {
        let s = state();
        assert!(
            s.check_trade(
                "WETH/USDC",
                true,
                100_000_000,
                Some(6_000_000),
                10,
                Some(100),
                5
            )
            .is_ok()
        );
    }

    #[test]
    fn kill_switch_overrides_everything() {
        let mut s = state();
        s.kill();
        assert_eq!(
            s.check_trade("WETH/USDC", true, 1, Some(99_999_999), 0, Some(100), 5),
            Err(RiskError::Killed)
        );
    }

    #[test]
    fn rejects_non_allowlisted() {
        let s = state();
        assert_eq!(
            s.check_trade("SCAM/ETH", false, 1, Some(9_999_999), 0, Some(100), 5),
            Err(RiskError::NotAllowlisted("SCAM/ETH".to_string()))
        );
    }

    #[test]
    fn rejects_oversize_and_thin_profit() {
        let s = state();
        assert!(matches!(
            s.check_trade(
                "WETH/USDC",
                true,
                999_999_999,
                Some(9_999_999),
                0,
                Some(100),
                5
            ),
            Err(RiskError::Oversize(..))
        ));
        assert!(matches!(
            s.check_trade("WETH/USDC", true, 1, Some(1), 0, Some(100), 5),
            Err(RiskError::BelowMinProfit(..))
        ));
        assert!(matches!(
            s.check_trade("WETH/USDC", true, 1, None, 0, Some(100), 5),
            Err(RiskError::BelowMinProfit(..))
        ));
    }

    #[test]
    fn rejects_stale_data() {
        let s = state();
        assert_eq!(
            s.check_trade("WETH/USDC", true, 1, Some(9_999_999), 0, None, 5),
            Err(RiskError::StaleData)
        );
        assert_eq!(
            s.check_trade("WETH/USDC", true, 1, Some(9_999_999), 0, Some(50), 5),
            Err(RiskError::StaleData)
        );
    }

    #[test]
    fn breaker_trips_after_streak_then_halts() {
        let mut s = state();
        for _ in 0..3 {
            s.record_result(-1_000_000, true);
        }
        assert!(s.halted);
        assert_eq!(
            s.check_trade("WETH/USDC", true, 1, Some(9_999_999), 0, Some(100), 5),
            Err(RiskError::Halted)
        );
    }

    #[test]
    fn win_resets_streak() {
        let mut s = state();
        s.record_result(-1, true);
        s.record_result(-1, true);
        s.record_result(10_000_000, false);
        assert_eq!(s.consecutive_failures, 0);
        assert!(!s.halted);
    }

    #[test]
    fn daily_cap_halts() {
        let mut s = state();
        s.record_result(-200_000_000, false);
        assert!(s.halted);
        assert_eq!(
            s.check_trade("WETH/USDC", true, 1, Some(9_999_999), 0, Some(100), 5),
            Err(RiskError::Halted)
        );
    }

    #[test]
    fn hard_cap_binds_size_even_at_consistent_max() {
        // Consistent ladder (max == hard): a size above the hard ceiling is
        // still Oversize.
        let s = state();
        assert_eq!(s.limits.max_flash, s.limits.hard_max_flash);
        assert!(matches!(
            s.check_trade(
                "WETH/USDC",
                true,
                s.limits.hard_max_flash + 1,
                Some(99_999_999),
                0,
                Some(100),
                5
            ),
            Err(RiskError::Oversize(..))
        ));
    }

    #[test]
    fn max_above_hard_is_misconfiguration() {
        // max_flash > hard_max_flash must fail closed for every trade, even
        // dust with huge profit.
        let mut limits = RiskLimits::default();
        limits.max_flash = limits.hard_max_flash + 1;
        let s = RiskState::new(limits);
        assert_eq!(
            s.check_trade("WETH/USDC", true, 1, Some(99_999_999), 0, Some(1), 5),
            Err(RiskError::MaxExceedsHard(
                HARD_MAX_FLASH_USDC + 1,
                HARD_MAX_FLASH_USDC
            ))
        );
    }

    #[test]
    fn in_flight_bounded() {
        let mut s = state();
        s.track_submit();
        assert_eq!(
            s.check_trade("WETH/USDC", true, 1, Some(9_999_999), 0, Some(100), 5),
            Err(RiskError::TooManyInFlight(1, 1))
        );
        s.track_settle();
        assert!(
            s.check_trade("WETH/USDC", true, 1, Some(9_999_999), 0, Some(100), 5)
                .is_ok()
        );
    }
}
