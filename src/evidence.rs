//! Evidence emission (`--emit-evidence`): run-summary JSON for the auditor.
//!
//! Keyless and network-free: counts, rejection reasons, predicted-vs-realized
//! nets, breaker events, and the ledger head hash. Contains no keys, no URLs,
//! no signing or sending code (S1-clean by construction).
//!
//! Honesty rule (L9): summaries state sample sizes and never present
//! predicted numbers as realized. When no realized samples exist, the
//! realized fields are zero AND the note says so explicitly.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Schema version for the emitted summary.
pub const EVIDENCE_SCHEMA: u32 = 1;

/// Commit this binary was built from (`BUILD_COMMIT` from build.rs,
/// `"unknown"` when git was unavailable at compile time).
pub fn build_commit() -> String {
    option_env!("BUILD_COMMIT").unwrap_or("unknown").to_string()
}

/// One `--emit-evidence` run summary. `predicted_*` come from the estimator /
/// simulator; `realized_*` come only from settled on-chain receipts (or
/// fork-measured fills explicitly labelled as such by the caller).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceSummary {
    /// Schema version ([`EVIDENCE_SCHEMA`]).
    pub schema: u32,
    /// Which binary produced this (`"paper"` or `"live"`).
    pub binary: String,
    /// Build commit (never a prediction of chain state).
    pub commit: String,
    /// Chain id the run was configured for.
    pub chain_id: u64,
    /// UTC unix seconds the summary was written.
    pub written_at: u64,
    /// Candidates seen.
    pub opportunities_seen: u64,
    /// Candidates that reached simulation.
    pub attempts_simulated: u64,
    /// Simulations that cleared (profitable on paper).
    pub attempts_won: u64,
    /// Rejections per gate name (fail-closed reasons).
    pub rejections: BTreeMap<String, u64>,
    /// Sum of predicted nets (profit-token base units, signed).
    pub predicted_net_sum: i128,
    /// Number of predictions behind `predicted_net_sum`.
    pub predicted_samples: u64,
    /// Sum of realized nets (settled receipts only).
    pub realized_net_sum: i128,
    /// Number of settled fills behind `realized_net_sum`.
    pub realized_samples: u64,
    /// Breaker/kill-switch events observed this run.
    pub breaker_events: Vec<String>,
    /// Ledger head hash when a ledger was in use, else `None`.
    pub ledger_head: Option<String>,
    /// Human note; MUST state when realized data is absent.
    pub note: String,
}

impl EvidenceSummary {
    /// Paper-mode summary: predictions only, never realized.
    pub fn paper(
        chain_id: u64,
        seen: u64,
        simulated: u64,
        won: u64,
        rejections: BTreeMap<String, u64>,
        predicted_net_sum: i128,
        predicted_samples: u64,
    ) -> Self {
        Self {
            schema: EVIDENCE_SCHEMA,
            binary: "paper".to_string(),
            commit: build_commit(),
            chain_id,
            written_at: now_utc_secs(),
            opportunities_seen: seen,
            attempts_simulated: simulated,
            attempts_won: won,
            rejections,
            predicted_net_sum,
            predicted_samples,
            realized_net_sum: 0,
            realized_samples: 0,
            breaker_events: Vec::new(),
            ledger_head: None,
            note: "paper dry-run: predicted values only; no realized fills (realized_samples=0)"
                .to_string(),
        }
    }

    /// True when this summary carries no realized fills.
    pub fn has_no_realized(&self) -> bool {
        self.realized_samples == 0
    }
}

/// Current UTC unix seconds (wall clock; evidence timestamp only, never a
/// consensus or profit input).
pub fn now_utc_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write `summary.json` into `dir` (created when missing). Returns the file
/// path. Never edits existing evidence; fails when the target exists... no:
/// overwrites only the file it owns (`summary.json`) in the caller-chosen
/// directory, which must be empty or already hold this run's output.
pub fn write_evidence(dir: &Path, summary: &EvidenceSummary) -> anyhow::Result<PathBuf> {
    // Honesty gate: a summary that claims realized fills without samples, or
    // a predicted sum without samples, is a bug -- refuse to emit it.
    anyhow::ensure!(
        summary.realized_samples > 0 || summary.realized_net_sum == 0,
        "evidence refuses: realized_net_sum != 0 with realized_samples == 0"
    );
    anyhow::ensure!(
        summary.predicted_samples > 0 || summary.predicted_net_sum == 0,
        "evidence refuses: predicted_net_sum != 0 with predicted_samples == 0"
    );
    std::fs::create_dir_all(dir)?;
    let path = dir.join("summary.json");
    let body = serde_json::to_string_pretty(summary)?;
    std::fs::write(&path, body)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paper_summary_states_no_realized() {
        let s = EvidenceSummary::paper(8453, 3, 2, 1, BTreeMap::new(), 100, 1);
        assert!(s.has_no_realized());
        assert_eq!(s.realized_net_sum, 0);
        assert!(s.note.contains("realized_samples=0"), "note: {}", s.note);
        assert_eq!(s.binary, "paper");
        assert_eq!(s.schema, EVIDENCE_SCHEMA);
    }

    #[test]
    fn refuses_predicted_as_realized() {
        let dir = std::env::temp_dir().join("aero-evidence-test-refuse");
        let mut s = EvidenceSummary::paper(8453, 0, 0, 0, BTreeMap::new(), 0, 0);
        s.realized_net_sum = 999; // lie: fills without samples
        assert!(write_evidence(&dir, &s).is_err());
        let mut s2 = EvidenceSummary::paper(8453, 0, 0, 0, BTreeMap::new(), 0, 0);
        s2.predicted_net_sum = 5; // lie: predictions without samples
        assert!(write_evidence(&dir, &s2).is_err());
    }

    #[test]
    fn roundtrip_writes_summary_json() {
        let dir = std::env::temp_dir().join("aero-evidence-test-ok");
        let _ = std::fs::remove_dir_all(&dir);
        let mut rej = BTreeMap::new();
        rej.insert("risk".to_string(), 2);
        let s = EvidenceSummary::paper(84532, 3, 1, 0, rej, 0, 0);
        let path = write_evidence(&dir, &s).expect("write works");
        assert!(path.ends_with("summary.json"));
        let back: EvidenceSummary =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
                .expect("parses");
        assert_eq!(back, s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
