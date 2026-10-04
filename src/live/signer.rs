//! L3 signer isolation: operator key only, Sepolia-only env keys.
//!
//! - [`LiveSigner`] trait with two concrete types: [`EnvKeySigner`] asserts
//!   `chain_id == 84532` (Sepolia throwaways) and refuses mainnet before
//!   touching key material; [`RemoteSigner`] is the mainnet-only KMS stub
//!   (fail-closed until P4 wires it).
//! - The trading binary loads the **operator** key only.
//!   [`guard_trading_custody`] refuses startup when `OWNER_KEY` or
//!   `PAUSER_KEY` is visible (mirroring the Discord custody guard); it never
//!   reads the operator value itself.
//! - Keys never appear in logs, errors, metrics, or the ledger:
//!   [`redact_secret`] scrubs exact secrets plus key-shaped hex,
//!   [`sanitize_error`] funnels `anyhow` signer errors through it, signers
//!   use a custom [`Debug`](std::fmt::Debug) (address/label only), and key
//!   buffers are [`zeroize::Zeroize`]d on every path.

use alloy::primitives::{Address, B256};
use alloy::signers::local::PrivateKeySigner;
use std::fmt;
use thiserror::Error;
use zeroize::Zeroize;

use super::profile::{MAINNET_CHAIN_ID, SEPOLIA_CHAIN_ID};

