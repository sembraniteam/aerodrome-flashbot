//! Paper-trading binary: simulate-only, dry-run enforced.
//!
//! - Never needs a private key, never signs, never broadcasts.
//! - Best-effort WS probe (redacted logs only), then evaluates the
//!   allowlisted pairs with the off-chain estimator over a deterministic
//!   fixture skew, gates every candidate through
//!   [`base_flash_arb::risk`], verifies via
//!   [`base_flash_arb::sim`], and logs metrics to stdout + CSV.
//! - Fork mode: run `anvil --fork-url <PUBLIC_BASE_RPC>` first, then point
//!   `--config` at a TOML whose `rpc_ws_url` is the local anvil WS endpoint.
//!   See README "Fork test" section. No key is required for `eth_call` reads.

use base_flash_arb::{
    alerts, chain, config, discord, fork_check, metrics, pools, profit, risk, sequencer, sim,
};

use alloy::primitives::U256;
use alloy::sol_types::SolCall;
use clap::Parser;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chain::BlockFeed as _;
use config::{BotConfig, FeedMode, addresses};
use metrics::Metrics;
use pools::{ClPoolState, PoolKind, resolve_fee};
use profit::{CostModel, breakeven_size, evaluate_both_directions, optimal_size};
use risk::{RiskError, RiskLimits, RiskState};

#[derive(Debug, Parser)]
#[command(
    name = "paper",
    about = "Dry-run paper-trading harness (simulate only)"
)]
struct Args {
    /// Path to TOML config (default: config/default.toml).
    #[arg(long, default_value = "config/default.toml")]
    config: PathBuf,
    /// Optional CSV output path (default: stdout-adjacent results.csv).
    #[arg(long, default_value = "results.csv")]
    csv: PathBuf,
    /// Optional fork/anvil URL: log label for paper mode (redacted), endpoint
    /// for `--fork-check` (overrides `$FORK_URL`; `ws(s)://` normalized to
    /// `http(s)://`).
    #[arg(long, default_value = "")]
    fork_url: String,
    /// Quote-divergence tolerance in bps for sim verification.
    #[arg(long, default_value_t = 50)]
    tolerance_bps: u64,
    /// Engage the manual kill switch before evaluating (demonstrates the
    /// fail-closed stop path: every trade is then rejected as Killed).
    #[arg(long, default_value_t = false)]
    kill: bool,
    /// Read-only fork check: real QuoterV2 vs off-chain estimator on a Base
    /// fork (no keys, no signing, no broadcast). Skips gracefully if no RPC
    /// is reachable. Endpoint: `--fork-url` > `$FORK_URL` > local default.
    #[arg(long, default_value_t = false)]
    fork_check: bool,
    /// Read-only fork matrix (No.4): quoter table plus Vault fee>0 path,
    /// sandwich drill, deadlines, transient note, L1 getL1Fee per size,
    /// codehash freeze rows, sequencer gate. Skips gracefully offline.
    /// Endpoint resolution identical to `--fork-check`.
    #[arg(long, default_value_t = false)]
    fork_matrix: bool,
    /// Send a Discord alert for each simulated WIN (best-effort, never breaks
    /// the paper loop). Default OFF. Requires DISCORD_WEBHOOK_URL in the
    /// environment; empty URL = stdout fallback inside the sender.
    #[arg(long, default_value_t = false)]
    discord_alerts: bool,
    /// Measured L1 data fee in profit-token base units (from `getL1Fee` on
    /// the OP-Stack GasPriceOracle). Overrides the offline fixture default
    /// (`profit::FIXTURE_L1_DATA_FEE_USDC`); the source (fixture vs measured)
    /// is logged. Lets fork-measured values flow into the cost model without
    /// any network call inside the paper binary.
    #[arg(long)]
    l1_fee: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    let cfg = BotConfig::load(&args.config)?;
    if args.fork_check {
        // Read-only mode: real quoter vs estimator. No keys, no broadcast.
        // Skips gracefully (exit 0) when no RPC is reachable.
        return fork_check::run_fork_check(&args.fork_url, args.tolerance_bps).await;
    }
    if args.fork_matrix {
        // Read-only mode: quoter table + No.4 matrix. No keys, no broadcast.
        // Skips gracefully (exit 0) when no RPC is reachable.
        return fork_check::run_fork_matrix(&args.fork_url, args.tolerance_bps).await;
    }
    probe_ws(&cfg, &args).await;
    log_feed_selection(&cfg).await;
    run_paper(&args, &cfg).await
}

