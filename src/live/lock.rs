//! L1 Live Lock orchestration: build + config + arm + manifest, in order.
//!
//! Strict startup order in the live binary: L1 lock -> role read-back (L4)
//! -> float-cap check -> [`crate::live::signer`] construction LAST. No key
//! or KMS handle is loaded before L1 passes; each L1 failure asserts the
//! signer constructor was never called (L10 probe below).

use thiserror::Error;

use super::float_cap::{FloatCapError, check_float_cap};
use super::manifest::{ManifestError, ReadinessManifest, verify_arm, verify_manifest};
use super::profile::LiveProfile;
use super::roles::{RoleError, RoleMatrix, verify_roles};
use super::signer::{SignerError, SignerKind, select_signer_kind};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LockError {
    #[error("config lock: [live] enabled = false")]
    ConfigDisabled,
    #[error("manifest lock: {0}")]
    Manifest(#[from] ManifestError),
    #[error("role read-back: {0}")]
    Roles(#[from] RoleError),
    #[error("float cap: {0}")]
    FloatCap(#[from] FloatCapError),
    #[error("signer selection: {0}")]
    Signer(#[from] SignerError),
}

/// All L1 inputs. Byte slices (not paths) keep the pure core testable
/// without fixtures; the binary reads the files.
pub struct LockInputs<'a> {
    pub profile: &'a LiveProfile,
    pub manifest: &'a ReadinessManifest,
    pub manifest_bytes: &'a [u8],
    pub live_arm: Option<&'a str>,
    pub embedded_commit: &'a str,
    pub rpc_chain_id: u64,
    pub now_secs: u64,
    pub freeze_bytes: &'a [u8],
    pub profile_bytes: &'a [u8],
    pub lock_bytes: &'a [u8],
}

/// L1 lock: build (compile-time, asserted by the caller living behind the
/// `live` feature) -> config -> arm -> manifest.
pub fn live_lock(inputs: &LockInputs<'_>) -> Result<(), LockError> {
    // Build lock: this function links ONLY into the `live`-feature build
    // (`#[cfg(feature = "live")] mod live` + `required-features = ["live"]`
    // on the binary). The paper binary cannot name this symbol -- that
    // compile-time separation is the build lock, and the S1b gating test
    // (`tests/p3_gates.rs`) asserts the gating lines. No runtime assert can
    // prove more than Cargo already does.
    // Config lock.
    if !inputs.profile.live_section.enabled {
        return Err(LockError::ConfigDisabled);
    }
    // Arm lock (constant-time compare, match/mismatch only).
    verify_arm(inputs.live_arm, inputs.manifest_bytes)?;
    // Manifest lock (commit, three-way chain id, stage, expiry, hashes).
    verify_manifest(
        inputs.manifest,
        inputs.embedded_commit,
        inputs.profile,
        inputs.rpc_chain_id,
        inputs.now_secs,
        inputs.freeze_bytes,
        inputs.profile_bytes,
        inputs.lock_bytes,
    )?;
    Ok(())
}

/// Probe recording whether the signer step ran (L10: every refusal asserts
/// the signer constructor never ran).
#[derive(Debug, Default)]
pub struct StartupProbe {
    pub signer_step_reached: bool,
}

/// Full startup sequence: L1 -> roles -> float cap -> signer selection.
/// `on_signer_step` constructs the real signer; tests pass a probe closure
/// and assert it never ran on any refusal path.
pub fn startup_sequence(
    inputs: &LockInputs<'_>,
    observed_roles: &RoleMatrix,
    float_balance_equiv: Option<u64>,
    signer_kind: SignerKind,
    probe: &mut StartupProbe,
    on_signer_step: impl FnOnce() -> Result<(), SignerError>,
) -> Result<(), LockError> {
    live_lock(inputs)?;
    verify_roles(
        &RoleMatrix::from_profile(inputs.profile),
        observed_roles,
        inputs.profile.live_section.hard_max_flash_usdc,
    )?;
    check_float_cap(
        float_balance_equiv,
        inputs.profile.live_section.operator_float_cap_usdc,
    )?;
    select_signer_kind(inputs.profile.base.chain_id, signer_kind)?;
    probe.signer_step_reached = true;
    on_signer_step()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;
    use std::path::Path;

    fn profile() -> LiveProfile {
        LiveProfile::load(Path::new("config/sepolia.toml")).expect("profile")
    }

    fn manifest_bytes() -> Vec<u8> {
        let p = profile();
        let m = ReadinessManifest {
            commit: "test-commit".to_string(),
            chain_id: p.base.chain_id,
            stage_ready: p.live_section.stage_required.clone(),
            expires_at: "2099-01-01T00:00:00Z".to_string(),
            hashes: super::super::manifest::ManifestHashes {
                freeze_md: super::super::manifest::sha256_hex(b"freeze"),
                profile: super::super::manifest::sha256_hex(b"profile"),
                cargo_lock: super::super::manifest::sha256_hex(b"lock"),
            },
        };
        serde_json::to_vec(&m).expect("serializes")
    }

    fn manifest_of(bytes: &[u8]) -> ReadinessManifest {
        serde_json::from_slice(bytes).expect("parses")
    }

    /// Full startup inputs where everything passes; each refusal test
    /// mutates one element and asserts the signer step never ran.
    fn run_case(
        mutate: impl FnOnce(&mut LiveProfile, &mut RoleMatrix, &mut Option<u64>),
    ) -> (Result<(), LockError>, StartupProbe) {
        let mut p = profile();
        let mut roles = RoleMatrix::from_profile(&p);
        let mut float = Some(1_000_000u64);
        mutate(&mut p, &mut roles, &mut float);
        let bytes = manifest_bytes();
        // Recompute the manifest when the test mutated the profile: the
        // manifest must track the profile under test (chain id, stage).
        let mut m = manifest_of(&bytes);
        m.chain_id = p.base.chain_id;
        m.stage_ready = p.live_section.stage_required.clone();
        let m_bytes = serde_json::to_vec(&m).expect("serializes");
        // NOTE: hash/profile/commit elements keep the ORIGINAL bytes on
        // purpose (each refusal test below covers one lock element).
        let arm = super::super::manifest::sha256_hex(&m_bytes);
        let inputs = LockInputs {
            profile: &p,
            manifest: &m,
            manifest_bytes: &m_bytes,
            live_arm: Some(&arm),
            embedded_commit: "test-commit",
            rpc_chain_id: p.base.chain_id,
            now_secs: 1_700_000_000,
            freeze_bytes: b"freeze",
            profile_bytes: b"profile",
            lock_bytes: b"lock",
        };
        let mut probe = StartupProbe::default();
        let result = startup_sequence(&inputs, &roles, float, SignerKind::Env, &mut probe, || {
            Ok(())
        });
        (result, probe)
    }

    #[test]
    fn startup_passes_when_everything_agrees() {
        let (result, probe) = run_case(|_, _, _| {});
        assert!(result.is_ok(), "unexpected refusal: {result:?}");
        assert!(probe.signer_step_reached);
    }

    #[test]
    fn each_l1_failure_never_reaches_signer() {
        // Config disabled.
        let (r, probe) = run_case(|p, _, _| p.live_section.enabled = false);
        assert_eq!(r, Err(LockError::ConfigDisabled));
        assert!(!probe.signer_step_reached);
        // Role mismatch (L4 read-back).
        let (r, probe) = run_case(|_, roles, _| {
            roles.operator = Address::repeat_byte(0x99);
        });
        assert!(matches!(r, Err(LockError::Roles(..))));
        assert!(!probe.signer_step_reached);
        // Float cap: over and unknown.
        let (r, probe) = run_case(|_, _, float| *float = Some(99_000_000));
        assert!(matches!(r, Err(LockError::FloatCap(..))));
        assert!(!probe.signer_step_reached);
        let (r, probe) = run_case(|_, _, float| *float = None);
        assert!(matches!(r, Err(LockError::FloatCap(..))));
        assert!(!probe.signer_step_reached);
    }

    #[test]
    fn arm_and_manifest_refusals_never_reach_signer() {
        // Bad arm: rebuild inputs with a wrong arm value.
        let p = profile();
        let bytes = manifest_bytes();
        let m = manifest_of(&bytes);
        let inputs = LockInputs {
            profile: &p,
            manifest: &m,
            manifest_bytes: &bytes,
            live_arm: Some("wrong"),
            embedded_commit: "test-commit",
            rpc_chain_id: p.base.chain_id,
            now_secs: 1_700_000_000,
            freeze_bytes: b"freeze",
            profile_bytes: b"profile",
            lock_bytes: b"lock",
        };
        let mut probe = StartupProbe::default();
        let roles = RoleMatrix::from_profile(&p);
        let r = startup_sequence(
            &inputs,
            &roles,
            Some(1),
            SignerKind::Env,
            &mut probe,
            || Ok(()),
        );
        assert_eq!(r, Err(LockError::Manifest(ManifestError::ArmRefused)));
        assert!(!probe.signer_step_reached);
        // Wrong RPC chain id.
        let arm = super::super::manifest::sha256_hex(&bytes);
        let inputs = LockInputs {
            live_arm: Some(&arm),
            rpc_chain_id: 1,
            ..inputs
        };
        let mut probe = StartupProbe::default();
        let r = startup_sequence(
            &inputs,
            &roles,
            Some(1),
            SignerKind::Env,
            &mut probe,
            || Ok(()),
        );
        assert!(matches!(
            r,
            Err(LockError::Manifest(ManifestError::RpcChainMismatch(..)))
        ));
        assert!(!probe.signer_step_reached);
    }

    #[test]
    fn wrong_signer_kind_for_chain_never_constructs() {
        let (r, probe) = run_case(|_, _, _| {});
        assert!(r.is_ok());
        assert!(probe.signer_step_reached);
        // Same passing inputs but mainnet-kind on Sepolia chain.
        let p = profile();
        let bytes = manifest_bytes();
        let m = manifest_of(&bytes);
        let arm = super::super::manifest::sha256_hex(&bytes);
        let inputs = LockInputs {
            profile: &p,
            manifest: &m,
            manifest_bytes: &bytes,
            live_arm: Some(&arm),
            embedded_commit: "test-commit",
            rpc_chain_id: p.base.chain_id,
            now_secs: 1_700_000_000,
            freeze_bytes: b"freeze",
            profile_bytes: b"profile",
            lock_bytes: b"lock",
        };
        let mut probe = StartupProbe::default();
        let roles = RoleMatrix::from_profile(&p);
        let r = startup_sequence(
            &inputs,
            &roles,
            Some(1),
            SignerKind::Remote,
            &mut probe,
            || Ok(()),
        );
        assert!(matches!(r, Err(LockError::Signer(..))));
        assert!(!probe.signer_step_reached);
    }
}
