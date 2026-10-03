//! Discord ops bot: SEPARATE process from the trading hot path.
//!
//! - Holds the PAUSER key ONLY (`PAUSER_KEY` env). NEVER owner/operator keys:
//!   startup refuses when `OWNER_KEY` or `OPERATOR_KEY` is present.
//! - No fund-moving commands exist here. `resume` (unpause) is intentionally
//!   NOT a subcommand: resume is an on-host, owner-signed action per
//!   `docs/RUNBOOK_SEPOLIA.md`. Typing `resume` prints a refusal.
//! - Full serenity gateway bot is a drop-in later; this CLI covers drills now
//!   (`status`, `alerts`, `pause --confirmed`, `test-alert`) with the same
//!   role gate as `src/discord.rs`.
//!
//! Secrets: `PAUSER_KEY` and `DISCORD_WEBHOOK_URL` come ONLY from the
//! environment, are never logged, and never written to files. Audit lines
//! contain role/command/result only.

use anyhow::Context;
use base_flash_arb::alerts::{OpportunityAlert, WebhookSender, format_discord_message};
use base_flash_arb::discord;
use clap::{Parser, Subcommand};
use std::io::Write as _;

/// Role from `DISCORD_ROLE` env (viewer|operator|admin, default viewer).
fn parse_role() -> discord::Role {
    match std::env::var("DISCORD_ROLE")
        .unwrap_or_else(|_| "viewer".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "operator" => discord::Role::Operator,
        "admin" => discord::Role::Admin,
        _ => discord::Role::Viewer,
    }
}

fn role_label(r: discord::Role) -> &'static str {
    match r {
        discord::Role::Viewer => "viewer",
        discord::Role::Operator => "operator",
        discord::Role::Admin => "admin",
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "discord-bot",
    about = "Discord ops CLI (pauser key only; separate process from trading hot path)",
    long_about = "Discord ops CLI (pauser key only).\n\
        Commands: status | alerts | pause --confirmed | test-alert.\n\
        `resume` is intentionally ABSENT: unpause is an on-host, owner-signed\n\
        action per docs/RUNBOOK_SEPOLIA.md, never a chat/CLI command here."
)]
struct Args {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read-only status (any role). No keys needed.
    Status,
    /// Read-only recent alerts placeholder (any role). No keys needed.
    Alerts,
    /// On-chain pause() with the PAUSER key. Requires DISCORD_ROLE=operator|admin
    /// plus --confirmed. Never unpauses, never moves funds.
    Pause {
        /// Explicit confirmation (required).
        #[arg(long, default_value_t = false)]
        confirmed: bool,
    },
    /// Format + deliver one sample alert (dry-run safe, no keys needed).
    /// Uses DISCORD_WEBHOOK_URL when set, else stdout fallback.
    TestAlert,
}

/// Refuse to run when owner/operator keys are visible in this process.
fn guard_key_custody() -> anyhow::Result<()> {
    for var in ["OWNER_KEY", "OPERATOR_KEY"] {
        if let Ok(v) = std::env::var(var)
            && !v.trim().is_empty()
        {
            anyhow::bail!(
                "refusing to start: {var} is set in the Discord process. \
                 This process holds the PAUSER key only; unset {var} and retry."
            );
        }
    }
    Ok(())
}

/// Append an audit line (stdout + best-effort CSV). Audit contains
/// role/command/result only -- never keys or URLs.
fn audit(user: &str, role: discord::Role, command: &str, result: &str) {
    let line = format!("{user},{},{command},{result}", role_label(role));
    println!("AUDIT {line}");
    let path = std::env::var("AUDIT_CSV").unwrap_or_else(|_| "discord-audit.csv".to_string());
    // Best-effort: audit must never crash the CLI.
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{ts},{line}");
    }
}

