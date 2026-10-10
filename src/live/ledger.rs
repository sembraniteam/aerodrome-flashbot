//! L8 append-only ledger: the raw material of evidence.
//!
//! JSON Lines under the profile `ledger_dir` (git-ignored, S5): one record
//! per opportunity, intent, submission, receipt, reconciliation, breaker
//! event, and operator action. Each record carries `prev_hash` and
//! `hash = sha256(prev_hash || canonical_json)` so tampering is detectable.
//! A `ledger verify` subcommand recomputes the chain.
//!
//! The ledger holds no secrets or URLs: field names that would carry them
//! (`*_key`, `*_secret`, `*rpc_url*`, `*webhook*`) are refused, plus an
//! optional exact-value blocklist for test/fixture secrets.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use thiserror::Error;

use super::manifest::sha256_hex;
use crate::evidence::build_commit;

/// Ledger record kinds (one per pipeline event).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    Opportunity,
    Intent,
    Submission,
    Receipt,
    Reconcile,
    Breaker,
    OperatorAction,
}

/// One chained record. `hash` covers `prev_hash` plus the canonical body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LedgerRecord {
    pub seq: u64,
    pub kind: LedgerKind,
    pub prev_hash: String,
    pub hash: String,
    pub commit: String,
    pub body: Value,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LedgerError {
    #[error("ledger refuses field '{0}' (secret/URL-shaped name)")]
    ForbiddenField(String),
    #[error("ledger refuses blocklisted value in field '{0}'")]
    BlocklistedValue(String),
    #[error("ledger chain broken at index {0}")]
    BrokenChain(u64),
    #[error("ledger io: {0}")]
    Io(String),
}

/// Field-name fragments that must never enter the ledger.
const FORBIDDEN_FRAGMENTS: &[&str] = &["private_key", "secret", "rpc_url", "webhook"];

fn field_name_forbidden(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.contains("key") || FORBIDDEN_FRAGMENTS.iter().any(|f| lower.contains(f))
}