/// Best-effort WS probe: proves the provider wiring compiles and the
/// endpoint is reachable; failures only warn (paper continues on fixtures).
/// The URL is always redacted in logs.
async fn probe_ws(cfg: &BotConfig, args: &Args) {
    let url = if args.fork_url.is_empty() {
        cfg.rpc_ws_url.clone()
    } else {
        args.fork_url.clone()
    };
    tracing::info!(rpc = %chain::redact_url(&url), "probing WS provider (best effort)");
    match tokio::time::timeout(Duration::from_secs(3), chain::connect_ws(&url)).await {
        Ok(Ok(_)) => tracing::info!("WS provider reachable"),
        Ok(Err(e)) => tracing::warn!("WS provider unreachable, continuing on fixtures: {e:#}"),
        Err(_) => tracing::warn!("WS probe timed out, continuing on fixtures"),
    }
}

async fn run_paper(args: &Args, cfg: &BotConfig) -> anyhow::Result<()> {
    // PAPER MODE: force dry-run regardless of file content, loudly.
    if !cfg.dry_run {
        tracing::warn!("config dry_run=false ignored: paper binary forces dry-run");
    }
    log_address_registry();
    // Discord alerts: opt-in only (--discord-alerts). URL comes solely from
    // DISCORD_WEBHOOK_URL; empty URL = stdout fallback. Sender is
    // best-effort: failures/rate-limits only warn, never break the loop.
    let mut alerter = if args.discord_alerts {
        let s = alerts::WebhookSender::from_env();
        tracing::info!(
            endpoint = s.endpoint_label(),
            "discord alerts armed (best-effort)"
        );
        Some(s)
    } else {
        None
    };

    let limits = RiskLimits {
        max_flash: cfg.max_flash_usdc,
        min_net_profit: cfg.min_net_profit_usdc,
        max_slippage_bps: cfg.max_slippage_bps,
        daily_loss_cap: cfg.daily_loss_cap_usdc,
        max_consecutive_failures: cfg.max_consecutive_failures,
        max_in_flight: cfg.max_in_flight,
        hard_max_flash: risk::HARD_MAX_FLASH_USDC,
        ..RiskLimits::default()
    };
    tracing::info!(
        max_flash = limits.max_flash,
        max_loss_per_trade = limits.max_loss_per_trade,
        daily_loss_cap = limits.daily_loss_cap,
        max_in_flight = limits.max_in_flight,
        deadline_secs = cfg.deadline_secs,
        private_relay = !cfg.private_rpc_url.as_deref().unwrap_or("").is_empty(),
        hard_max_flash = limits.hard_max_flash,
        "risk limits armed (dry-run)"
    );
    let mut risk = RiskState::new(limits);
    // Live head when the feed yields one, else the deterministic fixture
    // head (offline). Either way the number flows into `observe_head` and
    // every `check_trade` call below -- no hardcoded head constants.
    let (head_number, head_source) =
        match chain::poll_head_once(cfg.feed_mode, &cfg.rpc_ws_url).await {
            Some(event) => (event.number, "live"),
            None => (FIXTURE_HEAD, "fixture"),
        };
    risk.observe_head(head_number);
    tracing::info!(
        head = head_number,
        source = head_source,
        feed_mode = ?cfg.feed_mode,
        "head observed (live feed or fixture fallback)"
    );
    if args.kill {
        risk.kill();
        tracing::warn!("manual kill switch engaged: all trades will be rejected");
    }

    // Control-plane stub: startup status line through the role gate, plus
    // a gate-matrix demo proving Viewer cannot mutate and confirmation is
    // required (audited, no secrets involved).
    let status = Metrics::new().summary();
    match discord::handle_command(
        discord::ControlCommand::Status,
        discord::Role::Viewer,
        "paper-runner",
        false,
        &status,
    ) {
        Ok((out, audit)) => tracing::info!("control-plane: {out:?} audit={audit:?}"),
        Err(e) => tracing::warn!("control-plane stub error: {e}"),
    }
    demo_control_plane();

    let mut metrics = Metrics::new();
    metrics.record_feed_lag_ms(0); // paper: no live feed; live runner samples here.
    // L1 data-fee term: fixture default keeps offline runs deterministic;
    // `--l1-fee` injects a fork-measured `getL1Fee` value. The source is
    // logged so fixture vs measured is never ambiguous.
    let mut costs = CostModel::paper_fixture();
    let l1_source = match args.l1_fee {
        Some(measured) => {
            costs.set_l1_fee(U256::from(measured));
            "measured(--l1-fee)"
        }
        None => "fixture",
    };
    tracing::info!(
        l1_data_fee = %costs.l1_data_fee,
        source = l1_source,
        "L1 data-fee term (use --l1-fee to inject a measured getL1Fee value)"
    );
    let l1_demo = chain::l1_fee_calldata(&[0xde, 0xad, 0xbe, 0xef]);
    tracing::info!(
        oracle = chain::L1_GAS_PRICE_ORACLE,
        calldata_len = l1_demo.len(),
        "L1 data-fee oracle binding ready (getL1Fee via eth_call)"
    );
    let min_net = U256::from(cfg.min_net_profit_usdc);
    let max_flash = U256::from(cfg.max_flash_usdc);

    for pair in cfg.pairs.iter().filter(|p| p.enabled) {
        // Exercise the allowlist lookup helper too.
        debug_assert!(cfg.pair(&pair.name).is_some());
        metrics.record_seen();
        let t0 = Instant::now();
        tracing::info!(
            pair = %pair.name,
            token_a = %pair.token_a,
            token_b = %pair.token_b,
            decimals = ?(pair.decimals_a, pair.decimals_b),
            "evaluating allowlisted pair"
        );
        let (leg_a, leg_b) = fixture_legs();
        // Paper fixture skews leg B +2% vs leg A so the harness exercises
        // the profitable branch deterministically (fork data replaces this).
        let leg_b = skewed_clone(&leg_b);
        tracing::debug!("leg_a={leg_a:?} leg_b={leg_b:?}");

        match breakeven_size(&leg_a, &leg_b, max_flash, true, &costs, min_net) {
            Some(s) => tracing::info!(pair = %pair.name, breakeven = %s, "break-even size"),
            None => tracing::info!(pair = %pair.name, "no break-even size at any grid point"),
        }

        let Some((size, net)) = optimal_size(&leg_a, &leg_b, max_flash, true, &costs, min_net)
        else {
            tracing::info!(pair = %pair.name, "no clearing size (below min net)");
            metrics.record_risk_reject();
            metrics.record_pair_seen(&pair.name, "-");
            continue;
        };
        // Full two-direction decision for the chosen size (exercises the
        // ProfitError path and the profitability predicate). The best
        // direction also labels the Discord alert below.
        let (best_dir, dir_label) =
            match evaluate_both_directions(&leg_a, &leg_b, size, true, &costs, min_net) {
                Ok(d) => {
                    tracing::info!(
                        pair = %pair.name,
                        best = ?d.best,
                        other_net = ?d.other.net,
                        profitable = d.is_profitable(),
                        "two-direction decision"
                    );
                    let label = if d.best.forward { "A->B->A" } else { "B->A->B" };
                    (format!("{:?}", d.best), label.to_string())
                }
                Err(e) => {
                    tracing::info!(pair = %pair.name, error = %e, "direction harness rejected");
                    metrics.record_risk_reject();
                    metrics.record_pair_seen(&pair.name, "-");
                    continue;
                }
            };
        let size_u64: u64 = size.try_into().unwrap_or(u64::MAX);
        let net_u64: u64 = net.try_into().unwrap_or(u64::MAX);

        // Risk gate (allowlist membership = enabled list). Slippage, head and
        // staleness come from config / the observed head -- no magic
        // constants. Paper assumes the worst-case tolerated slippage; the
        // live path measures it per quote.
        match risk.check_trade(
            &pair.name,
            true,
            size_u64,
            Some(net_u64),
            cfg.max_slippage_bps,
            Some(head_number),
            cfg.head_staleness_blocks,
        ) {
            Ok(()) => {
                risk.track_submit();
                // Dry-run "simulation": paper constructs a consistent
                // gross so offchain==onchain; fork runs replace onchain
                // with the real eth_call quote and verify via
                // sim::verify_quote.
                let gross = size.saturating_add(costs.flat_costs()).saturating_add(net);
                let sim_res = sim::SimulationResult {
                    onchain_out: gross,
                    offchain_out: gross,
                    gas_estimate: 250_000,
                    would_revert: false,
                };
                tracing::debug!("sim={sim_res:?}");
                let gas_limit = sim::gas_limit_with_margin(sim_res.gas_estimate, 2000, 1_000_000);
                tracing::debug!(gas_limit, "gas limit with margin");
                match sim::decide_from_simulation(
                    &sim_res,
                    size,
                    costs.flat_costs(),
                    min_net,
                    args.tolerance_bps,
                ) {
                    sim::SimDecision::Submit { net } => {
                        tracing::info!(
                            pair = %pair.name,
                            size = %size,
                            net = %net,
                            "PAPER OPPORTUNITY (not submitted)"
                        );
                        metrics.record_simulated(true, false, 0, 0);
                        metrics.record_pair_sim(
                            &pair.name,
                            &dir_label,
                            true,
                            net.try_into().unwrap_or(i128::MAX),
                        );
                        risk.record_result(0, false); // paper: no PnL movement
                        // Best-effort Discord alert: failures/rate-limits are
                        // swallowed inside the sender; never break the loop.
                        if let Some(sender) = alerter.as_mut() {
                            let size_u64: u64 = size.try_into().unwrap_or(u64::MAX);
                            let net_u64: u64 = net.try_into().unwrap_or(u64::MAX);
                            // Fixture skew is +2% by construction (see
                            // skewed_clone above); live path replaces this
                            // with the measured quote spread.
                            let fee_label = format!(
                                "slip {}bps + v3 {}bps",
                                leg_a.fee_ppm / 100,
                                leg_b.fee_ppm / 100
                            );
                            let alert = alerts::OpportunityAlert {
                                pair: pair.name.clone(),
                                direction: best_dir.clone(),
                                spread_bps: 200,
                                size_usdc: size_u64,
                                net_usdc: net_u64,
                                fee_pp: fee_label,
                                tx_link: "paper-dry-run:no-tx".to_string(),
                            };
                            let msg = alerts::format_discord_message(&alert);
                            sender.send(&msg).await;
                        }
                    }
                    sim::SimDecision::Reject { reason } => {
                        tracing::info!(pair = %pair.name, %reason, "rejected by sim");
                        if reason == sim::RejectReason::QuoteDiverged {
                            metrics.record_diverged();
                        }
                        metrics.record_simulated(false, false, 0, 0);
                        metrics.record_pair_sim(&pair.name, &dir_label, false, 0);
                        metrics.record_risk_reject();
                        risk.record_result(0, false);
                    }
                }
                risk.track_settle();
            }
            Err(RiskError::BelowMinProfit(..)) => {
                tracing::info!(pair = %pair.name, "rejected: below min net");
                metrics.record_risk_reject();
                metrics.record_pair_seen(&pair.name, &dir_label);
            }
            Err(e) => {
                tracing::info!(pair = %pair.name, error = %e, "rejected by risk");
                metrics.record_risk_reject();
                metrics.record_pair_seen(&pair.name, &dir_label);
            }
        }
        metrics.record_stage_latency("evaluate", t0.elapsed().as_micros() as u64);
    }

    tracing::info!("{}", metrics.summary());
    println!("{}", metrics.summary());
    println!(
        "p95_evaluate_us={:.1} hit_rate={:.3} revert_rate={:.3}",
        metrics.p95_latency_us("evaluate"),
        metrics.hit_rate(),
        metrics.revert_rate()
    );

    // Flexible widths: the summary row (9 fields) and the breakdown section
    // (6 fields) share one file.
    let file = std::fs::File::create(&args.csv)?;
    let mut w = csv::WriterBuilder::new().flexible(true).from_writer(file);
    w.write_record([
        "opportunities_seen",
        "attempts_simulated",
        "attempts_won",
        "hit_rate",
        "revert_rate",
        "rejected_by_risk",
        "diverged",
        "net_pnl",
        "fee_spend",
    ])?;
    metrics.write_csv_row(&mut w)?;
    // Per-pair/direction breakdown section after the summary row (see
    // `Metrics::write_csv_breakdown`).
    metrics.write_csv_breakdown(&mut w)?;
    tracing::info!(csv = %args.csv.display(), "metrics written");
    Ok(())
}

