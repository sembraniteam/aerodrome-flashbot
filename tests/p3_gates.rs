//! P3 ungated gates: run with AND without `--features live`.
//!
//! - S1: no signing/sending symbols outside `src/live/` + `src/bin/live.rs`
//!   and the two designed Discord pause-path files (mirrors the
//!   `collect-evidence.sh` S1 scan; Discord hits stay adjudicated by the
//!   readiness allowlist, and `s1_candidates_stay_pause_only` bounds them).
//! - S1b: `mod live` is feature-gated and the live binary requires it.
//! - L10 wiring: the paper tree never names `OnchainExecutor` (dry-run can
//!   never reach it).
//! - S8/L2: Sepolia and mainnet-canary profiles share no addresses.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// S1 scan tokens (same set as `collect-evidence.sh`).
const S1_TOKENS: &[&str] = &[
    "PrivateKeySigner",
    "LocalSigner",
    "EthereumWallet",
    "send_transaction",
    "send_raw_transaction",
    "eth_sendRawTransaction",
];

fn is_live_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.contains("src/live/") || s.ends_with("src/bin/live.rs") || s.ends_with("/live.rs")
}

/// Designed S1 exceptions (same set as `collect-evidence.sh`
/// `S1_CANDIDATES`): the Discord pause path holds the PAUSER key only.
/// Hits here are adjudicated by a human via `docs/readiness-allowlist.txt`,
/// never auto-passed -- this test only bounds WHAT they may do (see below).
fn is_s1_candidate(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.ends_with("src/bin/discord-bot.rs") || s.ends_with("src/discord.rs")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

fn contains_dot_sign_underscore(body: &str) -> bool {
    // Mirrors `\.sign_`: a literal '.' followed by 'sign_'.
    body.as_bytes().windows(6).any(|w| w == b".sign_")
}

#[test]
fn s1_no_signing_symbols_outside_live_paths() {
    let root = repo_root();
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    assert!(!files.is_empty(), "scan must see src/ files");
    let mut hits = Vec::new();
    for path in &files {
        if is_live_path(path) || is_s1_candidate(path) {
            continue;
        }
        let body = std::fs::read_to_string(path).expect("read rs file");
        for token in S1_TOKENS {
            if body.contains(token) {
                hits.push(format!("{}: {token}", path.display()));
            }
        }
        if contains_dot_sign_underscore(&body) {
            hits.push(format!("{}: .sign_", path.display()));
        }
    }
    assert!(
        hits.is_empty(),
        "S1 violation: signing/sending symbols outside src/live/ + src/bin/live.rs:\n{}",
        hits.join("\n")
    );
}

#[test]
fn s1_scan_is_not_vacuous() {
    // The exclusion must matter: live code really does hold the signer.
    let body = std::fs::read_to_string(repo_root().join("src/live/signer.rs"))
        .expect("live signer exists (P3)");
    assert!(body.contains("PrivateKeySigner"));
}

#[test]
fn s1_candidates_stay_pause_only() {
    // The designed Discord exception may construct the pauser wallet for
    // `pause()`; it must never gain a raw-send capability. A new hit class
    // here is a Critical finding (fix the code, not this test).
    const RAW_SEND: &[&str] = &[
        "LocalSigner",
        "send_transaction",
        "send_raw_transaction",
        "eth_sendRawTransaction",
    ];
    for file in ["src/bin/discord-bot.rs", "src/discord.rs"] {
        let body = std::fs::read_to_string(repo_root().join(file)).expect("candidate file exists");
        for token in RAW_SEND {
            assert!(
                !body.contains(token),
                "S1 candidate {file} gained raw-send capability: {token}"
            );
        }
        if !contains_dot_sign_underscore(&body) {
            continue;
        }
        panic!("S1 candidate {file} gained a direct signing call");
    }
}

#[test]
fn s1b_live_module_and_binary_are_feature_gated() {
    let lib = std::fs::read_to_string(repo_root().join("src/lib.rs")).expect("lib.rs");
    let mut lines = lib.lines().peekable();
    let mut gated = false;
    while let Some(line) = lines.next() {
        if line.trim() == "#[cfg(feature = \"live\")]"
            && lines.peek().is_some_and(|n| n.trim() == "pub mod live;")
        {
            gated = true;
        }
    }
    assert!(
        gated,
        "need `#[cfg(feature = \"live\")]` immediately before `pub mod live;`"
    );
    let cargo = std::fs::read_to_string(repo_root().join("Cargo.toml")).expect("Cargo.toml");
    assert!(
        cargo.contains("required-features = [\"live\"]"),
        "live binary must declare required-features = [\"live\"]"
    );
    assert!(
        repo_root().join("src/bin/live.rs").exists(),
        "src/bin/live.rs must exist"
    );
    assert!(
        repo_root().join("src/live").is_dir(),
        "src/live/ must exist"
    );
}

#[test]
fn dry_run_tree_never_names_onchain_executor() {
    // L10 wiring: dry-run can never reach `OnchainExecutor`. Only the live
    // paths may name the type (compile-time separation, not a comment).
    let root = repo_root();
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    let mut hits = Vec::new();
    for path in &files {
        if is_live_path(path) {
            continue;
        }
        let body = std::fs::read_to_string(path).expect("read rs file");
        if body.contains("OnchainExecutor") {
            hits.push(path.display().to_string());
        }
    }
    assert!(
        hits.is_empty(),
        "paper tree must never name OnchainExecutor: {hits:?}"
    );
    let live_body = std::fs::read_to_string(root.join("src/live/sender.rs")).expect("sender");
    assert!(live_body.contains("OnchainExecutor"));
}

/// Extract `0x...` addresses from profile text (40-hex), excluding the zero
/// sentinel used for "unconfigured".
fn profile_addresses(path: &Path) -> std::collections::BTreeSet<String> {
    let body = std::fs::read_to_string(path).expect("read profile");
    let mut out = std::collections::BTreeSet::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i + 42 <= bytes.len() {
        if bytes[i] == b'0' && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X') {
            let hex: Vec<u8> = bytes[i + 2..i + 42].to_vec();
            if hex.iter().all(|c| c.is_ascii_hexdigit()) {
                let addr = String::from_utf8(hex)
                    .expect("hex is ascii")
                    .to_ascii_lowercase();
                if addr != "0000000000000000000000000000000000000000" {
                    out.insert(addr);
                }
                i += 42;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[test]
fn profiles_are_address_disjoint() {
    // S8/L2: no address appears in both profiles.
    let root = repo_root();
    let sepolia = profile_addresses(&root.join("config/sepolia.toml"));
    let canary = profile_addresses(&root.join("config/mainnet-canary.toml"));
    assert!(
        !sepolia.is_empty() && !canary.is_empty(),
        "profiles must pin addresses"
    );
    let overlap: Vec<_> = sepolia.intersection(&canary).collect();
    assert!(
        overlap.is_empty(),
        "profile address overlap (S8): {overlap:?}"
    );
}