/// Which signer implementation a startup may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignerKind {
    /// Key from `OPERATOR_KEY` env (Sepolia only).
    Env,
    /// Remote signer / KMS handle (mainnet only; stubbed until P4).
    Remote,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SignerError {
    #[error("env-key signer refused: chain {0} is not Sepolia (84532)")]
    EnvKeyRefusedForChain(u64),
    #[error("remote signer refused: chain {0} is not mainnet (8453)")]
    RemoteRefusedForChain(u64),
    #[error("refusing to start: {0} is set in the trading process (operator key only)")]
    CustodyRefused(&'static str),
    #[error("missing OPERATOR_KEY in the environment")]
    MissingOperatorKey,
    #[error("invalid OPERATOR_KEY (not 32-byte hex)")]
    BadOperatorKey,
    #[error("remote signer/KMS not configured (P4 wiring)")]
    KmsNotConfigured,
}

/// Chain binding for signer selection: env keys stay on Sepolia throwaways,
/// mainnet accepts the remote signer only.
pub fn select_signer_kind(chain_id: u64, kind: SignerKind) -> Result<(), SignerError> {
    match kind {
        SignerKind::Env if chain_id == SEPOLIA_CHAIN_ID => Ok(()),
        SignerKind::Env => Err(SignerError::EnvKeyRefusedForChain(chain_id)),
        SignerKind::Remote if chain_id == MAINNET_CHAIN_ID => Ok(()),
        SignerKind::Remote => Err(SignerError::RemoteRefusedForChain(chain_id)),
    }
}

/// Custody inputs for [`guard_trading_custody`]. Deliberately has NO operator
/// field: the guard must never read the operator value.
#[derive(Debug, Default)]
pub struct CustodyEnv {
    pub owner_key_present: bool,
    pub pauser_key_present: bool,
}

/// Refuse startup when owner/pauser keys are visible in the trading process.
/// Names the variable, never its value.
pub fn guard_trading_custody(env: &CustodyEnv) -> Result<(), SignerError> {
    if env.owner_key_present {
        return Err(SignerError::CustodyRefused("OWNER_KEY"));
    }
    if env.pauser_key_present {
        return Err(SignerError::CustodyRefused("PAUSER_KEY"));
    }
    Ok(())
}

/// Read the process custody (presence only, never values).
pub fn custody_from_process_env() -> CustodyEnv {
    CustodyEnv {
        owner_key_present: present("OWNER_KEY"),
        pauser_key_present: present("PAUSER_KEY"),
    }
}

fn present(var: &str) -> bool {
    std::env::var(var)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Minimal live-signer surface: identity only. Concrete signing happens
/// through the typed methods below (P4 wires submission).
pub trait LiveSigner: Send + Sync {
    fn address(&self) -> Address;
    fn label(&self) -> &str;
}

/// Env-held operator key (Sepolia only). Holds real key material: never
/// `Clone`, custom `Debug`, zeroized decode buffer.
pub struct EnvKeySigner {
    address: Address,
    inner: PrivateKeySigner,
}

impl EnvKeySigner {
    /// Build from a hex key (with or without `0x`). The chain assertion runs
    /// FIRST: a mainnet call is refused before any key byte is decoded.
    pub fn new(chain_id: u64, key_hex: &str) -> Result<Self, SignerError> {
        select_signer_kind(chain_id, SignerKind::Env)?;
        let clean = key_hex.trim().strip_prefix("0x").unwrap_or(key_hex.trim());
        let mut raw = hex::decode(clean).map_err(|_| SignerError::BadOperatorKey)?;
        if raw.len() != 32 {
            raw.zeroize();
            return Err(SignerError::BadOperatorKey);
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&raw);
        raw.zeroize();
        Self::from_key_bytes(chain_id, &mut arr)
    }

    /// Build from raw bytes, zeroizing `key` on every path (Ok and Err).
    /// `chain_id` is re-checked so direct callers cannot skip the binding.
    pub fn from_key_bytes(chain_id: u64, key: &mut [u8; 32]) -> Result<Self, SignerError> {
        let result = (|| {
            select_signer_kind(chain_id, SignerKind::Env)?;
            let signer =
                PrivateKeySigner::from_slice(&key[..]).map_err(|_| SignerError::BadOperatorKey)?;
            Ok::<_, SignerError>(signer)
        })();
        let signer = match result {
            Ok(s) => s,
            Err(e) => {
                key.zeroize();
                return Err(e);
            }
        };
        let address = signer.address();
        key.zeroize();
        Ok(Self {
            address,
            inner: signer,
        })
    }

    /// Load the operator key from the process environment. Reads
    /// `OPERATOR_KEY` only (after the custody guard); owner/pauser values
    /// are never touched. The env string is zeroized after decoding.
    pub fn from_env(chain_id: u64) -> Result<Self, SignerError> {
        guard_trading_custody(&custody_from_process_env())?;
        let mut held =
            std::env::var("OPERATOR_KEY").map_err(|_| SignerError::MissingOperatorKey)?;
        if held.trim().is_empty() {
            held.zeroize();
            return Err(SignerError::MissingOperatorKey);
        }
        let signer = Self::new(chain_id, &held);
        held.zeroize();
        signer
    }

    /// Sign a 32-byte digest (authorization-gated submission path, P4).
    pub async fn sign_digest(&self, digest: B256) -> anyhow::Result<[u8; 65]> {
        use alloy::signers::Signer as _;
        let sig = self.inner.sign_hash(&digest).await?;
        Ok(sig.as_bytes())
    }
}

impl LiveSigner for EnvKeySigner {
    fn address(&self) -> Address {
        self.address
    }
    fn label(&self) -> &str {
        "env-operator-key"
    }
}

/// Custom `Debug`: address and label only -- never key material, never the
/// inner signer (whose `Debug` could leak internals across versions).
impl fmt::Debug for EnvKeySigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnvKeySigner")
            .field("address", &self.address)
            .field("label", &self.label())
            .finish()
    }
}

/// Remote signer / KMS stub (mainnet only). Construction binds the chain;
/// every signing attempt fails closed until P4 wires the KMS handle.
pub struct RemoteSigner {
    address: Address,
    label: String,
}

impl RemoteSigner {
    pub fn new(chain_id: u64, address: Address) -> Result<Self, SignerError> {
        select_signer_kind(chain_id, SignerKind::Remote)?;
        Ok(Self {
            address,
            label: "remote-kms".to_string(),
        })
    }