/// Deterministic fixture head used when the live feed yields nothing
/// (offline paper). Logged explicitly as `source = "fixture"`.
const FIXTURE_HEAD: u64 = 1;

/// Startup log of every verified address this build depends on.
fn log_address_registry() {
    use addresses::*;
    tracing::info!(
        balancer_vault = %BALANCER_VAULT,
        aero_factory = %AERO_SLIPSTREAM_FACTORY_V1,
        aero_quoter = %AERO_QUOTER_V2,
        aero_router = %AERO_SWAP_ROUTER,
        univ3_factory = %UNIV3_FACTORY_BASE,
        univ3_quoter = %UNIV3_QUOTER_V2,
        univ3_router02 = %UNIV3_SWAP_ROUTER02,
        universal_router = %UNIV3_UNIVERSAL_ROUTER,
        usdc = %USDC_NATIVE,
        weth = %WETH,
        aero = %AERO,
        sequencer_feed = %SEQUENCER_UPTIME_FEED_BASE,
        chain_id = BASE_CHAIN_ID,
        "address registry (Base mainnet, verified)"
    );
    // Touch the remaining contract bindings so ABI drift breaks the build
    // here, loudly, instead of at 3am in the live runner.
    tracing::debug!(
        slot0_selector = ?pools::IClPool::slot0Call::SELECTOR,
        flash_loan_sig = pools::IBalancerVault::flashLoanCall::SIGNATURE,
        sequencer_feed = %sequencer::SEQUENCER_FEED_BASE_MAINNET,
        sequencer_grace_secs = sequencer::GRACE_PERIOD_SECS,
        "contract bindings linked"
    );
    // Paper has no live sequencer snapshot: demonstrate the fail-closed
    // default (unknown status refuses) so the gate is exercised every run.
    debug_assert!(
        !sequencer::should_send_execute(None, sequencer::GRACE_PERIOD_SECS),
        "unknown sequencer status must refuse"
    );
}

