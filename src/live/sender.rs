//! L6 sender and reconciliation: one nonce owner, terminal-state tracking.
//!
//! - [`NonceManager`]: single owner of the transaction count; recovery after
//!   dropped/replaced transactions. No blind retries: a retry re-runs the L5
//!   pipeline first (enforced by move semantics -- `submit` consumes a fresh
//!   [`super::pipeline::ApprovedIntent`).
//! - Every transaction is tracked to a terminal state and booked only after
//!   the profile `confirmation_depth`.
//! - [`reconcile`] compares ledger totals against on-chain balances after
//!   each transaction (and at least daily); any mismatch trips the breaker.
//!
//! Provider-backed submission lands in P4; [`OnchainExecutor`] carries the
//! preflight checks (kill switch, breaker, caps) that P3 wires and tests.

use alloy::primitives::Address;
use thiserror::Error;

use super::breaker::{BreakerError, TripReason};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SenderError {
    #[error("nonce {0} already in flight (max_in_flight = 1)")]
    AlreadyInFlight(u64),
    #[error("unknown nonce {0}")]
    UnknownNonce(u64),
    #[error("submission refused: {0}")]
    Refused(String),
}

/// Single-owner nonce counter. `take` fails while a transaction is in
/// flight (`max_in_flight = 1`); dropped/replaced transactions release the
/// nonce for reuse instead of skipping it (gap recovery).
#[derive(Debug, Default)]
pub struct NonceManager {
    next: u64,
    in_flight: Option<u64>,
}

impl NonceManager {
    pub fn new(start: u64) -> Self {
        Self {
            next: start,
            in_flight: None,
        }
    }

    /// Take the next nonce for submission. Fails while one is in flight.
    pub fn take(&mut self) -> Result<u64, SenderError> {
        if let Some(n) = self.in_flight {
            return Err(SenderError::AlreadyInFlight(n));
        }
        let n = self.next;
        self.in_flight = Some(n);
        Ok(n)
    }

    /// Terminal confirmation: the nonce is spent, the counter advances.
    pub fn confirm(&mut self, nonce: u64) -> Result<(), SenderError> {
        if self.in_flight != Some(nonce) {
            return Err(SenderError::UnknownNonce(nonce));
        }
        self.in_flight = None;
        self.next = self.next.saturating_add(1);
        Ok(())
    }

    /// Dropped or replaced: the nonce is free again WITHOUT advancing, so a
    /// re-run of L5 can reuse it (recovery, not a gap).
    pub fn release(&mut self, nonce: u64) -> Result<(), SenderError> {
        if self.in_flight != Some(nonce) {
            return Err(SenderError::UnknownNonce(nonce));
        }
        self.in_flight = None;
        Ok(())
    }

    pub fn in_flight(&self) -> Option<u64> {
        self.in_flight
    }

    pub fn next(&self) -> u64 {
        self.next
    }
}

/// Terminal states for a tracked transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxState {
    Submitted,
    Confirmed { confirmations: u64 },
    Reverted,
    Dropped,
}

impl TxState {
    pub fn is_terminal(self) -> bool {
        matches!(self, TxState::Reverted | TxState::Dropped)
            || matches!(self, TxState::Confirmed { .. })
    }

    /// Bookable only after `confirmation_depth` (L6): reorg safety.
    pub fn bookable(self, confirmation_depth: u64) -> bool {
        match self {
            TxState::Confirmed { confirmations } => confirmations >= confirmation_depth,
            TxState::Reverted => true,
            TxState::Submitted | TxState::Dropped => false,
        }
    }
}

/// Reconcile ledger totals against on-chain balances (executor + operator).
/// `expected` is the ledger-booked total, `observed` the summed on-chain
/// delta; `|expected - observed| <= tolerance` passes, else the caller trips
/// [`TripReason::ReconcileMismatch`]. Integer math only.
pub fn reconcile(expected: i128, observed: i128, tolerance: i128) -> Result<(), TripReason> {
    let diff = expected.saturating_sub(observed).abs();
    if diff <= tolerance {
        Ok(())
    } else {
        Err(TripReason::ReconcileMismatch)
    }
}

