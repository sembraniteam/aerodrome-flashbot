//! L7 circuit breaker and kill switch.
//!
//! The breaker trips on: daily loss cap, `max_consecutive_failures`,
//! realized-vs-predicted drift beyond band, reconciliation mismatch,
//! codehash drift, sequencer/provider faults beyond policy. State persists
//! across restarts (ledger-backed file); reset is manual, owner-only, with
//! a reason written to the ledger.
//!
//! The kill switch is a file flag AND the on-chain pause, both checked
//! before every submit. An unreadable flag file means stopped.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Every condition that trips the breaker (persisted by name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TripReason {
    DailyLossCap,
    ConsecutiveFailures,
    DriftBeyondBand,
    ReconcileMismatch,
    CodehashDrift,
    SequencerFault,
    ProviderFault,
    FloatCapBreach,
    ManualKill,
    CorruptState,
}

impl TripReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            TripReason::DailyLossCap => "daily-loss-cap",
            TripReason::ConsecutiveFailures => "consecutive-failures",
            TripReason::DriftBeyondBand => "drift-beyond-band",
            TripReason::ReconcileMismatch => "reconcile-mismatch",
            TripReason::CodehashDrift => "codehash-drift",
            TripReason::SequencerFault => "sequencer-fault",
            TripReason::ProviderFault => "provider-fault",
            TripReason::FloatCapBreach => "float-cap-breach",
            TripReason::ManualKill => "manual-kill",
            TripReason::CorruptState => "corrupt-state",
        }
    }
}

/// Role allowed to reset the breaker: the owner only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetRole {
    Owner,
    Other,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BreakerError {
    #[error("breaker tripped ({0}); new submissions refused")]
    Tripped(&'static str),
    #[error("breaker reset refused: owner only")]
    ResetNotOwner,
    #[error("breaker state unreadable: fail closed ({0})")]
    Unreadable(String),
}

/// Content a kill-switch file must hold to ALLOW submission. Anything else
/// (missing, unreadable, other content) means stopped.
pub const KILL_SWITCH_ARMED_CONTENT: &str = "OK";

/// Kill-switch file check: `false` = stopped. Unreadable means stopped.
pub fn kill_switch_allows(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|c| c.trim() == KILL_SWITCH_ARMED_CONTENT)
        .unwrap_or(false)
}

/// Realized-vs-predicted drift in bps: `|predicted - realized| * 10000 /
/// predicted`. `None` predicted (zero) with nonzero realized is maximal
/// drift (fail closed); both zero is no drift. Integer math only.
pub fn drift_bps(predicted: u64, realized: i128) -> Option<u64> {
    if predicted == 0 {
        return if realized == 0 { Some(0) } else { None };
    }
    let diff = (predicted as i128 - realized).unsigned_abs();
    Some(((diff * 10_000) / predicted as u128).min(u64::MAX as u128) as u64)
}

/// Persisted breaker snapshot (JSON under the ledger dir).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct BreakerSnapshot {
    tripped: Option<TripReason>,
    commit: String,
}

/// Circuit breaker: trips once, stays tripped across restarts, resets only
/// by the owner with a ledger-recorded reason.
#[derive(Debug)]
pub struct Breaker {
    state_path: PathBuf,
    tripped: Option<TripReason>,
}

impl Breaker {
    fn path_for(ledger_dir: &Path) -> PathBuf {
        ledger_dir.join("breaker.json")
    }