/// Pick the block-feed implementation from config (never a literal
/// `pending` subscription scattered at call sites) and demonstrate the
/// reorg + millisecond-timestamp helpers on a fixture head.
async fn log_feed_selection(cfg: &BotConfig) {
    // One best-effort live poll; None = offline/unresponsive/stub (the paper
    // loop falls back to the fixture head and says so).
    let live_head = chain::poll_head_once(cfg.feed_mode, &cfg.rpc_ws_url).await;
    match cfg.feed_mode {
        FeedMode::Flashblocks => {
            tracing::info!(
                live_head = ?live_head.as_ref().map(|h| h.number),
                "feed mode: flashblocks (pre-Denim pending-tag preconfirmations; stub yields None)"
            );
        }
        FeedMode::Canonical => {
            tracing::info!(
                live_head = ?live_head.as_ref().map(|h| h.number),
                "feed mode: canonical live newHeads (post-Denim default)"
            );
        }
    }
    let prev = chain::HeadEvent {
        number: 41,
        hash: alloy::primitives::B256::repeat_byte(1),
        timestamp_ms: chain::timestamp_ms(1_700_000_000, None),
        is_preconfirmation: false,
    };
    let next = chain::HeadEvent {
        number: 42,
        hash: alloy::primitives::B256::repeat_byte(2),
        timestamp_ms: chain::timestamp_ms(1_700_000_000, Some(1_700_000_000_200)),
        is_preconfirmation: cfg.feed_mode == FeedMode::Flashblocks,
    };
    tracing::debug!(
        reorged = chain::FlashblocksFeed::reorged(&prev, &next),
        number = next.number,
        hash = %next.hash,
        ts_ms = ?next.timestamp_ms,
        preconf = next.is_preconfirmation,
        "feed helpers linked"
    );
}