    pub async fn sign_digest(&self, _digest: B256) -> anyhow::Result<[u8; 65]> {
        anyhow::bail!(SignerError::KmsNotConfigured)
    }
}

impl LiveSigner for RemoteSigner {
    fn address(&self) -> Address {
        self.address
    }
    fn label(&self) -> &str {
        &self.label
    }
}

impl fmt::Debug for RemoteSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteSigner")
            .field("address", &self.address)
            .field("label", &self.label)
            .finish()
    }
}

/// Scrub a string for logs/errors: every exact `secrets` value plus every
/// `0x` + 64-hex (private-key-shaped) value becomes `[REDACTED]`.
/// Addresses (40 hex) pass through; tx hashes travel in ledger fields, never
/// in errors passed through here.
pub fn redact_secret(input: &str, secrets: &[&str]) -> String {
    let mut out = input.to_string();
    for secret in secrets {
        let trimmed = secret.trim();
        if !trimmed.is_empty() {
            out = out.replace(trimmed, "[REDACTED]");
        }
    }
    let chars: Vec<char> = out.chars().collect();
    let mut res = String::with_capacity(out.len());
    let mut i = 0;
    while i < chars.len() {
        let is_key = chars[i] == '0'
            && i + 1 < chars.len()
            && (chars[i + 1] == 'x' || chars[i + 1] == 'X')
            && i + 66 <= chars.len()
            && chars[i + 2..i + 66].iter().all(|c| c.is_ascii_hexdigit());
        if is_key {
            res.push_str("[REDACTED]");
            i += 66;
        } else {
            res.push(chars[i]);
            i += 1;
        }
    }
    res
}

