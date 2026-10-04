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

#[test]
fn p4_submit_book_reconcile_evidenced_offline() {
    // P4 call-site wiring, all offline: a fresh L5 approval (constructed
    // here with the same shape `pipeline::run_presubmit` returns; the
    // pipeline unit tests cover producing one) flows submit -> receipt
    // booking -> reconcile -> evidence, and every refusal (kill, breaker,
    // under-depth receipt, reconcile mismatch) fails closed with no partial
    // state. No network, no keys, no broadcast.
    use alloy::primitives::U256;
    use base_flash_arb::evidence::{EvidenceSummary, write_evidence};
    use base_flash_arb::live::breaker::Breaker;
    use base_flash_arb::live::pipeline::ApprovedIntent;
    use base_flash_arb::live::sender::{TxState, book_receipt, reconcile_and_trip};

    let exec = OnchainExecutor {
        executor: base_flash_arb::config::addresses::BALANCER_VAULT,
        chain_id: 84532,
    };
    let fresh_intent = || ApprovedIntent {
        size: U256::from(5_000_000u64),
        net: U256::from(1_000_000u64),
        gas_limit: 300_000,
        ledger_seq: 11,
    };
    let ok: Result<(), BreakerError> = Ok(());

    // Happy path: submit binds the nonce, a confirmed-at-depth receipt
    // books, per-tx reconcile passes, and the arming evidence honors the
    // honesty gates (window ordered, no realized presented).
    let dir = std::env::temp_dir().join("aero-live-l10-p4-happy");
    let _ = std::fs::remove_dir_all(&dir);
    let mut nonces = NonceManager::new(3);
    let mut breaker = Breaker::load(&dir, "test").expect("fresh");
    let pending = exec
        .submit(fresh_intent(), false, &ok, &mut nonces)
        .expect("submit binds");
    assert_eq!(pending.nonce, 3);
    assert_eq!(pending.ledger_seq, 11);
    let booked =
        book_receipt(&pending, TxState::Confirmed { confirmations: 1 }, 1).expect("at-depth books");
    assert_eq!(booked.ledger_seq, 11);
    assert!(reconcile_and_trip(&mut breaker, 1_000_000, 1_000_000, 0).is_ok());
    assert!(breaker.check().is_ok());
    let ev_dir = dir.join("evidence");
    let summary = EvidenceSummary::live_arming(84532, Some("head".to_string()), 1_700_000_000);
    write_evidence(&ev_dir, &summary).expect("honest evidence emits");
    let back: EvidenceSummary = serde_json::from_str(
        &std::fs::read_to_string(ev_dir.join("summary.json")).expect("summary written"),
    )
    .expect("auditor-consumable JSON");
    assert_eq!(back.ledger_head, Some("head".to_string()));
    assert_eq!(back.realized_samples, 0);
    assert!(back.window_end >= back.window_start);
    let _ = std::fs::remove_dir_all(&dir);

    // Refusal path: kill-engaged submit takes no nonce.
    let mut nonces = NonceManager::new(5);
    assert!(exec.submit(fresh_intent(), true, &ok, &mut nonces).is_err());
    assert_eq!(nonces.in_flight(), None);

    // Refusal path: tripped breaker refuses submit AND reconcile mismatch
    // trips a fresh breaker with the TripReason returned.
    let dir = std::env::temp_dir().join("aero-live-l10-p4-refuse");
    let _ = std::fs::remove_dir_all(&dir);
    let mut breaker = Breaker::load(&dir, "test").expect("fresh");
    breaker
        .trip(TripReason::ConsecutiveFailures)
        .expect("trips");
    let tripped: Result<(), BreakerError> = breaker.check();
    let mut nonces = NonceManager::new(5);
    assert!(
        exec.submit(fresh_intent(), false, &tripped, &mut nonces)
            .is_err()
    );
    assert_eq!(nonces.in_flight(), None, "no nonce on breaker refusal");

    // Refusal path: under-depth receipt never books (reorg safety), and a
    // reconcile mismatch returns the TripReason while tripping.
    let dir2 = std::env::temp_dir().join("aero-live-l10-p4-reconcile");
    let _ = std::fs::remove_dir_all(&dir2);
    let mut breaker2 = Breaker::load(&dir2, "test").expect("fresh");
    let pending = exec
        .submit(fresh_intent(), false, &ok, &mut NonceManager::new(0))
        .expect("binds");
    assert!(book_receipt(&pending, TxState::Submitted, 1).is_err());
    assert!(
        book_receipt(&pending, TxState::Confirmed { confirmations: 1 }, 5).is_err(),
        "under-depth refuses"
    );
    assert_eq!(
        reconcile_and_trip(&mut breaker2, 100, 200, 10),
        Err(TripReason::ReconcileMismatch)
    );
    assert!(breaker2.check().is_err(), "mismatch trips");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}