fn cmd_status(role: discord::Role, user: &str) -> anyhow::Result<()> {
    let status = "discord-bot: dry-run, pauser-only, separate hot path. Default: read-only mode.";
    match discord::handle_command(discord::ControlCommand::Status, role, user, false, status) {
        Ok((out, entry)) => {
            println!("STATUS OK ({role:?}): {out:?}");
            audit(user, role, "status", &format!("{out:?}"));
            let _ = entry;
            Ok(())
        }
        Err(e) => {
            audit(user, role, "status", &format!("ERR {e}"));
            Err(e.into())
        }
    }
}

fn cmd_alerts(role: discord::Role, user: &str) -> anyhow::Result<()> {
    // Read-only placeholder: the live feed tails the alert store; drills use
    // `test-alert` to verify delivery end-to-end (stdout fallback offline).
    let status = "no new alerts (paper/dry-run). Run `test-alert` to test delivery.";
    match discord::handle_command(discord::ControlCommand::Alerts, role, user, false, status) {
        Ok((out, _)) => {
            println!("ALERTS ({role:?}): {out:?}");
            audit(user, role, "alerts", "ok");
            Ok(())
        }
        Err(e) => {
            audit(user, role, "alerts", &format!("ERR {e}"));
            Err(e.into())
        }
    }
}

async fn cmd_test_alert(role: discord::Role, user: &str) -> anyhow::Result<()> {
    let sample = OpportunityAlert {
        pair: "WETH/USDC".to_string(),
        direction: "Slipstream->UniV3".to_string(),
        spread_bps: 200,
        size_usdc: 500_000_000,
        net_usdc: 5_120_000,
        fee_pp: "slip 3bps + v3 5bps".to_string(),
        tx_link: "paper-dry-run:no-tx".to_string(),
    };
    let msg = format_discord_message(&sample);
    let mut sender = WebhookSender::from_env();
    // Never log the URL itself; label only.
    println!("endpoint: {}", sender.endpoint_label());
    sender.send(&msg).await;
    println!("test-alert sent (or stdout fallback when URL is empty).");
    audit(user, role, "test-alert", "ok");
    Ok(())
}

async fn cmd_pause(role: discord::Role, user: &str, confirmed: bool) -> anyhow::Result<()> {
    // Pure gate first (role + confirmation), identical rules to src/discord.rs.
    let gate = discord::handle_command(
        discord::ControlCommand::Pause { confirmed },
        role,
        user,
        false,
        "pause",
    );
    // Discord allowlist: Pause must be exposed; Resume must not be.
    debug_assert!(discord::discord_exposed(&discord::ControlCommand::Pause {
        confirmed: true
    }));
    debug_assert!(!discord::discord_exposed(
        &discord::ControlCommand::Resume { confirmed: true }
    ));
    match gate {
        Err(discord::ControlError::Unauthorized) => {
            let msg = "denied: pause requires DISCORD_ROLE=operator|admin.";
            println!("{msg}");
            audit(user, role, "pause", "ERR Unauthorized");
            anyhow::bail!("{msg}");
        }
        Err(discord::ControlError::ConfirmationRequired) => {
            let msg = "denied: retry with --confirmed (explicit confirmation required).";
            println!("{msg}");
            audit(user, role, "pause", "ERR ConfirmationRequired");
            anyhow::bail!("{msg}");
        }
        Err(e) => {
            audit(user, role, "pause", &format!("ERR {e}"));
            anyhow::bail!("pause gate: {e}");
        }
        Ok(_) => {}
    }

    let pauser_key = std::env::var("PAUSER_KEY").unwrap_or_default();
    if pauser_key.trim().is_empty() {
        let msg = "PAUSER_KEY is empty: set PAUSER_KEY (pauser key ONLY) in the environment for the pause drill.";
        println!("{msg}");
        audit(user, role, "pause", "ERR missing-pauser-key");
        anyhow::bail!("{msg}");
    }
    let executor: String = std::env::var("EXECUTOR_ADDRESS").unwrap_or_default();
    if executor.trim().is_empty() {
        let msg = "EXECUTOR_ADDRESS is empty: set the drill executor address (Sepolia) in the environment.";
        println!("{msg}");
        audit(user, role, "pause", "ERR missing-executor");
        anyhow::bail!("{msg}");
    }
    let rpc = std::env::var("RPC_URL")
        .or_else(|_| std::env::var("BASE_SEPOLIA_RPC_URL"))
        .unwrap_or_default();
    if rpc.trim().is_empty() {
        let msg = "RPC_URL is empty: set RPC_URL (or BASE_SEPOLIA_RPC_URL) for the pause drill.";
        println!("{msg}");
        audit(user, role, "pause", "ERR missing-rpc");
        anyhow::bail!("{msg}");
    }

    pause_onchain(&pauser_key, &executor, &rpc)
        .await
        .context("on-chain pause() failed")?;
    println!("pause() sent (pauser key). Verify `paused == true` via cast call.");
    audit(user, role, "pause", "ok pause-sent");
    Ok(())
}