/// Startup self-test of the control-plane role gate: every state-changing
/// command, every role, confirmed and unconfirmed. Read-only commands pass
/// for Viewers; mutations without role or confirmation are refused; nothing
/// here can move funds or raise limits (no such command exists).
fn demo_control_plane() {
    use discord::{ControlCommand::*, Role::*};
    let seq = [
        (Alerts, Viewer, "viewer", false),
        (Pause { confirmed: false }, Operator, "op", false),
        (Pause { confirmed: true }, Operator, "op", false),
        (Resume { confirmed: true }, Admin, "adm", false),
        (Pause { confirmed: true }, Viewer, "mallory", false),
    ];
    for (cmd, role, user, killed) in seq {
        // Transport allowlist: only `discord_exposed` commands may ever be
        // wired to the Discord bot process (pauser key only, never owner).
        let exposed = discord::discord_exposed(&cmd);
        match discord::handle_command(cmd, role, user, killed, "paper") {
            Ok((out, audit)) => {
                tracing::info!("control demo ok: {out:?} audit={audit:?} discord_exposed={exposed}")
            }
            Err(e) => tracing::info!("control demo gate refused: {e} discord_exposed={exposed}"),
        }
    }
}

/// Deterministic fixture legs priced at parity 1:1. Fees come from the real
/// static maps via [`resolve_fee`] (live path overrides with `fee()`).
/// Real pool state (slot0/liquidity/tick/fee) comes from fork `eth_call` in
/// the live/sim path; fixtures exist only so `cargo test` and the paper
/// binary run with zero network.
fn fixture_legs() -> (ClPoolState, ClPoolState) {
    // sqrtPriceX96 for price 1.0 = 2^96.
    let sqrt = U256::from(1u128 << 96);
    let fee_slip = resolve_fee(PoolKind::Slipstream, 50, None).expect("known spacing");
    let fee_v3 = resolve_fee(PoolKind::UniV3, 10, None).expect("known spacing");
    debug_assert!(
        pools::UNIV3_FEE_TIERS_PPM.contains(&fee_v3),
        "fixture fee is a real tier"
    );
    let a = ClPoolState::new(
        addresses::AERO_QUOTER_V2,
        PoolKind::Slipstream,
        sqrt,
        10_000_000_000_000_000_000,
        0,
        fee_slip,
        50,
    )
    .expect("fixture state valid");
    let b = ClPoolState::new(
        addresses::UNIV3_QUOTER_V2,
        PoolKind::UniV3,
        sqrt,
        10_000_000_000_000_000_000,
        0,
        fee_v3,
        10,
    )
    .expect("fixture state valid");
    (a, b)
}

