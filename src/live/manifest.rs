//! L1 manifest + arm lock: the readiness manifest the live binary trusts
//! only after re-verification.
//!
//! The manifest (see the evidence schema) names a `commit`, `chain_id`,
//! `stage_ready`, `attempt_stage`, `waived`, `expires_at`, and the hashes of
//! `docs/FREEZE.md`, the profile file, and `Cargo.lock`. The lock recomputes
//! all three hashes and compares them -- it reads the manifest, it does not
//! trust it blindly.
//!
//! Attempt-authorization vs completion: the lock arms an *attempt* of the
//! profile's required stage from honestly completed state (`stage_ready`,
//! strictly below the attempt) plus explicit per-rung `waived` entries with
//! reasons. It never accepts a manifest that claims the required stage as
//! already complete. Manifests written before `attempt_stage`/`waived`
//! existed fail closed at parse (no serde defaults on those fields).
//!
//! `LIVE_ARM` must equal the SHA-256 of the exact manifest bytes, compared
//! in constant time; logs record match/mismatch only, never values.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;

use super::profile::{LiveProfile, stage_label, stage_order};

/// Hashes the manifest pins (recomputed at startup, never trusted blindly).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestHashes {
    pub freeze_md: String,
    pub profile: String,
    pub cargo_lock: String,
}

/// Explicit, auditable skip of one readiness rung between `stage_ready`
/// and `attempt_stage`. `stage` is a `G0`..=`G6` label; `reason` is a
/// non-empty human justification (blank/whitespace-only refuses).
/// Coverage is set-based: duplicate entries for the same rung are
/// tolerated, but any entry outside the open interval
/// (`stage_ready`, `attempt_stage`) refuses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Waiver {
    pub stage: String,
    pub reason: String,
}

/// Readiness manifest subset the L1 lock enforces.
///
/// No serde defaults: a manifest written before `attempt_stage`/`waived`
/// existed fails to parse (fail closed). `LIVE_ARM` (SHA-256 of the exact
/// manifest bytes) automatically covers the new fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadinessManifest {
    pub commit: String,
    pub chain_id: u64,
    pub stage_ready: String,
    pub attempt_stage: String,
    pub waived: Vec<Waiver>,
    pub expires_at: String,
    pub hashes: ManifestHashes,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("manifest commit does not match the embedded build commit")]
    CommitMismatch,
    #[error("manifest chain_id {0} != profile chain_id {1}")]
    ProfileChainMismatch(u64, u64),
    #[error("manifest chain_id {0} != rpc chain_id {1}")]
    RpcChainMismatch(u64, u64),
    #[error("manifest attempt_stage '{0}' != profile stage_required '{1}'")]
    AttemptMismatch(String, String),
    #[error(
        "manifest stage_ready '{0}' is not strictly below attempt_stage '{1}' (the attempt must be uncompleted work)"
    )]
    ReadyNotBelowAttempt(String, String),
    #[error("missing waiver for stage '{0}' between stage_ready and attempt_stage")]
    MissingWaiver(String),
    #[error("waiver for stage '{0}' has an empty reason")]
    EmptyWaiverReason(String),
    #[error("waiver for stage '{0}' is outside (stage_ready, attempt_stage)")]
    ExtraWaiver(String),
    #[error("unknown stage label '{0}' (want G0..=G6)")]
    BadStage(String),
    #[error("manifest expires_at '{0}' is not past (now={1})")]
    Expired(String, u64),
    #[error("manifest expires_at '{0}' is not UTC RFC3339 'YYYY-MM-DDTHH:MM:SSZ'")]
    BadExpiry(String),
    #[error("hash mismatch for '{0}' (recomputed != manifest)")]
    HashMismatch(&'static str),
    #[error("arm lock: LIVE_ARM missing or mismatched")]
    ArmRefused,
    #[error("io: {0}")]
    Io(String),
}

