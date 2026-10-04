//! Live binary (ADR-002): armed ONLY through the L1 Live Lock.
//!
//! - Requires `--features live` (`required-features` in `Cargo.toml`); the
//!   paper build cannot link this target.
//! - Strict startup order: L1 lock -> role read-back (L4) -> float-cap
//!   check -> signer construction LAST. No key or KMS handle exists before
//!   L1 passes.
//! - P4 scope: lock + read-only verification + `--emit-evidence` +
//!   `ledger verify` + the OFFLINE submit/booking/reconcile state machine
//!   (`sender::submit` consumes an `ApprovedIntent`; `book_receipt` gates on
//!   `bookable()`; `reconcile_and_trip` trips on mismatch). No trading loop
//!   and no broadcast yet: a successful start prints ARMED and exits. The
//!   P4 runbooks execute drills by hand; hashes come back as evidence.
//! - Secrets: the operator value enters memory only post-lock (ledger
//!   append-boundary blocklist) or on the failure path (error-funnel
//!   sanitizer), via [`SecretBlocklist`] (both paste forms, zeroized on
//!   drop). Errors leaving this binary never carry key material. The
//!   operator NEVER runs this without a drill runbook. Real keys,
//!   deployments, funding, and broadcasts are human steps from runbooks.

use alloy::providers::{Provider, ProviderBuilder};
use base_flash_arb::chain::redact_url;
use base_flash_arb::evidence::{self, EvidenceSummary};
use base_flash_arb::live::{
    breaker::{self, Breaker},
    float_cap::eth_wei_to_usdc_base,
    ledger::Ledger,
    lock::{LockInputs, StartupProbe, startup_sequence},
    manifest::{ReadinessManifest, sha256_hex},
    profile::LiveProfile,
    roles::{RoleMatrix, read_roles},
    signer::{EnvKeySigner, LiveSigner, SecretBlocklist, SignerError, SignerKind, sanitize_error},
};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::time::Duration;

/// Max age of the operator-attested ETH spot price (seconds). Older prices
/// refuse (fail closed); P4 replaces attested prices with an oracle feed.
const MAX_PRICE_AGE_SECS: u64 = 300;