fn check_value(top_key: &str, value: &Value, blocklist: &[String]) -> Result<(), LedgerError> {
    match value {
        Value::String(s) => {
            if blocklist.iter().any(|b| !b.is_empty() && s.contains(b)) {
                return Err(LedgerError::BlocklistedValue(top_key.to_string()));
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                check_value(top_key, item, blocklist)?;
            }
            Ok(())
        }
        Value::Object(map) => {
            for (k, v) in map {
                if field_name_forbidden(k) {
                    return Err(LedgerError::ForbiddenField(format!("{top_key}.{k}")));
                }
                check_value(top_key, v, blocklist)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn check_body(body: &Value, blocklist: &[String]) -> Result<(), LedgerError> {
    // Bodies are flat today, but the scan is recursive so secrets nested
    // inside an object/array value cannot bypass the boundary.
    let obj = body.as_object().cloned().unwrap_or_default();
    for (key, value) in &obj {
        let lower = key.to_ascii_lowercase();
        // Owner/operator/pauser *addresses* are fine; key material is not.
        let is_role_address =
            (lower == "owner" || lower == "operator" || lower == "pauser") && value.is_string();
        if !is_role_address && field_name_forbidden(key) {
            return Err(LedgerError::ForbiddenField(key.clone()));
        }
        check_value(key, value, blocklist)?;
    }
    Ok(())
}

/// Canonical record hash: `sha256(prev_hash || canonical_json)`.
/// `serde_json::Value` objects sort keys (`BTreeMap`), so `to_string` is
/// canonical for the shapes we emit.
pub fn record_hash(prev_hash: &str, kind: LedgerKind, body: &Value, commit: &str) -> String {
    let kind_s = serde_json::to_string(&kind).unwrap_or_default();
    let body_s = serde_json::to_string(body).unwrap_or_default();
    let mut input =
        Vec::with_capacity(prev_hash.len() + kind_s.len() + body_s.len() + commit.len());
    input.extend_from_slice(prev_hash.as_bytes());
    input.extend_from_slice(kind_s.as_bytes());
    input.extend_from_slice(body_s.as_bytes());
    input.extend_from_slice(commit.as_bytes());
    sha256_hex(&input)
}

/// Append-only JSONL ledger.
///
/// `blocklist` holds exact secret values that must never be written (the
/// live binary passes [`super::signer::SecretBlocklist`] built from
/// `OPERATOR_KEY`; tests pass fixture secrets). The list itself is secret:
/// [`Debug`](std::fmt::Debug) is manual and prints the entry COUNT only,
/// and a refused append names the FIELD only, never the value.
pub struct Ledger {
    path: PathBuf,
    blocklist: Vec<String>,
    head_hash: String,
    next_seq: u64,
}

impl std::fmt::Debug for Ledger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ledger")
            .field("path", &self.path)
            .field("blocklist_entries", &self.blocklist.len())
            .field("head_hash", &self.head_hash)
            .field("next_seq", &self.next_seq)
            .finish()
    }
}

impl Ledger {
    /// Open (or create) the ledger at `ledger_dir/ledger.jsonl`. Resumes the
    /// chain from existing records so restarts extend, never fork, history.
    /// `blocklist` holds exact secret values that must never be written:
    /// production passes the operator [`super::signer::SecretBlocklist`]
    /// (both paste forms); the append boundary refuses any body whose string
    /// field contains one, naming the field only. Read-only `verify` flows
    /// pass an empty list (nothing is written).
    pub fn open(ledger_dir: &Path, blocklist: Vec<String>) -> Result<Self, LedgerError> {
        std::fs::create_dir_all(ledger_dir).map_err(|e| LedgerError::Io(e.to_string()))?;
        let path = ledger_dir.join("ledger.jsonl");
        let mut head_hash = "GENESIS".to_string();
        let mut next_seq = 0u64;
        if path.exists() {
            let records = read_records(&path)?;
            verify_records(&records)?;
            if let Some(last) = records.last() {
                head_hash = last.hash.clone();
                next_seq = last.seq + 1;
            }
        }
        Ok(Self {
            path,
            blocklist,
            head_hash,
            next_seq,
        })
    }

    /// Persist an intent/receipt/event BEFORE submission (L5 step 7).
    pub fn append(&mut self, kind: LedgerKind, body: Value) -> Result<LedgerRecord, LedgerError> {
        check_body(&body, &self.blocklist)?;
        let commit = build_commit();
        let hash = record_hash(&self.head_hash, kind, &body, &commit);
        let record = LedgerRecord {
            seq: self.next_seq,
            kind,
            prev_hash: self.head_hash.clone(),
            hash: hash.clone(),
            commit,
            body,
        };
        let mut line =
            serde_json::to_string(&record).map_err(|e| LedgerError::Io(e.to_string()))?;
        line.push('\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| {
                use std::io::Write as _;
                f.write_all(line.as_bytes())
            })
            .map_err(|e| LedgerError::Io(e.to_string()))?;
        self.head_hash = hash;
        self.next_seq += 1;
        Ok(record)
    }

    pub fn head_hash(&self) -> &str {
        &self.head_hash
    }

    pub fn len(&self) -> u64 {
        self.next_seq
    }

    pub fn is_empty(&self) -> bool {
        self.next_seq == 0
    }

    /// Re-verify the whole chain (the `ledger verify` subcommand).
    pub fn verify(&self) -> Result<u64, LedgerError> {
        verify_records(&read_records(&self.path)?).map(|n| n as u64)
    }
}

fn read_records(path: &Path) -> Result<Vec<LedgerRecord>, LedgerError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let body = std::fs::read_to_string(path).map_err(|e| LedgerError::Io(e.to_string()))?;
    let mut out = Vec::new();
    for (i, line) in body.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let record: LedgerRecord =
            serde_json::from_str(line).map_err(|_| LedgerError::BrokenChain(i as u64))?;
        out.push(record);
    }
    Ok(out)
}

/// Recompute every link. Returns the record count on success, else the
/// first broken index.
fn verify_records(records: &[LedgerRecord]) -> Result<usize, LedgerError> {
    let mut prev = "GENESIS".to_string();
    for (i, r) in records.iter().enumerate() {
        if r.seq != i as u64 || r.prev_hash != prev {
            return Err(LedgerError::BrokenChain(i as u64));
        }
        let recomputed = record_hash(&r.prev_hash, r.kind, &r.body, &r.commit);
        if recomputed != r.hash {
            return Err(LedgerError::BrokenChain(i as u64));
        }
        prev = r.hash.clone();
    }
    Ok(records.len())
}