/// SHA-256 of bytes, lowercase hex.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// SHA-256 of a file's exact bytes.
pub fn sha256_file(path: &std::path::Path) -> Result<String, ManifestError> {
    let bytes = std::fs::read(path).map_err(|e| ManifestError::Io(e.to_string()))?;
    Ok(sha256_hex(&bytes))
}

/// Constant-time equality for arm comparison. Lengths are not secret here
/// (both sides are 64-hex digests); content comparison is constant-time.
pub fn arm_constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// Arm lock: `LIVE_ARM` must equal `sha256(manifest_bytes)`. Returns `Ok(())`
/// on match, `Err(ArmRefused)` otherwise. Callers log match/mismatch only.
pub fn verify_arm(provided: Option<&str>, manifest_bytes: &[u8]) -> Result<(), ManifestError> {
    let expected = sha256_hex(manifest_bytes);
    match provided {
        Some(got) if arm_constant_time_eq(got.trim().as_bytes(), expected.as_bytes()) => Ok(()),
        _ => Err(ManifestError::ArmRefused),
    }
}

/// Parse strict UTC `YYYY-MM-DDTHH:MM:SSZ` to unix seconds. Fixed format only:
/// anything else is a fail-closed `BadExpiry` (no chrono dependency on
/// purpose; expiry parsing must stay dependency-light and deterministic).
pub fn parse_expiry_utc(s: &str) -> Result<u64, ManifestError> {
    let bad = || ManifestError::BadExpiry(s.to_string());
    if s.len() != 20
        || !s.ends_with('Z')
        || s.as_bytes()[10] != b'T'
        || s.as_bytes()[4] != b'-'
        || s.as_bytes()[7] != b'-'
        || s.as_bytes()[13] != b':'
        || s.as_bytes()[16] != b':'
    {
        return Err(bad());
    }
    let num = |lo: usize, hi: usize| -> Result<u64, ManifestError> {
        s[lo..hi].parse::<u64>().map_err(|_| bad())
    };
    let (y, mo, d, h, mi, sec) = (
        num(0, 4)?,
        num(5, 7)?,
        num(8, 10)?,
        num(11, 13)?,
        num(14, 16)?,
        num(17, 19)?,
    );
    if !(1970..=9999).contains(&y)
        || !(1..=12).contains(&mo)
        || !(1..=31).contains(&d)
        || h > 23
        || mi > 59
        || sec > 60
    {
        return Err(bad());
    }
    // Days since epoch (civil algorithm; valid for the checked ranges).
    let y_adj = if mo <= 2 { y - 1 } else { y };
    let era = y_adj / 400;
    let yoe = y_adj - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Ok(days * 86400 + h * 3600 + mi * 60 + sec)
}