#[derive(Debug, Parser)]
#[command(
    name = "live",
    about = "Live execution binary (triple-locked; P3 arms and verifies only)"
)]
struct Args {
    /// Chain profile (`config/sepolia.toml` or `config/mainnet-canary.toml`).
    #[arg(long, default_value = "config/sepolia.toml")]
    config: PathBuf,
    /// Readiness manifest the arm hash covers (required for startup).
    #[arg(long)]
    manifest: Option<PathBuf>,
    /// Write a run-summary JSON into <dir> after arming (auditor input).
    #[arg(long)]
    emit_evidence: Option<PathBuf>,
    /// Operator-attested ETH spot price in USD cents (e.g. 300000 = $3000).
    /// Required for the float-cap check; refused when missing or stale.
    #[arg(long)]
    eth_price_cents: Option<u64>,
    /// Unix seconds the attested price was observed (staleness bound).
    #[arg(long)]
    eth_price_asof: Option<u64>,
    #[command(subcommand)]
    cmd: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Recompute a ledger hash chain (L8): prints PASS + count or the
    /// first broken index.
    LedgerVerify {
        /// Ledger directory holding `ledger.jsonl`.
        #[arg(long)]
        ledger_dir: PathBuf,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    if let Some(Command::LedgerVerify { ledger_dir }) = args.cmd {
        return cmd_ledger_verify(&ledger_dir);
    }
    // Single error funnel (P4): every startup failure leaves through
    // `sanitize_error` with the operator blocklist, so key material pasted
    // into the environment can never echo into logs, metrics, or the
    // returned error (which the shell may print). The blocklist is built
    // ONLY on the failure path (sanitizer-only, zeroized on drop):
    // pre-lock code never reads key material, so pre-lock errors cannot
    // carry it -- the funnel is belt-and-braces for post-lock signer
    // errors. Ledger/breaker errors name fields/paths only by construction
    // (see `live::ledger`).
    run_startup(&args).await.map_err(|e| {
        let op_secrets = SecretBlocklist::from_process_env();
        let secret_refs: Vec<&str> = op_secrets.as_refs();
        anyhow::anyhow!("{}", sanitize_error(&e, &secret_refs))
    })
}

fn cmd_ledger_verify(ledger_dir: &std::path::Path) -> anyhow::Result<()> {
    // Read-only flow: nothing is appended, so no blocklist is needed.
    let ledger = Ledger::open(ledger_dir, Vec::new())?;
    match ledger.verify() {
        Ok(n) => {
            println!(
                "PASS ledger chain intact ({n} records) head={}",
                ledger.head_hash()
            );
            Ok(())
        }
        Err(e) => {
            println!("FAIL {e}");
            anyhow::bail!("ledger verify failed: {e}");
        }
    }
}

async fn run_startup(args: &Args) -> anyhow::Result<()> {
    let window_start = evidence::now_utc_secs();
    let profile = LiveProfile::load(&args.config)?;
    tracing::info!(
        chain = profile.base.chain_id,
        stage_required = %profile.live_section.stage_required,
        config = %args.config.display(),
        "live profile loaded"
    );
    let manifest_path = args.manifest.as_ref().ok_or_else(|| {
        anyhow::anyhow!("refusing to start: --manifest is required (L1 manifest lock)")
    })?;
    let manifest_bytes = std::fs::read(manifest_path)?;
    let manifest: ReadinessManifest = serde_json::from_slice(&manifest_bytes)?;
    let arm = std::env::var("LIVE_ARM").unwrap_or_default();
    let arm_present = !arm.trim().is_empty();
    tracing::info!(
        manifest = %manifest_path.display(),
        arm_present,
        "L1 arm material loaded (match/mismatch only, never values)"
    );

    // Read-only provider (no wallet, no signing): chain id, role
    // read-back, operator balance. Any RPC failure refuses downstream.
    let rpc_chain_id = read_chain_id(&profile.base.rpc_ws_url).await?;
    tracing::info!(rpc_chain_id, "rpc chain id read");

    let lock_inputs = LockInputs {
        profile: &profile,
        manifest: &manifest,
        manifest_bytes: &manifest_bytes,
        live_arm: if arm_present {
            Some(arm.as_str())
        } else {
            None
        },
        embedded_commit: option_env!("BUILD_COMMIT").unwrap_or("unknown"),
        rpc_chain_id,
        now_secs: evidence::now_utc_secs(),
        freeze_bytes: &std::fs::read("docs/FREEZE.md").unwrap_or_default(),
        profile_bytes: &std::fs::read(&args.config).unwrap_or_default(),
        lock_bytes: &std::fs::read("Cargo.lock").unwrap_or_default(),
    };

    // Role read-back (L4) and float-cap inputs resolve BEFORE the sequence
    // so the sequence itself stays strictly ordered (L1 -> roles -> float
    // -> signer last).
    let observed_roles = read_roles_live(&profile).await?;
    let float_balance = read_float_equiv(&profile, args).await?;
    let signer_kind = if profile.base.chain_id == 84532 {
        SignerKind::Env
    } else {
        SignerKind::Remote
    };

    let mut probe = StartupProbe::default();
    startup_sequence(
        &lock_inputs,
        &observed_roles,
        float_balance,
        signer_kind,
        &mut probe,
        || {
            // Signer construction: LAST. Sepolia loads the operator key
            // from the environment; mainnet refuses without a KMS handle
            // (USER-ACTION, P5; env keys never touch mainnet).
            match signer_kind {
                SignerKind::Env => {
                    let signer = EnvKeySigner::from_env(profile.base.chain_id)?;
                    tracing::info!(address = %signer.address(), "operator signer loaded (address only)");
                    Ok(())
                }
                SignerKind::Remote => Err(SignerError::KmsNotConfigured),
            }
        },
    )
    .map_err(|e| anyhow::anyhow!("live lock refused startup: {e}"))?;
    debug_assert!(probe.signer_step_reached);

    // Kill switch must allow arming (fresh checkouts without the flag file
    // refuse: unreadable means stopped).
    if !breaker::kill_switch_allows(&profile.live_section.kill_switch_file) {
        anyhow::bail!(
            "refusing to arm: kill switch engaged (flag file {} missing or not OK)",
            profile.live_section.kill_switch_file.display()
        );
    }
    // Breaker must be untripped (state persists across restarts).
    let breaker_state = Breaker::load(&profile.live_section.ledger_dir, "")?;
    if let Some(reason) = breaker_state.is_tripped() {
        anyhow::bail!("refusing to arm: breaker tripped ({})", reason.as_str());
    }

    // Ledger: record the arming as an operator action (L8), then report.
    // The operator blocklist guards the append boundary: any body field
    // containing the operator value (either paste form) is refused naming
    // the field only (see `live::ledger` + `signer::SecretBlocklist`).
    // Built HERE -- after the lock sequence (L1 -> roles -> float ->
    // signer-last) plus the kill-switch and breaker checks all passed --
    // so no key material enters memory for redaction before L1 passes
    // (the only earlier reader is the signer constructor itself, last in
    // the sequence, which owns its buffers and zeroizes them).
    let op_secrets = SecretBlocklist::from_process_env();
    let mut ledger = Ledger::open(&profile.live_section.ledger_dir, op_secrets.to_vec())?;
    ledger.append(
        base_flash_arb::live::ledger::LedgerKind::OperatorAction,
        serde_json::json!({
            "event": "armed",
            "chain_id": profile.base.chain_id,
            "stage_required": profile.live_section.stage_required,
        }),
    )?;
    tracing::info!(head = %ledger.head_hash(), "ledger armed event persisted");
    println!(
        "ARMED chain={} stage_required={} manifest={} ledger_head={} (no trading loop; runbooks execute)",
        profile.base.chain_id,
        profile.live_section.stage_required,
        sha256_hex(&manifest_bytes),
        ledger.head_hash(),
    );

    if let Some(dir) = args.emit_evidence.as_ref() {
        let summary = EvidenceSummary::live_arming(
            profile.base.chain_id,
            Some(ledger.head_hash().to_string()),
            window_start,
        );
        let path = evidence::write_evidence(dir, &summary)?;
        tracing::info!(evidence = %path.display(), "evidence summary written");
    }
    Ok(())
}

/// Read `eth_chainId` with a timeout. `Err` refuses startup (fail closed).
async fn read_chain_id(rpc_ws_url: &str) -> anyhow::Result<u64> {
    tracing::info!(rpc = %redact_url(rpc_ws_url), "connecting (read-only, redacted)");
    if rpc_ws_url.starts_with("ws://") || rpc_ws_url.starts_with("wss://") {
        let provider = tokio::time::timeout(
            Duration::from_secs(10),
            base_flash_arb::chain::connect_ws(rpc_ws_url),
        )
        .await
        .map_err(|_| anyhow::anyhow!("rpc connect timed out"))??;
        return tokio::time::timeout(Duration::from_secs(10), provider.get_chain_id())
            .await
            .map_err(|_| anyhow::anyhow!("eth_chainId timed out"))?
            .map_err(|e| anyhow::anyhow!("eth_chainId failed: {e:#}"));
    }
    if rpc_ws_url.starts_with("http://") || rpc_ws_url.starts_with("https://") {
        let url: reqwest::Url = rpc_ws_url.parse()?;
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(url);
        return tokio::time::timeout(Duration::from_secs(10), provider.get_chain_id())
            .await
            .map_err(|_| anyhow::anyhow!("eth_chainId timed out"))?
            .map_err(|e| anyhow::anyhow!("eth_chainId failed: {e:#}"));
    }
    anyhow::bail!("refusing to start: rpc_ws_url must be ws(s):// or http(s)://")
}

/// L4 role read-back over the profile RPC endpoint.
async fn read_roles_live(profile: &LiveProfile) -> anyhow::Result<RoleMatrix> {
    let url = &profile.base.rpc_ws_url;
    if url.starts_with("ws://") || url.starts_with("wss://") {
        let provider = tokio::time::timeout(
            Duration::from_secs(10),
            base_flash_arb::chain::connect_ws(url),
        )
        .await
        .map_err(|_| anyhow::anyhow!("rpc connect timed out"))??;
        return read_roles(&provider, profile.live_section.executor)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let parsed: reqwest::Url = url.parse()?;
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(parsed);
        return read_roles(&provider, profile.live_section.executor)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"));
    }
    anyhow::bail!("refusing to start: rpc_ws_url must be ws(s):// or http(s)://")
}

/// Operator float in USD-equiv base units: measured native balance via RPC
/// times the operator-attested spot price (staleness-bounded). Missing or
/// stale price refuses (fail closed); P4 replaces attestation with an
/// oracle feed.
async fn read_float_equiv(profile: &LiveProfile, args: &Args) -> anyhow::Result<Option<u64>> {
    let (price_cents, asof) = match (args.eth_price_cents, args.eth_price_asof) {
        (Some(p), Some(t)) => (p, t),
        _ => anyhow::bail!(
            "refusing to start: --eth-price-cents and --eth-price-asof are required (float-cap is fail-closed)"
        ),
    };
    let now = evidence::now_utc_secs();
    if asof > now || now - asof > MAX_PRICE_AGE_SECS {
        anyhow::bail!("refusing to start: attested ETH price is stale (older than 300 s)");
    }
    let url = &profile.base.rpc_ws_url;
    let balance_wei: u128 = if url.starts_with("ws://") || url.starts_with("wss://") {
        let provider = tokio::time::timeout(
            Duration::from_secs(10),
            base_flash_arb::chain::connect_ws(url),
        )
        .await
        .map_err(|_| anyhow::anyhow!("rpc connect timed out"))??;
        tokio::time::timeout(
            Duration::from_secs(10),
            provider.get_balance(profile.live_section.operator),
        )
        .await
        .map_err(|_| anyhow::anyhow!("operator balance timed out"))?
        .map_err(|e| anyhow::anyhow!("operator balance failed: {e:#}"))?
        .to::<u128>()
    } else if url.starts_with("http://") || url.starts_with("https://") {
        let parsed: reqwest::Url = url.parse()?;
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(parsed);
        tokio::time::timeout(
            Duration::from_secs(10),
            provider.get_balance(profile.live_section.operator),
        )
        .await
        .map_err(|_| anyhow::anyhow!("operator balance timed out"))?
        .map_err(|e| anyhow::anyhow!("operator balance failed: {e:#}"))?
        .to::<u128>()
    } else {
        anyhow::bail!("refusing to start: rpc_ws_url must be ws(s):// or http(s)://")
    };
    Ok(Some(eth_wei_to_usdc_base(balance_wei, price_cents)))
}