/// Scale a leg's implied price by +2% (fixture-only helper; the live
/// path never invents prices). Integer `U256` math over the full 256 bits
/// (`sqrt * 102 / 100`, saturating): no floats, no low-128-bit truncation.
fn skewed_clone(state: &ClPoolState) -> ClPoolState {
    let mut out = state.clone();
    out.sqrt_price_x96 =
        state.sqrt_price_x96.saturating_mul(U256::from(102u64)) / U256::from(100u64);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn paper_run_writes_csv_to_tmp() {
        let dir = std::env::temp_dir().join("aero-paper-test");
        let _ = std::fs::create_dir_all(&dir);
        let csv = dir.join("results.csv");
        let cfg = BotConfig::load(&PathBuf::from("config/default.toml")).expect("default config");
        let args = Args {
            config: PathBuf::from("config/default.toml"),
            csv: csv.clone(),
            fork_url: String::new(),
            tolerance_bps: 50,
            kill: false,
            fork_check: false,
            fork_matrix: false,
            discord_alerts: false,
            l1_fee: None,
        };
        run_paper(&args, &cfg).await.expect("paper run succeeds");
        let body = std::fs::read_to_string(&csv).expect("csv written");
        assert!(body.contains("opportunities_seen"));
        assert!(body.contains("hit_rate"));
        assert!(
            body.contains("pair,direction,seen,"),
            "csv must carry the per-pair breakdown section:\n{body}"
        );
    }

    #[test]
    fn skewed_clone_uses_full_256_bits() {
        // High bits set: the old f64/low-128 implementation silently dropped
        // them. Integer U256 math must preserve the full width (saturating).
        let (mut leg, _) = fixture_legs();
        leg.sqrt_price_x96 = U256::MAX;
        let out = skewed_clone(&leg);
        let high_bits = out.sqrt_price_x96 >> 128;
        assert!(
            high_bits > U256::ZERO,
            "high bits must survive the skew (got {out:?})"
        );
        assert_eq!(
            out.sqrt_price_x96,
            U256::MAX.saturating_mul(U256::from(102u64)) / U256::from(100u64),
            "exact integer semantics: saturating mul then divide"
        );
        // Normal fixture scale: +2% on sqrt (price moves ~+4%, still
        // exercises the profitable branch).
        let (base, _) = fixture_legs();
        let skewed = skewed_clone(&base);
        assert_eq!(
            skewed.sqrt_price_x96,
            base.sqrt_price_x96.saturating_mul(U256::from(102u64)) / U256::from(100u64)
        );
        assert!(skewed.sqrt_price_x96 > base.sqrt_price_x96);
    }

    #[test]
    fn measured_l1_fee_flows_into_paper_run() {
        // Same config, different L1 fee source: the measured run must log
        // `measured(--l1-fee)` costs (net differs from fixture). Offline:
        // assert on the pure cost-model wiring the flag drives.
        let mut fixture = CostModel::paper_fixture();
        let base_net = profit::net_profit(
            U256::from(600_000_000u64),
            U256::from(500_000_000u64),
            &fixture,
        )
        .expect("fixture clears");
        fixture.set_l1_fee(U256::from(1_000_000u64));
        let measured_net = profit::net_profit(
            U256::from(600_000_000u64),
            U256::from(500_000_000u64),
            &fixture,
        )
        .expect("measured clears");
        assert!(measured_net < base_net);
    }
}