/// Verify a ledger directory (subcommand entry point). Prints nothing;
/// returns the record count or the first broken index.
pub fn verify_ledger_dir(ledger_dir: &Path) -> Result<u64, LedgerError> {
    Ledger::open(ledger_dir, Vec::new())?.verify()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn appends_chain_and_verifies() {
        let dir = tmpdir("aero-ledger-test-ok");
        let mut l = Ledger::open(&dir, Vec::new()).expect("opens");
        // Tx-hash-shaped values (0x+64hex built at runtime) MUST be
        // writable: receipts need them; only secret field names are refused.
        let tx = format!("0x{}", "ab".repeat(32));
        l.append(LedgerKind::Intent, json!({"size": 50_000_000u64}))
            .expect("intent");
        l.append(
            LedgerKind::Receipt,
            json!({"tx": tx, "block": 42u64, "realized_net": 1_000_000i64}),
        )
        .expect("receipt");
        assert_eq!(l.len(), 2);
        assert_eq!(l.verify(), Ok(2));
        // Restart resumes the chain (no fork).
        let l2 = Ledger::open(&dir, Vec::new()).expect("reopens");
        assert_eq!(l2.head_hash(), l.head_hash());
        assert_eq!(l2.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tamper_detected_at_first_broken_index() {
        // L10: ledger hash chain detects tampering; `ledger verify` fails
        // on the edited record.
        let dir = tmpdir("aero-ledger-test-tamper");
        let mut l = Ledger::open(&dir, Vec::new()).expect("opens");
        l.append(LedgerKind::Intent, json!({"size": 1u64}))
            .expect("r0");
        l.append(LedgerKind::Intent, json!({"size": 2u64}))
            .expect("r1");
        l.append(LedgerKind::Intent, json!({"size": 3u64}))
            .expect("r2");
        assert_eq!(l.verify(), Ok(3));
        // Edit the middle record's body in place.
        let path = dir.join("ledger.jsonl");
        let body = std::fs::read_to_string(&path).expect("reads");
        let tampered = body.replacen("\"size\":2", "\"size\":200", 1);
        assert_ne!(body, tampered);
        std::fs::write(&path, tampered).expect("writes");
        assert_eq!(verify_ledger_dir(&dir), Err(LedgerError::BrokenChain(1)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refuses_secret_shaped_fields_and_blocklisted_values() {
        // L10: no secrets in logs/ledger/evidence.
        let dir = tmpdir("aero-ledger-test-secrets");
        let mut l =
            Ledger::open(&dir, vec!["test-only-fake-secret-001".to_string()]).expect("opens");
        assert_eq!(
            l.append(LedgerKind::Intent, json!({"operator_key": "abc"})),
            Err(LedgerError::ForbiddenField("operator_key".to_string()))
        );
        assert_eq!(
            l.append(
                LedgerKind::Intent,
                json!({"note": "leak test-only-fake-secret-001 here"})
            ),
            Err(LedgerError::BlocklistedValue("note".to_string()))
        );
        assert_eq!(
            l.append(LedgerKind::Intent, json!({"rpc_url": "wss://x"})),
            Err(LedgerError::ForbiddenField("rpc_url".to_string()))
        );
        // Role addresses are data, not secrets: writable.
        assert!(
            l.append(
                LedgerKind::Reconcile,
                json!({"operator": "0x3333333333333333333333333333333333333333"})
            )
            .is_ok()
        );
        assert_eq!(l.len(), 1, "refused records must not advance the chain");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn production_blocklist_refuses_operator_key_shaped_value() {
        // P4 append boundary: the production blocklist (operator key in both
        // paste forms, as `SecretBlocklist::from_values` builds it) refuses
        // the write even though the VALUE is tx-hash-shaped (0x + 64 hex),
        // which non-blocklisted receipts must still allow (see
        // `appends_chain_and_verifies`: tx hashes are data, not secrets --
        // the blocklist is what distinguishes them).
        use super::super::signer::SecretBlocklist;
        let raw_key = "be".repeat(32);
        let blocklist = SecretBlocklist::from_values(&[raw_key.as_str()]);
        let dir = tmpdir("aero-ledger-test-operator-boundary");
        let mut l = Ledger::open(&dir, blocklist.to_vec()).expect("opens");
        let leaked = format!("0x{raw_key}");
        assert_eq!(
            l.append(LedgerKind::Intent, json!({"note": leaked})),
            Err(LedgerError::BlocklistedValue("note".to_string()))
        );
        // Refusal names the field only, never the value.
        let err = format!("{}", LedgerError::BlocklistedValue("note".to_string()));
        assert!(!err.contains(&raw_key), "value echoed in error: {err}");
        assert_eq!(l.len(), 0, "refused records must not advance the chain");
        // Ledger Debug never carries the blocklisted value.
        let dbg = format!("{l:?}");
        assert!(!dbg.contains(&raw_key), "secret leaked in Debug: {dbg}");
        assert!(dbg.contains("blocklist_entries"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