/// Non-terminal intents found at startup must resolve to a terminal state
/// before any new trade is allowed (L6). Returns the count still pending.
pub fn pending_intents_must_be_zero(pending: u64) -> Result<(), SenderError> {
    if pending == 0 {
        Ok(())
    } else {
        Err(SenderError::Refused(format!(
            "{pending} non-terminal intent(s): reconcile before new trades"
        )))
    }
}

/// On-chain executor handle (P3): address + chain binding with the preflight
/// checks the sender enforces before every submit. The provider-backed
/// `submit` lands in P4; dry-run can never construct this type (L10 wiring
/// test asserts the paper tree never names it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnchainExecutor {
    pub executor: Address,
    pub chain_id: u64,
}

impl OnchainExecutor {
    /// Preflight: kill switch disengaged AND breaker untripped AND nonce
    /// free. Any failure refuses (fail closed); the caller maps the breaker
    /// error to a trip where applicable.
    pub fn preflight(
        &self,
        kill_switch_engaged: bool,
        breaker: &Result<(), BreakerError>,
        nonces: &mut NonceManager,
    ) -> Result<u64, SenderError> {
        if kill_switch_engaged {
            return Err(SenderError::Refused("kill switch engaged".to_string()));
        }
        if let Err(e) = breaker {
            return Err(SenderError::Refused(format!("breaker: {e}")));
        }
        nonces.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonce_sequences_and_recovers() {
        // L10: sequencing, gaps, replacement, recovery.
        let mut n = NonceManager::new(7);
        assert_eq!(n.take(), Ok(7));
        assert_eq!(n.take(), Err(SenderError::AlreadyInFlight(7)));
        n.confirm(7).expect("confirms");
        assert_eq!(n.next(), 8);
        assert_eq!(n.take(), Ok(8));
        n.release(8).expect("dropped: released without advancing");
        assert_eq!(n.next(), 8, "recovery reuses the nonce (no gap)");
        assert_eq!(n.take(), Ok(8));
        n.confirm(8).expect("confirms");
        assert_eq!(n.next(), 9);
    }

    #[test]
    fn unknown_nonce_is_an_error() {
        let mut n = NonceManager::new(0);
        assert_eq!(n.confirm(3), Err(SenderError::UnknownNonce(3)));
        assert_eq!(n.release(3), Err(SenderError::UnknownNonce(3)));
    }

    #[test]
    fn booking_waits_for_confirmation_depth() {
        // L10: booked only after confirmation_depth.
        assert!(!TxState::Submitted.bookable(1));
        assert!(!TxState::Dropped.bookable(1));
        assert!(!TxState::Confirmed { confirmations: 2 }.bookable(5));
        assert!(TxState::Confirmed { confirmations: 5 }.bookable(5));
        assert!(TxState::Reverted.bookable(5));
        assert!(!TxState::Submitted.is_terminal());
        assert!(TxState::Reverted.is_terminal());
        assert!(TxState::Dropped.is_terminal());
    }

    #[test]
    fn reconcile_mismatch_trips() {
        // L10: reconciliation mismatch trips the breaker.
        assert!(reconcile(100, 100, 0).is_ok());
        assert!(reconcile(100, 105, 10).is_ok());
        assert_eq!(reconcile(100, 200, 10), Err(TripReason::ReconcileMismatch));
    }

    #[test]
    fn startup_refuses_with_pending_intents() {
        assert!(pending_intents_must_be_zero(0).is_ok());
        assert!(pending_intents_must_be_zero(2).is_err());
    }

    #[test]
    fn preflight_refuses_on_kill_breaker_or_busy_nonce() {
        let exec = OnchainExecutor {
            executor: Address::repeat_byte(0x11),
            chain_id: 8453,
        };
        let ok: Result<(), BreakerError> = Ok(());
        let mut n = NonceManager::new(0);
        assert!(exec.preflight(true, &ok, &mut n).is_err(), "kill refuses");
        let tripped: Result<(), BreakerError> = Err(BreakerError::Tripped("x"));
        assert!(
            exec.preflight(false, &tripped, &mut n).is_err(),
            "breaker refuses"
        );
        assert!(exec.preflight(false, &ok, &mut n).is_ok());
        assert!(
            exec.preflight(false, &ok, &mut n).is_err(),
            "second take while in flight refuses"
        );
    }
}
