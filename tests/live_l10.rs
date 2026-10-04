//! L10 live orchestration tests (require `--features live`).
//!
//! These exercise the REAL profile/manifest/hash files end to end (the unit
//! tests use fixture bytes): the lock recomputes `docs/FREEZE.md`, the
//! profile, and `Cargo.lock` and only passes when every element agrees.
//! All offline, no network, no keys, no broadcasts.

#![cfg(feature = "live")]

use base_flash_arb::live::breaker::{Breaker, BreakerError, TripReason};
use base_flash_arb::live::ledger::{Ledger, LedgerKind};
use base_flash_arb::live::lock::{LockInputs, StartupProbe, startup_sequence};
use base_flash_arb::live::manifest::{ReadinessManifest, sha256_hex};
use base_flash_arb::live::profile::LiveProfile;
use base_flash_arb::live::roles::RoleMatrix;
use base_flash_arb::live::sender::{NonceManager, OnchainExecutor};
use base_flash_arb::live::signer::SignerKind;
use std::path::Path;

fn sepolia() -> LiveProfile {
    LiveProfile::load(Path::new("config/sepolia.toml")).expect("sepolia profile")
}

/// Build a manifest matching the REAL files, with the arm over the exact
/// manifest bytes (the same bytes the binary would hash for `LIVE_ARM`).
fn real_manifest(profile: &LiveProfile) -> (ReadinessManifest, Vec<u8>) {
    let freeze = std::fs::read("docs/FREEZE.md").expect("freeze");
    let prof = std::fs::read("config/sepolia.toml").expect("profile bytes");
    let lock = std::fs::read("Cargo.lock").expect("lock");
    let m = ReadinessManifest {
        commit: "test-commit".to_string(),
        chain_id: profile.base.chain_id,
        stage_ready: profile.live_section.stage_required.clone(),
        expires_at: "2099-01-01T00:00:00Z".to_string(),
        hashes: base_flash_arb::live::manifest::ManifestHashes {
            freeze_md: sha256_hex(&freeze),
            profile: sha256_hex(&prof),
            cargo_lock: sha256_hex(&lock),
        },
    };
    let bytes = serde_json::to_vec(&m).expect("serializes");
    (m, bytes)
}

#[test]
fn startup_passes_against_real_files() {
    let p = sepolia();
    let (m, m_bytes) = real_manifest(&p);
    let arm = sha256_hex(&m_bytes);
    let freeze = std::fs::read("docs/FREEZE.md").expect("freeze");
    let prof = std::fs::read("config/sepolia.toml").expect("profile bytes");
    let lock = std::fs::read("Cargo.lock").expect("lock");
    let inputs = LockInputs {
        profile: &p,
        manifest: &m,
        manifest_bytes: &m_bytes,
        live_arm: Some(&arm),
        embedded_commit: "test-commit",
        rpc_chain_id: p.base.chain_id,
        now_secs: base_flash_arb::evidence::now_utc_secs(),
        freeze_bytes: &freeze,
        profile_bytes: &prof,
        lock_bytes: &lock,
    };
    let roles = RoleMatrix::from_profile(&p);
    let mut probe = StartupProbe::default();
    startup_sequence(
        &inputs,
        &roles,
        Some(1_000_000),
        SignerKind::Env,
        &mut probe,
        || Ok(()),
    )
    .expect("real files agree: startup passes");
    assert!(probe.signer_step_reached);
}

#[test]
fn real_file_tamper_refuses_before_signer() {
    // Flipping one byte of the profile under a pinned manifest hash must
    // refuse (the lock reads the manifest; it does not trust it blindly).
    let p = sepolia();
    let (m, m_bytes) = real_manifest(&p);
    let arm = sha256_hex(&m_bytes);
    let freeze = std::fs::read("docs/FREEZE.md").expect("freeze");
    let mut prof = std::fs::read("config/sepolia.toml").expect("profile bytes");
    prof[100] ^= 0xFF;
    let lock = std::fs::read("Cargo.lock").expect("lock");
    let inputs = LockInputs {
        profile: &p,
        manifest: &m,
        manifest_bytes: &m_bytes,
        live_arm: Some(&arm),
        embedded_commit: "test-commit",
        rpc_chain_id: p.base.chain_id,
        now_secs: base_flash_arb::evidence::now_utc_secs(),
        freeze_bytes: &freeze,
        profile_bytes: &prof,
        lock_bytes: &lock,
    };
    let roles = RoleMatrix::from_profile(&p);
    let mut probe = StartupProbe::default();
    let result = startup_sequence(
        &inputs,
        &roles,
        Some(1_000_000),
        SignerKind::Env,
        &mut probe,
        || Ok(()),
    );
    assert!(result.is_err(), "tampered profile bytes must refuse");
    assert!(!probe.signer_step_reached);
}

#[test]
fn tripped_breaker_blocks_sender_preflight() {
    // L7 -> L6 wiring: a persisted trip refuses new submissions.
    let dir = std::env::temp_dir().join("aero-live-l10-breaker");
    let _ = std::fs::remove_dir_all(&dir);
    let mut b = Breaker::load(&dir, "test").expect("fresh");
    b.trip(TripReason::DriftBeyondBand).expect("trips");
    let exec = OnchainExecutor {
        executor: base_flash_arb::config::addresses::BALANCER_VAULT,
        chain_id: 8453,
    };
    let mut nonces = NonceManager::new(0);
    let tripped: Result<(), BreakerError> = b.check();
    assert!(tripped.is_err());
    assert!(
        exec.preflight(false, &tripped, &mut nonces).is_err(),
        "tripped breaker refuses preflight"
    );
    assert_eq!(nonces.in_flight(), None, "no nonce taken on refusal");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ledger_verify_roundtrip_on_temp_dir() {
    // L8 `ledger verify` entry point over a temp ledger.
    let dir = std::env::temp_dir().join("aero-live-l10-ledger");
    let _ = std::fs::remove_dir_all(&dir);
    let mut l = Ledger::open(&dir, Vec::new()).expect("opens");
    l.append(
        LedgerKind::Intent,
        serde_json::json!({"size": 50_000_000u64}),
    )
    .expect("intent");
    assert_eq!(base_flash_arb::live::ledger::verify_ledger_dir(&dir), Ok(1));
    let _ = std::fs::remove_dir_all(&dir);
}