/// Funnel an `anyhow` signer error through [`redact_secret`] (never logs the
/// raw error, which could echo key material from a parse failure).
pub fn sanitize_error(err: &anyhow::Error, secrets: &[&str]) -> String {
    redact_secret(&format!("{err:#}"), secrets)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Obviously-fake test key (never a real secret; no `0x` prefix so the
    /// S5 `0x`+64hex scan stays silent).
    const FAKE_KEY_HEX: &str = "4242424242424242424242424242424242424242424242424242424242424242";

    #[test]
    fn signer_kind_matrix_per_combination() {
        // L10: env on Sepolia only, remote on mainnet only.
        assert!(select_signer_kind(SEPOLIA_CHAIN_ID, SignerKind::Env).is_ok());
        assert_eq!(
            select_signer_kind(MAINNET_CHAIN_ID, SignerKind::Env),
            Err(SignerError::EnvKeyRefusedForChain(MAINNET_CHAIN_ID))
        );
        assert_eq!(
            select_signer_kind(1, SignerKind::Env),
            Err(SignerError::EnvKeyRefusedForChain(1))
        );
        assert!(select_signer_kind(MAINNET_CHAIN_ID, SignerKind::Remote).is_ok());
        assert_eq!(
            select_signer_kind(SEPOLIA_CHAIN_ID, SignerKind::Remote),
            Err(SignerError::RemoteRefusedForChain(SEPOLIA_CHAIN_ID))
        );
    }

    #[test]
    fn env_key_refused_before_touching_key_material() {
        // Garbage key + mainnet chain: the CHAIN error must win, proving the
        // assertion runs before any key parsing.
        assert_eq!(
            EnvKeySigner::new(MAINNET_CHAIN_ID, "not-even-hex!!").unwrap_err(),
            SignerError::EnvKeyRefusedForChain(MAINNET_CHAIN_ID)
        );
    }

    #[test]
    fn env_key_builds_on_sepolia_and_zeroizes_buffer() {
        let mut buf = [0x42u8; 32];
        let signer =
            EnvKeySigner::from_key_bytes(SEPOLIA_CHAIN_ID, &mut buf).expect("fake key builds");
        assert_eq!(buf, [0u8; 32], "key buffer zeroized on Ok");
        assert_ne!(signer.address(), Address::ZERO);
        assert_eq!(signer.label(), "env-operator-key");
    }

    #[test]
    fn key_buffer_zeroized_on_refusal_too() {
        let mut buf = [0x42u8; 32];
        let err = EnvKeySigner::from_key_bytes(MAINNET_CHAIN_ID, &mut buf).unwrap_err();
        assert_eq!(err, SignerError::EnvKeyRefusedForChain(MAINNET_CHAIN_ID));
        assert_eq!(buf, [0u8; 32], "key buffer zeroized on Err");
    }

    #[test]
    fn debug_never_carries_key_material() {
        let signer = EnvKeySigner::new(SEPOLIA_CHAIN_ID, FAKE_KEY_HEX).expect("builds");
        let dbg = format!("{:?}", signer);
        assert!(dbg.contains("EnvKeySigner"));
        // Address `Debug` vs `Display` may differ in EIP-55 case; compare
        // case-insensitively (the check is "address present", not casing).
        assert!(
            dbg.to_ascii_lowercase()
                .contains(&signer.address().to_string().to_ascii_lowercase()),
            "address missing from Debug: {dbg}"
        );
        assert!(!dbg.contains(FAKE_KEY_HEX), "key leaked in Debug: {dbg}");
        assert!(!dbg.contains("4242"), "key fragment leaked in Debug: {dbg}");
    }

    #[test]
    fn remote_signer_is_mainnet_only_and_never_signs() {
        let addr: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        assert!(RemoteSigner::new(MAINNET_CHAIN_ID, addr).is_ok());
        assert_eq!(
            RemoteSigner::new(SEPOLIA_CHAIN_ID, addr).unwrap_err(),
            SignerError::RemoteRefusedForChain(SEPOLIA_CHAIN_ID)
        );
    }

    #[tokio::test]
    async fn remote_sign_refuses_without_kms() {
        let addr: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let r = RemoteSigner::new(MAINNET_CHAIN_ID, addr).expect("builds");
        let err = r.sign_digest(B256::ZERO).await.unwrap_err();
        assert!(err.to_string().contains("KMS not configured"));
    }

    #[test]
    fn custody_guard_refuses_owner_and_pauser_presence() {
        // L10: trading custody is operator-only.
        assert_eq!(
            guard_trading_custody(&CustodyEnv {
                owner_key_present: true,
                pauser_key_present: false
            }),
            Err(SignerError::CustodyRefused("OWNER_KEY"))
        );
        assert_eq!(
            guard_trading_custody(&CustodyEnv {
                owner_key_present: false,
                pauser_key_present: true
            }),
            Err(SignerError::CustodyRefused("PAUSER_KEY"))
        );
        assert!(guard_trading_custody(&CustodyEnv::default()).is_ok());
    }

    #[test]
    fn redact_scrubs_exact_secrets_and_key_shaped_hex() {
        // Fake secret + runtime-built key-shaped value (no 0x+64hex literal
        // in source, keeping the S5 scan silent).
        let fake_key = format!("0x{}", "ab".repeat(32));
        let addr = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
        let input = format!("key={fake_key} addr={addr} raw=test-only-fake-secret-001");
        let out = redact_secret(&input, &["test-only-fake-secret-001"]);
        assert!(!out.contains(&fake_key), "key survived: {out}");
        assert!(
            !out.contains("test-only-fake-secret-001"),
            "secret survived: {out}"
        );
        assert!(out.contains(addr), "address must pass through: {out}");
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn sanitize_error_never_echoes_key() {
        let fake_key = format!("0x{}", "cd".repeat(32));
        let err = anyhow::anyhow!("parse failed for input {fake_key}");
        let clean = sanitize_error(&err, &[]);
        assert!(!clean.contains(&fake_key), "key echoed: {clean}");
    }
}
