//! L6 sender and reconciliation: one nonce owner, terminal-state tracking.
//!
//! - [`NonceManager`]: single owner of the transaction count; recovery after
//!   dropped/replaced transactions. No blind retries: a retry re-runs the L5
//!   pipeline first (enforced by move semantics -- [`submit`] consumes a
//!   fresh [`super::pipeline::ApprovedIntent`]; the compiler rejects reuse).
//! - Every transaction is tracked to a terminal state and booked only after
//!   the profile `confirmation_depth` ([`book_receipt`] enforces
//!   [`TxState::bookable`]; unconfirmed states refuse).
//! - [`reconcile_and_trip`] compares ledger totals against on-chain balances
//!   after each transaction AND at least daily (same helper, two call sites:
//!   per-tx totals vs day-aggregated totals); any mismatch trips the breaker
//!   and returns the [`TripReason`] (callers stop, never proceed).
//!
//! P4 scope: authorization + nonce binding + booking/reconcile state machine
//! only (all offline). Provider-backed broadcast lands behind the same L1
//! lock later; this module performs no network I/O.

use alloy::primitives::Address;
use thiserror::Error;

use super::breaker::{Breaker, BreakerError, TripReason};
use super::pipeline::ApprovedIntent;

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

/// A submitted transaction awaiting a terminal state: the nonce it owns plus
/// the ledger intent row (L5 step 7) it settles. Produced ONLY by
/// [`OnchainExecutor::submit`], which consumes the [`ApprovedIntent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingTx {
    pub nonce: u64,
    pub ledger_seq: u64,
    pub gas_limit: u64,
}

/// Ledger booking authorization for a settled transaction. Produced ONLY by
/// [`book_receipt`] after the [`TxState::bookable`] gate passes; the caller
/// appends the receipt row carrying this `ledger_seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiptBooking {
    pub nonce: u64,
    pub ledger_seq: u64,
}

/// Book a receipt ONLY after [`TxState::bookable`]: confirmed with
/// `confirmations >= confirmation_depth`, or reverted (its gas cost is final
/// and books immediately). Submitted/dropped/under-confirmed states refuse
/// (fail closed against reorgs and premature profit claims).
pub fn book_receipt(
    tx: &PendingTx,
    state: TxState,
    confirmation_depth: u64,
) -> Result<ReceiptBooking, SenderError> {
    if state.bookable(confirmation_depth) {
        Ok(ReceiptBooking {
            nonce: tx.nonce,
            ledger_seq: tx.ledger_seq,
        })
    } else {
        Err(SenderError::Refused(format!(
            "receipt not bookable: {state:?} below confirmation_depth {confirmation_depth}"
        )))
    }
}