/// Minimal executor binding: `pause()` only. Binding anything else here
/// (unpause/sweep/allowlist) is FORBIDDEN by review.
mod executor_abi {
    alloy::sol! {
        #[sol(rpc)]
        contract FlashArbExecutor {
            function pause() external;
        }
    }
}

/// Send `pause()` signed by the pauser key. Keys/URLs are never logged.
async fn pause_onchain(pauser_key: &str, executor: &str, rpc_url: &str) -> anyhow::Result<()> {
    use alloy::network::EthereumWallet;
    use alloy::providers::ProviderBuilder;
    use alloy::signers::local::PrivateKeySigner;

    let signer: PrivateKeySigner = pauser_key
        .trim()
        .parse()
        .context("invalid PAUSER_KEY (not a hex private key)")?;
    let wallet = EthereumWallet::from(signer);
    let url: reqwest::Url = rpc_url.trim().parse().context("invalid RPC_URL")?;
    let provider = ProviderBuilder::new().wallet(wallet).connect_http(url);
    let to: alloy::primitives::Address = executor
        .trim()
        .parse()
        .context("invalid EXECUTOR_ADDRESS")?;
    let contract = executor_abi::FlashArbExecutor::new(to, provider);
    // Gas/fee fields follow the provider fillers; reverted tx still costs
    // gas, so this stays a conscious human drill step (never auto-retried).
    let pending = contract.pause().send().await?;
    // Never log the hash-adjacent secret material; tx hash itself is public
    // once broadcast, but keep output minimal for drill copy-paste.
    tracing::info!("pause() broadcast, waiting for receipt");
    let receipt = pending.get_receipt().await?;
    println!("receipt status: {}", u64::from(receipt.status()));
    anyhow::ensure!(receipt.status(), "pause() receipt status == 0 (revert)");
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    // Friendly refusal for the forbidden subcommand (clap has no `resume`
    // variant on purpose; catch it before parsing so the runbook pointer
    // always prints).
    if std::env::args().any(|a| a.eq_ignore_ascii_case("resume")) {
        eprintln!(
            "DENIED: `resume`/unpause is not a Discord command.\n\
             Resume is an on-host, OWNER-signed action at a terminal,\n\
             never a chat command. See docs/RUNBOOK_SEPOLIA.md §4 + §6."
        );
        std::process::exit(2);
    }
    guard_key_custody()?;
    let args = Args::parse();
    let role = parse_role();
    let user = std::env::var("DISCORD_USER").unwrap_or_else(|_| "cli".to_string());
    match args.cmd {
        Command::Status => cmd_status(role, &user),
        Command::Alerts => cmd_alerts(role, &user),
        Command::TestAlert => cmd_test_alert(role, &user).await,
        Command::Pause { confirmed } => cmd_pause(role, &user, confirmed).await,
    }
}
