//! L1 manifest + arm lock: the readiness manifest the live binary trusts
//! only after re-verification.
//!
//! The manifest (see the evidence schema) names a `commit`, `chain_id`,
//! `stage_ready`, `expires_at`, and the hashes of `docs/FREEZE.md`, the
//! profile file, and `Cargo.lock`. The lock recomputes all three hashes and
//! compares them -- it reads the manifest, it does not trust it blindly.
//!
//! `LIVE_ARM` must equal the SHA-256 of the exact manifest bytes, compared
//! in constant time; logs record match/mismatch only, never values.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;

use super::profile::{LiveProfile, stage_order};

/// Hashes the manifest pins (recomputed at startup, never trusted blindly).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestHashes {
    pub freeze_md: String,
    pub profile: String,
    pub cargo_lock: String,
}

/// Readiness manifest subset the L1 lock enforces.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadinessManifest {
    pub commit: String,
    pub chain_id: u64,
    pub stage_ready: String,
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
    #[error("manifest stage_ready '{0}' below profile stage_required '{1}'")]
    StageInsufficient(String, String),
    #[error("manifest stage_ready '{0}' is not a G0..=G6 label")]
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
/// ways (manifest == profile == RPC), stage_ready reaches the profile's
/// required stage, expiry is in the future, and the three pinned hashes
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
    let ready = stage_order(&manifest.stage_ready)
        .ok_or_else(|| ManifestError::BadStage(manifest.stage_ready.clone()))?;
    let required = stage_order(&profile.live_section.stage_required)
        .ok_or_else(|| ManifestError::BadStage(profile.live_section.stage_required.clone()))?;
    if ready < required {
        return Err(ManifestError::StageInsufficient(
            manifest.stage_ready.clone(),
            profile.live_section.stage_required.clone(),
        ));
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
    /// mutates one element).
    fn manifest_for(profile: &LiveProfile, chain_id: u64) -> (ReadinessManifest, Vec<u8>) {
        let m = ReadinessManifest {
            commit: "test-commit".to_string(),
            chain_id,
            stage_ready: profile.live_section.stage_required.clone(),
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
        // Stage.
        let (mut m, _) = manifest_for(&p, 84532);
        m.stage_ready = "G2".to_string();
        assert_eq!(
            check(&m, &p, 84532),
            Err(ManifestError::StageInsufficient(
                "G2".to_string(),
                "G3".to_string()
            ))
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