/// Reconcile ledger totals against on-chain balances and trip the breaker on
/// mismatch. Call sites (L6): after EACH transaction (per-tx totals) and at
/// least DAILY (day-aggregated totals). `Ok` = within tolerance, proceed;
/// `Err(reason)` = breaker tripped with that reason, STOP (callers never
/// proceed on `Err`). Integer math only (see [`reconcile`]).
///
/// Note: when the trip PERSIST fails (disk fault), the in-memory breaker is
/// still tripped (fail closed for this process); the next restart loads
/// fail-closed on the unreadable state file. Either way no new trade is
/// allowed after an `Err` return.
pub fn reconcile_and_trip(
    breaker: &mut Breaker,
    expected: i128,
    observed: i128,
    tolerance: i128,
) -> Result<(), TripReason> {
    match reconcile(expected, observed, tolerance) {
        Ok(()) => Ok(()),
        Err(reason) => {
            // Persist best-effort: the return value already stops the caller.
            let _ = breaker.trip(reason);
            Err(reason)
        }
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

    /// Authorize a submission: consumes a FRESH [`ApprovedIntent`] (move, not
    /// copy -- a retry needs a new L5 pipeline run; the compiler enforces it)
    /// and binds it to a nonce behind the preflight gate. Refusals take no
    /// nonce and persist nothing.
    ///
    /// P4 scope: authorization + nonce binding only (offline). The
    /// provider-backed broadcast re-checks these exact gates at send time
    /// behind the L1 lock; there is no path that submits without an intent.
    pub fn submit(
        &self,
        intent: ApprovedIntent,
        kill_switch_engaged: bool,
        breaker: &Result<(), BreakerError>,
        nonces: &mut NonceManager,
    ) -> Result<PendingTx, SenderError> {
        let nonce = self.preflight(kill_switch_engaged, breaker, nonces)?;
        Ok(PendingTx {
            nonce,
            ledger_seq: intent.ledger_seq,
            gas_limit: intent.gas_limit,
        })
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

    fn test_executor() -> OnchainExecutor {
        OnchainExecutor {
            executor: Address::repeat_byte(0x11),
            chain_id: 84532,
        }
    }

    fn test_intent(ledger_seq: u64) -> ApprovedIntent {
        ApprovedIntent {
            size: alloy::primitives::U256::from(5_000_000u64),
            net: alloy::primitives::U256::from(1_000_000u64),
            gas_limit: 300_000,
            ledger_seq,
        }
    }

    #[test]
    fn submit_consumes_fresh_intent_and_binds_nonce() {
        // P4 move-enforced path: each submit takes a fresh intent by value
        // (retry = new L5 run; reuse does not compile -- the caller no
        // longer owns the value after this call).
        let exec = test_executor();
        let ok: Result<(), BreakerError> = Ok(());
        let mut n = NonceManager::new(4);
        let pending = exec
            .submit(test_intent(7), false, &ok, &mut n)
            .expect("submit binds");
        assert_eq!(pending.nonce, 4);
        assert_eq!(pending.ledger_seq, 7);
        assert_eq!(pending.gas_limit, 300_000);
        assert_eq!(n.in_flight(), Some(4), "nonce held while in flight");
    }

    #[test]
    fn submit_refusal_takes_no_nonce_or_ledger_row() {
        // Every refusal path leaves the nonce free (fail closed, no partial
        // state): kill, breaker, and busy-nonce each refuse.
        let exec = test_executor();
        let ok: Result<(), BreakerError> = Ok(());
        let tripped: Result<(), BreakerError> = Err(BreakerError::Tripped("x"));
        for (kill, breaker) in [(true, &ok), (false, &tripped)] {
            let mut n = NonceManager::new(9);
            assert!(exec.submit(test_intent(1), kill, breaker, &mut n).is_err());
            assert_eq!(n.in_flight(), None, "no nonce on refusal");
            assert_eq!(n.next(), 9, "counter unmoved on refusal");
        }
        // Busy nonce: first submit holds it, second (fresh intent) refuses.
        let mut n = NonceManager::new(9);
        exec.submit(test_intent(1), false, &ok, &mut n)
            .expect("first binds");
        assert!(exec.submit(test_intent(2), false, &ok, &mut n).is_err());
        // Recovery: dropped tx releases the nonce; a FRESH intent (new L5
        // run, never the consumed one) re-binds the same nonce (no gap).
        n.release(9).expect("dropped releases");
        let retry = exec
            .submit(test_intent(3), false, &ok, &mut n)
            .expect("retry with fresh intent binds");
        assert_eq!(retry.nonce, 9);
        assert_eq!(retry.ledger_seq, 3, "fresh intent row, not the old one");
    }

    #[test]
    fn booking_waits_for_confirmation_depth_per_state() {
        // P4 bookable() gating: only terminal-confirmed-at-depth (or
        // reverted, whose cost is final) produce a booking.
        let tx = PendingTx {
            nonce: 4,
            ledger_seq: 7,
            gas_limit: 300_000,
        };
        assert!(book_receipt(&tx, TxState::Submitted, 1).is_err());
        assert!(book_receipt(&tx, TxState::Dropped, 1).is_err());
        assert!(
            book_receipt(&tx, TxState::Confirmed { confirmations: 2 }, 5).is_err(),
            "under-depth refuses (reorg safety)"
        );
        let booked =
            book_receipt(&tx, TxState::Confirmed { confirmations: 5 }, 5).expect("at depth books");
        assert_eq!(
            booked,
            ReceiptBooking {
                nonce: 4,
                ledger_seq: 7
            }
        );
        assert!(
            book_receipt(&tx, TxState::Reverted, 5).is_ok(),
            "revert books"
        );
    }

    #[test]
    fn reconcile_and_trip_stops_and_trips_on_mismatch() {
        // P4: per-tx shape (small totals) and daily shape (aggregated
        // totals) share the helper; mismatch trips the persisted breaker.
        use super::super::breaker::Breaker;
        let dir = std::env::temp_dir().join("aero-sender-test-reconcile");
        let _ = std::fs::remove_dir_all(&dir);
        // Per-tx: exact pass.
        let mut b = Breaker::load(&dir).expect("fresh");
        assert!(reconcile_and_trip(&mut b, 1_000_000, 1_000_000, 0).is_ok());
        assert!(b.check().is_ok(), "pass leaves breaker clear");
        // Per-tx: mismatch trips with the reason returned to the caller.
        assert_eq!(
            reconcile_and_trip(&mut b, 1_000_000, 2_000_000, 10),
            Err(TripReason::ReconcileMismatch)
        );
        assert!(b.check().is_err(), "mismatch trips");
        // Trip persists: a restart stays stopped (no new trade allowed).
        let b2 = Breaker::load(&dir).expect("reloads");
        assert_eq!(b2.is_tripped(), Some(TripReason::ReconcileMismatch));
        let _ = std::fs::remove_dir_all(&dir);
        // Daily shape: aggregated totals within tolerance pass.
        let dir2 = std::env::temp_dir().join("aero-sender-test-reconcile-daily");
        let _ = std::fs::remove_dir_all(&dir2);
        let mut b3 = Breaker::load(&dir2).expect("fresh");
        assert!(reconcile_and_trip(&mut b3, 50_000_000, 49_999_500, 1_000).is_ok());
        assert!(b3.check().is_ok());
        let _ = std::fs::remove_dir_all(&dir2);
    }
}