/// Manifest lock: commit == embedded build commit, chain ids agree three
/// ways (manifest == profile == RPC), then the attempt-authorization rule:
/// `attempt_stage` equals the profile's required stage, `stage_ready` is a
/// valid label strictly below `attempt_stage` (the attempt is uncompleted
/// work -- completion is proven by stage-exit evidence, never by the
/// manifest), every rung strictly between the two appears in `waived` with
/// a non-empty reason, and no waiver names a rung outside that open
/// interval. Then expiry is in the future, and the three pinned hashes
/// recompute exactly.
#[allow(clippy::too_many_arguments)]
pub fn verify_manifest(
    manifest: &ReadinessManifest,
    embedded_commit: &str,
    profile: &LiveProfile,
    rpc_chain_id: u64,
    now_secs: u64,
    freeze_bytes: &[u8],
    profile_bytes: &[u8],
    lock_bytes: &[u8],
) -> Result<(), ManifestError> {
    if manifest.commit.trim() != embedded_commit.trim() || embedded_commit.trim().is_empty() {
        return Err(ManifestError::CommitMismatch);
    }
    if manifest.chain_id != profile.base.chain_id {
        return Err(ManifestError::ProfileChainMismatch(
            manifest.chain_id,
            profile.base.chain_id,
        ));
    }
    if manifest.chain_id != rpc_chain_id {
        return Err(ManifestError::RpcChainMismatch(
            manifest.chain_id,
            rpc_chain_id,
        ));
    }
    let attempt = stage_order(&manifest.attempt_stage)
        .ok_or_else(|| ManifestError::BadStage(manifest.attempt_stage.clone()))?;
    let required = stage_order(&profile.live_section.stage_required)
        .ok_or_else(|| ManifestError::BadStage(profile.live_section.stage_required.clone()))?;
    if attempt != required {
        return Err(ManifestError::AttemptMismatch(
            manifest.attempt_stage.clone(),
            profile.live_section.stage_required.clone(),
        ));
    }
    let ready = stage_order(&manifest.stage_ready)
        .ok_or_else(|| ManifestError::BadStage(manifest.stage_ready.clone()))?;
    if ready >= attempt {
        return Err(ManifestError::ReadyNotBelowAttempt(
            manifest.stage_ready.clone(),
            manifest.attempt_stage.clone(),
        ));
    }
    // Each waiver entry, in order: known label, non-empty reason, inside
    // the open interval (ready, attempt). Coverage is checked after, so a
    // manifest that is both extra AND missing reports the extra first
    // (deterministic; either way it refuses).
    for w in &manifest.waived {
        let order =
            stage_order(&w.stage).ok_or_else(|| ManifestError::BadStage(w.stage.clone()))?;
        if w.reason.trim().is_empty() {
            return Err(ManifestError::EmptyWaiverReason(w.stage.clone()));
        }
        if order <= ready || order >= attempt {
            return Err(ManifestError::ExtraWaiver(w.stage.clone()));
        }
    }
    for rung in (ready + 1)..attempt {
        let label = stage_label(rung).expect("rung inside (ready, attempt) is G0..=G6");
        let covered = manifest
            .waived
            .iter()
            .any(|w| stage_order(&w.stage) == Some(rung));
        if !covered {
            return Err(ManifestError::MissingWaiver(label.to_string()));
        }
    }
    let expiry = parse_expiry_utc(&manifest.expires_at)?;
    if expiry <= now_secs {
        return Err(ManifestError::Expired(
            manifest.expires_at.clone(),
            now_secs,
        ));
    }
    if sha256_hex(freeze_bytes) != manifest.hashes.freeze_md {
        return Err(ManifestError::HashMismatch("docs/FREEZE.md"));
    }
    if sha256_hex(profile_bytes) != manifest.hashes.profile {
        return Err(ManifestError::HashMismatch("profile"));
    }
    if sha256_hex(lock_bytes) != manifest.hashes.cargo_lock {
        return Err(ManifestError::HashMismatch("Cargo.lock"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Test manifest matching the real profile files. Hashes are filled by
    /// the helper below from FIXTURE bytes (each L10 refusal test then
    /// mutates one element). The stage triple arms the adjacent attempt:
    /// `stage_ready` one rung below the profile's required stage,
    /// `attempt_stage` equal to it, no waivers needed.
    fn manifest_for(profile: &LiveProfile, chain_id: u64) -> (ReadinessManifest, Vec<u8>) {
        let required = &profile.live_section.stage_required;
        let req_order = super::super::profile::stage_order(required).expect("profile stage valid");
        let m = ReadinessManifest {
            commit: "test-commit".to_string(),
            chain_id,
            stage_ready: stage_label(req_order - 1)
                .expect("required >= G1")
                .to_string(),
            attempt_stage: required.clone(),
            waived: Vec::new(),
            expires_at: "2099-01-01T00:00:00Z".to_string(),
            hashes: ManifestHashes {
                freeze_md: sha256_hex(b"freeze"),
                profile: sha256_hex(b"profile"),
                cargo_lock: sha256_hex(b"lock"),
            },
        };
        let bytes = serde_json::to_vec(&m).expect("serializes");
        (m, bytes)
    }

    fn waiver(stage: &str, reason: &str) -> Waiver {
        Waiver {
            stage: stage.to_string(),
            reason: reason.to_string(),
        }
    }

    fn check(
        m: &ReadinessManifest,
        profile: &LiveProfile,
        rpc_chain: u64,
    ) -> Result<(), ManifestError> {
        verify_manifest(
            m,
            "test-commit",
            profile,
            rpc_chain,
            1_700_000_000,
            b"freeze",
            b"profile",
            b"lock",
        )
    }

    #[test]
    fn accepts_matching_manifest() {
        let p = LiveProfile::load(Path::new("config/sepolia.toml")).expect("profile");
        let (m, _) = manifest_for(&p, 84532);
        assert!(check(&m, &p, 84532).is_ok());
    }

    #[test]
    fn refuses_each_mismatched_element() {
        let p = LiveProfile::load(Path::new("config/sepolia.toml")).expect("profile");
        let (mut m, _) = manifest_for(&p, 84532);
        // Commit.
        m.commit = "other".to_string();
        assert_eq!(check(&m, &p, 84532), Err(ManifestError::CommitMismatch));
        let (mut m, _) = manifest_for(&p, 84532);
        // Profile chain.
        m.chain_id = 8453;
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::ProfileChainMismatch(8453, 84532))
        );
        // RPC chain.
        let (m, _) = manifest_for(&p, 84532);
        assert_eq!(
            check(&m, &p, 8453),
            Err(ManifestError::RpcChainMismatch(84532, 8453))
        );
        // Expiry.
        let (mut m, _) = manifest_for(&p, 84532);
        m.expires_at = "2020-01-01T00:00:00Z".to_string();
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::Expired(
                "2020-01-01T00:00:00Z".to_string(),
                1_700_000_000
            ))
        );
        // Each recomputed hash.
        let (mut m, _) = manifest_for(&p, 84532);
        m.hashes.freeze_md = "00".to_string();
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::HashMismatch("docs/FREEZE.md"))
        );
        let (mut m, _) = manifest_for(&p, 84532);
        m.hashes.profile = "00".to_string();
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::HashMismatch("profile"))
        );
        let (mut m, _) = manifest_for(&p, 84532);
        m.hashes.cargo_lock = "00".to_string();
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::HashMismatch("Cargo.lock"))
        );
    }

    #[test]
    fn attempt_must_equal_required_stage() {
        // Sepolia requires G3: attempting above or below refuses even when
        // the rest of the stage triple is internally consistent.
        let p = LiveProfile::load(Path::new("config/sepolia.toml")).expect("profile");
        let (mut m, _) = manifest_for(&p, 84532);
        m.attempt_stage = "G4".to_string();
        m.stage_ready = "G2".to_string();
        m.waived = vec![waiver("G3", "reason")];
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::AttemptMismatch(
                "G4".to_string(),
                "G3".to_string()
            ))
        );
        let (mut m, _) = manifest_for(&p, 84532);
        m.attempt_stage = "G2".to_string();
        m.stage_ready = "G0".to_string();
        m.waived = vec![waiver("G1", "reason")];
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::AttemptMismatch(
                "G2".to_string(),
                "G3".to_string()
            ))
        );
    }

    #[test]
    fn ready_at_or_above_attempt_refuses_nothing_to_attempt() {
        // attempt == ready: there is no uncompleted work to attempt.
        let p = LiveProfile::load(Path::new("config/sepolia.toml")).expect("profile");
        let (mut m, _) = manifest_for(&p, 84532);
        m.stage_ready = "G3".to_string();
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::ReadyNotBelowAttempt(
                "G3".to_string(),
                "G3".to_string()
            ))
        );
        // ready above attempt: also refused (never equal/above).
        let (mut m, _) = manifest_for(&p, 84532);
        m.attempt_stage = "G3".to_string();
        m.stage_ready = "G4".to_string();
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::ReadyNotBelowAttempt(
                "G4".to_string(),
                "G3".to_string()
            ))
        );
    }

    #[test]
    fn gap_without_waiver_refuses() {
        let p = LiveProfile::load(Path::new("config/sepolia.toml")).expect("profile");
        let (mut m, _) = manifest_for(&p, 84532);
        m.stage_ready = "G0".to_string();
        m.waived = Vec::new();
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::MissingWaiver("G1".to_string()))
        );
        // Partial coverage still refuses at the first uncovered rung.
        let (mut m, _) = manifest_for(&p, 84532);
        m.stage_ready = "G0".to_string();
        m.waived = vec![waiver("G1", "reason")];
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::MissingWaiver("G2".to_string()))
        );
    }

    #[test]
    fn waiver_with_empty_reason_refuses() {
        let p = LiveProfile::load(Path::new("config/sepolia.toml")).expect("profile");
        let (mut m, _) = manifest_for(&p, 84532);
        m.stage_ready = "G0".to_string();
        m.waived = vec![waiver("G1", "shadow deferred to D1-D12"), waiver("G2", "")];
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::EmptyWaiverReason("G2".to_string()))
        );
        // Whitespace-only is empty too.
        let (mut m, _) = manifest_for(&p, 84532);
        m.stage_ready = "G0".to_string();
        m.waived = vec![waiver("G1", "ok"), waiver("G2", "   ")];
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::EmptyWaiverReason("G2".to_string()))
        );
    }

    #[test]
    fn drill_gap_with_reasons_arms() {
        // The drill triple: ready=G0, attempt=G3, waived=[G1, G2] with
        // reasons. Order of waivers must not matter.
        let p = LiveProfile::load(Path::new("config/sepolia.toml")).expect("profile");
        let (mut m, _) = manifest_for(&p, 84532);
        m.stage_ready = "G0".to_string();
        m.waived = vec![
            waiver("G2", "fork matrix deferred: mock-only drill entry"),
            waiver("G1", "shadow run deferred: mock-only drill entry"),
        ];
        assert!(check(&m, &p, 84532).is_ok());
        // Duplicate entries for the same rung are tolerated (set cover).
        let (mut m, _) = manifest_for(&p, 84532);
        m.stage_ready = "G1".to_string();
        m.waived = vec![waiver("G2", "first"), waiver("G2", "second")];
        assert!(check(&m, &p, 84532).is_ok());
    }

    #[test]
    fn waiver_outside_open_interval_refuses() {
        // Fail closed: waivers for rungs outside (ready, attempt) are
        // rejected, never silently ignored. At/above the attempt, at/below
        // the ready, and past the profile minimum all refuse.
        let p = LiveProfile::load(Path::new("config/sepolia.toml")).expect("profile");
        // Waiver AT ready (G2) with the adjacent triple otherwise arming.
        let (mut m, _) = manifest_for(&p, 84532);
        m.waived = vec![waiver("G2", "reason")];
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::ExtraWaiver("G2".to_string()))
        );
        // Waiver AT attempt (G3).
        let (mut m, _) = manifest_for(&p, 84532);
        m.waived = vec![waiver("G3", "reason")];
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::ExtraWaiver("G3".to_string()))
        );
        // Waiver BELOW ready while the gap itself is covered.
        let (mut m, _) = manifest_for(&p, 84532);
        m.stage_ready = "G1".to_string();
        m.waived = vec![waiver("G2", "covers the gap"), waiver("G0", "stale")];
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::ExtraWaiver("G0".to_string()))
        );
        // Waiver ABOVE attempt while the gap itself is covered.
        let (mut m, _) = manifest_for(&p, 84532);
        m.stage_ready = "G0".to_string();
        m.waived = vec![
            waiver("G1", "covers"),
            waiver("G2", "covers"),
            waiver("G4", "future"),
        ];
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::ExtraWaiver("G4".to_string()))
        );
    }

    #[test]
    fn unknown_stage_labels_refuse() {
        let p = LiveProfile::load(Path::new("config/sepolia.toml")).expect("profile");
        let (mut m, _) = manifest_for(&p, 84532);
        m.attempt_stage = "G7".to_string();
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::BadStage("G7".to_string()))
        );
        let (mut m, _) = manifest_for(&p, 84532);
        m.stage_ready = "g2".to_string();
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::BadStage("g2".to_string()))
        );
        let (mut m, _) = manifest_for(&p, 84532);
        m.stage_ready = "G0".to_string();
        m.waived = vec![waiver("G1", "ok"), waiver("nope", "ok")];
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::BadStage("nope".to_string()))
        );
        // A profile with an unknown required stage refuses too.
        let mut bad = p.clone();
        bad.live_section.stage_required = "nope".to_string();
        let (m, _) = manifest_for(&p, 84532);
        assert_eq!(
            verify_manifest(
                &m,
                "test-commit",
                &bad,
                84532,
                1_700_000_000,
                b"freeze",
                b"profile",
                b"lock",
            ),
            Err(ManifestError::BadStage("nope".to_string()))
        );
    }

    #[test]
    fn old_manifest_without_new_fields_fails_closed_at_parse() {
        // Pre-fix manifests (no attempt_stage/waived) must refuse via the
        // serde parse-error path -- they can never reach the lock.
        let old = serde_json::json!({
            "commit": "test-commit",
            "chain_id": 84532,
            "stage_ready": "G3",
            "expires_at": "2099-01-01T00:00:00Z",
            "hashes": {
                "freeze_md": "00",
                "profile": "00",
                "cargo_lock": "00"
            }
        });
        let raw = serde_json::to_vec(&old).expect("serializes");
        let parsed: Result<ReadinessManifest, _> = serde_json::from_slice(&raw);
        assert!(parsed.is_err(), "old manifest must fail closed at parse");
        // Missing only `waived` still refuses.
        let mut partial = old.clone();
        partial["attempt_stage"] = serde_json::json!("G3");
        let raw = serde_json::to_vec(&partial).expect("serializes");
        let parsed: Result<ReadinessManifest, _> = serde_json::from_slice(&raw);
        assert!(parsed.is_err(), "manifest without waived must fail closed");
    }

    #[test]
    fn arm_lock_matches_only_exact_digest() {
        let body = br#"{"commit":"abc"}"#;
        let good = sha256_hex(body);
        assert!(verify_arm(Some(&good), body).is_ok());
        assert_eq!(verify_arm(None, body), Err(ManifestError::ArmRefused));
        assert_eq!(verify_arm(Some(""), body), Err(ManifestError::ArmRefused));
        let mut bad = good.clone();
        bad.pop();
        bad.push('0');
        assert_eq!(verify_arm(Some(&bad), body), Err(ManifestError::ArmRefused));
        // Whitespace tolerance only at the edges; the digest itself is exact.
        assert!(verify_arm(Some(&format!("  {good}  ")), body).is_ok());
    }

    #[test]
    fn constant_time_eq_never_panics_and_compares() {
        assert!(arm_constant_time_eq(b"abc", b"abc"));
        assert!(!arm_constant_time_eq(b"abc", b"abd"));
        assert!(!arm_constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn expiry_parses_strict_utc_only() {
        assert_eq!(parse_expiry_utc("2099-01-01T00:00:00Z"), Ok(4070908800));
        assert_eq!(parse_expiry_utc("1970-01-01T00:00:00Z"), Ok(0));
        for bad in [
            "2099-01-01 00:00:00Z",
            "2099-01-01T00:00:00",
            "2099-01-01T00:00:00+00:00",
            "not-a-date",
            "",
            "2099-13-01T00:00:00Z",
            "2099-01-01T25:00:00Z",
        ] {
            assert!(parse_expiry_utc(bad).is_err(), "must refuse {bad}");
        }
    }

    #[test]
    fn sha256_matches_known_vector() {
        // Empty-string vector (independent of our own code paths).
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