    /// Load persisted state. Missing file = fresh (never tripped);
    /// unreadable/corrupt file = tripped (fail closed on ambiguity).
    pub fn load(ledger_dir: &Path) -> Result<Self, BreakerError> {
        let path = Self::path_for(ledger_dir);
        let tripped = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(BreakerError::Unreadable(e.to_string())),
            Ok(body) => match serde_json::from_str::<BreakerSnapshot>(&body) {
                Ok(snap) => snap.tripped,
                Err(_) => Some(TripReason::CorruptState),
            },
        };
        Ok(Self {
            state_path: path,
            tripped,
        })
    }

    pub fn is_tripped(&self) -> Option<TripReason> {
        self.tripped
    }

    /// Refuse new work while tripped.
    pub fn check(&self) -> Result<(), BreakerError> {
        match self.tripped {
            Some(r) => Err(BreakerError::Tripped(r.as_str())),
            None => Ok(()),
        }
    }

    /// Trip the breaker and persist immediately (first trip wins; the
    /// original reason is kept so restarts cannot launder it).
    pub fn trip(&mut self, reason: TripReason) -> Result<(), BreakerError> {
        if self.tripped.is_none() {
            self.tripped = Some(reason);
            self.persist()?;
        }
        Ok(())
    }

    /// Owner-only manual reset. Returns the reason to write to the ledger;
    /// the caller persists the ledger event (reset without a ledger reason
    /// is a bug, so the reason is returned, not logged-and-dropped).
    pub fn reset(&mut self, role: ResetRole, reason: &str) -> Result<String, BreakerError> {
        if role != ResetRole::Owner {
            return Err(BreakerError::ResetNotOwner);
        }
        let was = self.tripped.map(|r| r.as_str()).unwrap_or("untripped");
        self.tripped = None;
        self.persist()?;
        Ok(format!("breaker reset by owner (was {was}): {reason}"))
    }

    fn persist(&self) -> Result<(), BreakerError> {
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| BreakerError::Unreadable(e.to_string()))?;
        }
        let snap = BreakerSnapshot {
            tripped: self.tripped,
            commit: crate::evidence::build_commit(),
        };
        let body = serde_json::to_string_pretty(&snap)
            .map_err(|e| BreakerError::Unreadable(e.to_string()))?;
        std::fs::write(&self.state_path, body)
            .map_err(|e| BreakerError::Unreadable(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("tmpdir");
        d
    }

    #[test]
    fn trips_and_persists_across_restarts() {
        // L10: breaker trips and persists; first trip wins.
        let dir = tmpdir("aero-breaker-test-persist");
        let mut b = Breaker::load(&dir).expect("fresh");
        assert!(b.check().is_ok());
        b.trip(TripReason::ReconcileMismatch).expect("trips");
        assert_eq!(b.check(), Err(BreakerError::Tripped("reconcile-mismatch")));
        b.trip(TripReason::DailyLossCap)
            .expect("second trip is a no-op");
        let b2 = Breaker::load(&dir).expect("reloads");
        assert_eq!(b2.is_tripped(), Some(TripReason::ReconcileMismatch));
        assert!(b2.check().is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_state_fails_closed() {
        let dir = tmpdir("aero-breaker-test-corrupt");
        std::fs::write(dir.join("breaker.json"), "{not json").expect("write");
        let b = Breaker::load(&dir).expect("loads as tripped");
        assert_eq!(b.is_tripped(), Some(TripReason::CorruptState));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reset_is_owner_only_with_reason() {
        // L10: ledger-persisted owner-only reset.
        let dir = tmpdir("aero-breaker-test-reset");
        let mut b = Breaker::load(&dir).expect("fresh");
        b.trip(TripReason::ManualKill).expect("trips");
        assert_eq!(
            b.reset(ResetRole::Other, "mallory"),
            Err(BreakerError::ResetNotOwner)
        );
        assert!(b.check().is_err(), "non-owner reset must not clear");
        let event = b
            .reset(ResetRole::Owner, "drill complete")
            .expect("owner resets");
        assert!(event.contains("drill complete"));
        assert!(event.contains("manual-kill"));
        assert!(b.check().is_ok());
        // Reset persists: a restart stays untripped.
        let b2 = Breaker::load(&dir).expect("reloads");
        assert!(b2.check().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kill_switch_unreadable_means_stopped() {
        // L10: unreadable flag file means stopped.
        let dir = tmpdir("aero-breaker-test-kill");
        let missing = dir.join("no-such-dir").join("flag");
        assert!(!kill_switch_allows(&missing));
        let flag = dir.join("flag");
        std::fs::write(&flag, "ANYTHING").expect("write");
        assert!(!kill_switch_allows(&flag));
        std::fs::write(&flag, "OK\n").expect("write");
        assert!(kill_switch_allows(&flag));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn drift_band_math_is_integer() {
        assert_eq!(drift_bps(10_000, 10_000), Some(0));
        assert_eq!(drift_bps(10_000, 9_000), Some(1_000));
        assert_eq!(drift_bps(10_000, 11_000), Some(1_000));
        assert_eq!(drift_bps(0, 0), Some(0));
        assert_eq!(drift_bps(0, 5), None);
    }
}
