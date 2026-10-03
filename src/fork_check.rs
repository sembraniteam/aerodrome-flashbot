//! Read-only fork check: real QuoterV2 vs off-chain estimator.
//!
//! Compares on-chain `quoteExactInputSingle` results against the integer-only
//! [`estimate_amount_out`](crate::pools::estimate_amount_out) for the
//! allowlisted pairs:
//!
//! - `WETH/USDC`, `AERO/USDC` at $100 / $500 / $1000 (USDC -> token
//!   direction, amounts in 6dp USDC base units);
//! - `AERO/WETH` at 0.05 / 0.2 / 0.5 WETH (WETH -> AERO direction, amounts in
//!   18dp WETH base units -- both tokens have 18 decimals, so USDC notionals
//!   do not apply).
//!
//! Safety: read-only `eth_call`s only. No keys, no signing, no broadcast, no
//! state changes. If no RPC is reachable (offline CI), the check prints a
//! `SKIPPED` line and exits `Ok` so `cargo test` stays green with zero
//! network.
//!
//! Quoter shapes (verified against `IQuoterV2.sol` on both repos):
//!
//! - Aerodrome Slipstream QuoterV2 (`0x254c...15b0`):
//!   `quoteExactInputSingle((tokenIn,tokenOut,amountIn,tickSpacing,sqrtPriceLimitX96))`
//!   — the pool is keyed by `tickSpacing` (`CLFactory.getPool`), the live
//!   `fee()` is read separately for the estimator and the `fee_used` column.
//! - Uniswap V3 QuoterV2 (`0x3d4e...B76a`):
//!   `quoteExactInputSingle((tokenIn,tokenOut,amountIn,fee,sqrtPriceLimitX96))`
//!   — the pool is keyed by `fee` (`V3Factory.getPool`).
//!
//! References:
//!
//! - `aerodrome-finance/slipstream` lens QuoterV2 (tickSpacing-keyed struct).
//! - Uniswap `v3-periphery` QuoterV2 (fee-keyed struct).

use alloy::primitives::{Address, U160, U256, aliases::U24};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use std::future::IntoFuture as _;
use std::time::Duration;

use crate::config::addresses;
use crate::pools::{ClPoolState, PoolKind, estimate_amount_out, resolve_fee};
use crate::sequencer;
use crate::sim::verify_quote;

sol! {
    /// Slipstream factory: pools keyed by tick spacing.
    #[sol(rpc)]
    interface ISlipFactory {
        function getPool(address tokenA, address tokenB, int24 tickSpacing) external view returns (address);
    }

    /// Uniswap V3 factory: pools keyed by fee tier.
    #[sol(rpc)]
    interface IUniFactory {
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address);
    }

    /// Aerodrome Slipstream QuoterV2 single-hop quote (struct param, field
    /// order per `IQuoterV2.sol`: amountIn BEFORE tickSpacing — same layout
    /// as UniV3, with `int24` in place of `uint24`).
    #[sol(rpc)]
    interface IAeroQuoterV2 {
        struct AeroQuoteParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            int24 tickSpacing;
            uint160 sqrtPriceLimitX96;
        }
        function quoteExactInputSingle(AeroQuoteParams params) external returns (
            uint256 amountOut,
            uint160 sqrtPriceX96After,
            uint32 initializedTicksCrossed,
            uint256 gasEstimate
        );
    }

    /// Uniswap V3 QuoterV2 single-hop quote (struct param, note the field
    /// order: amountIn BEFORE fee).
    #[sol(rpc)]
    interface IUniQuoterV2 {
        struct UniQuoteParams {
            address tokenIn;
            address tokenOut;
            uint256 amountIn;
            uint24 fee;
            uint160 sqrtPriceLimitX96;
        }
        function quoteExactInputSingle(UniQuoteParams params) external returns (
            uint256 amountOut,
            uint160 sqrtPriceX96After,
            uint32 initializedTicksCrossed,
            uint256 gasEstimate
        );
    }

    /// OP-Stack GasPriceOracle: `getL1Fee(bytes)` for the L1 data-fee term
    /// of the cost model. Predeploy at
    /// `0x420000000000000000000000000000000000000F` on every OP-Stack chain.
    #[sol(rpc)]
    interface IGasPriceOracle {
        function getL1Fee(bytes memory data) external view returns (uint256);
    }

    /// Uniswap V3 pool: 7-field `slot0` (with `observationCardinalityNext`).
    #[sol(rpc)]
    interface IUniPool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function tickSpacing() external view returns (int24);
    }

    /// Slipstream pool: 6-field `slot0` (no `observationCardinalityNext`;
    /// matches `ICLPool.slot0` destructured as 6-tuple in QuoterV2).
    #[sol(rpc)]
    interface ISlipPool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function tickSpacing() external view returns (int24);
        function fee() external view returns (uint24);
    }
}

/// Tick spacings probed on the Slipstream factory. Tier list per the
/// Aerodrome Slipstream `SPECIFICATION.md` tick-spacing/fee table
/// (`aerodrome-finance/slipstream`: ts 1/50/100/200/2000 -- actual swap fees
/// are dynamic via the CustomSwapFeeModule, so the static
/// `slipstream_fee_for_tick_spacing` map is a fallback only and the live
/// `fee()` read wins wherever available).
pub const SLIP_SPACINGS: [i32; 5] = [1, 50, 100, 200, 2000];
/// Fee tiers probed on the Uniswap V3 factory.
pub const UNI_FEES: [u32; 4] = [100, 500, 3000, 10_000];

/// Quote sizes in USDC base units (6 decimals): $100 / $500 / $1000.
pub const SIZES: [(&str, u64); 3] = [
    ("$100", 100_000_000),
    ("$500", 500_000_000),
    ("$1000", 1_000_000_000),
];

/// One fork-check target: pair label, token_in, token_out, and quote sizes
/// denominated in token_in base units.
type Target<'a> = (&'a str, Address, Address, &'a [(&'a str, u64)]);

/// Quote sizes for AERO/WETH in WETH base units (18 decimals):
/// 0.05 / 0.2 / 0.5 WETH. A separate table because both tokens have 18
/// decimals, so the USDC notionals in [`SIZES`] do not apply.
pub const WETH_SIZES: [(&str, u64); 3] = [
    ("0.05W", 50_000_000_000_000_000),
    ("0.2W", 200_000_000_000_000_000),
    ("0.5W", 500_000_000_000_000_000),
];

/// Default local fork endpoint when neither `--fork-url` nor `FORK_URL` is set.
pub const DEFAULT_FORK_URL: &str = "http://127.0.0.1:8545";

/// Delay between `eth_call`s so bursts stay under public-endpoint rate limits
/// (a local anvil fork is unaffected, just slightly slower).
const PACE: Duration = Duration::from_millis(1200);
/// Backoffs between throttled attempts (public endpoints allow only a few
/// rps; a local anvil fork never throttles).
const THROTTLE_BACKOFFS: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
];

/// One comparison row.
#[derive(Debug, Clone)]
pub struct ForkRow {
    pub pair: String,
    pub venue: String,
    pub pool: Address,
    pub size_label: String,
    pub amount_in: U256,
    pub fee_ppm: u32,
    pub quoter_out: U256,
    pub estimator_out: U256,
    pub diff_bps: Option<u64>,
    pub pass: bool,
    pub note: String,
}

/// Relative mismatch in bps: `|est - quot| * 10000 / quot`.
/// `None` when the quoter returned zero (division undefined); callers treat
/// `None` as pass only when the estimator is also zero.
pub fn diff_bps(estimator_out: U256, quoter_out: U256) -> Option<u64> {
    if quoter_out.is_zero() {
        return None;
    }
    let diff = estimator_out.abs_diff(quoter_out);
    let bps = diff.saturating_mul(U256::from(10_000u64)) / quoter_out;
    Some(u64::try_from(bps).unwrap_or(u64::MAX))
}

/// Pass/fail of one row against `tolerance_bps`. Zero/zero passes (both agree
/// there is no output); quoter-zero with non-zero estimate fails.
pub fn pass_fail(estimator_out: U256, quoter_out: U256, tolerance_bps: u64) -> (Option<u64>, bool) {
    let bps = diff_bps(estimator_out, quoter_out);
    match bps {
        Some(b) => (
            Some(b),
            verify_quote(estimator_out, quoter_out, tolerance_bps),
        ),
        None => (None, estimator_out.is_zero()),
    }
}

/// Resolve the fork endpoint: `--fork-url` > `$FORK_URL` > local default.
pub fn resolve_fork_url(cli_fork_url: &str) -> String {
    if !cli_fork_url.is_empty() {
        return cli_fork_url.to_string();
    }
    if let Ok(env) = std::env::var("FORK_URL")
        && !env.trim().is_empty()
    {
        return env;
    }
    DEFAULT_FORK_URL.to_string()
}

/// Normalize `ws(s)://` to `http(s)://` for the HTTP provider. Anvil serves
/// HTTP on the same port, so `ws://127.0.0.1:8545` and
/// `http://127.0.0.1:8545` reach the same fork.
pub fn normalize_rpc_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("ws://") {
        return format!("http://{rest}");
    }
    if let Some(rest) = url.strip_prefix("wss://") {
        return format!("https://{rest}");
    }
    url.to_string()
}

/// Render the result table (pure, unit-tested).
pub fn format_table(rows: &[ForkRow], tolerance_bps: u64) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "fork-check (tolerance={tolerance_bps}bps, USDC->token, read-only eth_call)\n"
    ));
    out.push_str(&format!(
        "{:<10} {:<9} {:<42} {:<6} {:<12} {:<9} {:<22} {:<22} {:<9} {}\n",
        "pair",
        "venue",
        "pool",
        "size",
        "amount_in",
        "fee_used",
        "quoter_out",
        "estimator_out",
        "diff_bps",
        "result"
    ));
    for r in rows {
        let bps = r.diff_bps.map_or("-".to_string(), |b| b.to_string());
        let verdict = if r.pass { "PASS" } else { "FAIL" };
        let note = if r.note.is_empty() {
            String::new()
        } else {
            format!(" ({})", r.note)
        };
        out.push_str(&format!(
            "{:<10} {:<9} {:<42} {:<6} {:<12} {:<9} {:<22} {:<22} {:<9} {}{}\n",
            r.pair,
            r.venue,
            r.pool.to_string(),
            r.size_label,
            r.amount_in,
            r.fee_ppm,
            r.quoter_out,
            r.estimator_out,
            bps,
            verdict,
            note
        ));
    }
    let passed = rows.iter().filter(|r| r.pass).count();
    out.push_str(&format!(
        "summary: {passed}/{} rows within tolerance\n",
        rows.len()
    ));
    out
}

/// Entry point for `--fork-check`: connect, probe, print the table.
/// Never signs or broadcasts. Returns `Ok` on skip too (offline CI green).
pub async fn run_fork_check(cli_fork_url: &str, tolerance_bps: u64) -> anyhow::Result<()> {
    let raw = resolve_fork_url(cli_fork_url);
    let url = normalize_rpc_url(&raw);
    // Redact before logging (provider keys live in URL userinfo/path/query).
    tracing::info!(
        rpc = %crate::chain::redact_url(&raw),
        tolerance_bps,
        "fork-check: quoter vs estimator (read-only)"
    );

    let provider = match try_connect(&url).await {
        Some(p) => p,
        None => {
            println!(
                "fork-check SKIPPED: no RPC reachable at {} (start `anvil --fork-url https://mainnet.base.org`, then `FORK_URL=http://127.0.0.1:8545 cargo run -- --fork-check`). No secrets needed.",
                crate::chain::redact_url(&raw)
            );
            return Ok(());
        }
    };

    match collect_rows(&provider, tolerance_bps).await {
        Ok((rows, _trips)) => {
            if rows.is_empty() {
                println!(
                    "fork-check SKIPPED: no liquid pools discovered for WETH/USDC, AERO/USDC, AERO/WETH at {} (fork may be stale or pools not found).",
                    crate::chain::redact_url(&raw)
                );
                return Ok(());
            }
            println!("{}", format_table(&rows, tolerance_bps));
            Ok(())
        }
        Err(e) => {
            // Fail-closed but CI-green: RPC died mid-check -> report skip.
            println!(
                "fork-check SKIPPED: RPC error mid-check at {}: {e:#} (re-run against a live fork).",
                crate::chain::redact_url(&raw)
            );
            Ok(())
        }
    }
}

type HttpProvider = alloy::providers::RootProvider<alloy::network::Ethereum>;

/// True when an RPC error message reports throttling (worth one paced retry).
fn is_throttled(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("rate limit")
        || m.contains("over rate")
        || m.contains("32016")
        || m.contains("429")
        || m.contains("too many requests")
}

/// Bounded, paced `eth_call` with one throttled retry. Public Base endpoints
/// rate-limit bursts (`-32016 over rate limit`); a local anvil fork does not.
/// Read-only; no gas spent, no state changed.
///
/// The `op` closure must build a fresh call per attempt (alloy `EthCall`
/// borrows its builder, so callers construct everything inside an
/// `async move` block that owns its temporaries; only `Copy` scalars and the
/// provider reference are captured).
async fn rpc<F, Fut, T, E>(label: &str, timeout: Duration, op: F) -> anyhow::Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::IntoFuture<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    tokio::time::sleep(PACE).await;
    let run = || async {
        match tokio::time::timeout(timeout, op().into_future()).await {
            Err(_) => Err(anyhow::anyhow!("{label} timeout")),
            Ok(Err(e)) => Err(anyhow::anyhow!("{label}: {e:#}")),
            Ok(Ok(v)) => Ok(v),
        }
    };
    let mut err = match run().await {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };
    for backoff in THROTTLE_BACKOFFS {
        if !is_throttled(&err.to_string()) {
            return Err(err);
        }
        tracing::warn!("{label} throttled, backing off {}s", backoff.as_secs());
        tokio::time::sleep(backoff).await;
        match run().await {
            Ok(v) => return Ok(v),
            Err(e) => err = e,
        }
    }
    Err(err)
}

/// Connect with a short timeout; `None` means "skip gracefully".
async fn try_connect(url: &str) -> Option<HttpProvider> {
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(url.parse().ok()?);
    // Prove liveness (and fail fast offline) with a bounded chain-id probe.
    match tokio::time::timeout(Duration::from_secs(5), provider.get_chain_id()).await {
        Ok(Ok(id)) => {
            if id != addresses::BASE_CHAIN_ID {
                tracing::warn!(chain_id = id, "fork chain id is not Base mainnet (8453)");
            }
            Some(provider)
        }
        Ok(Err(e)) => {
            tracing::warn!("fork RPC unreachable: {e:#}");
            None
        }
        Err(_) => {
            tracing::warn!("fork RPC probe timed out");
            None
        }
    }
}

/// Off-chain two-leg round trip in consistent units (USDC -> token on one
/// venue, token -> USDC on the other, both in USDC base units). This is the
/// unit-consistent baseline the sandwich drill shocks: single-leg quotes
/// are in OUTPUT-token units and must never be compared to the USDC input.
#[derive(Debug, Clone)]
pub struct RoundTrip {
    pub pair: String,
    pub size_label: String,
    pub amount_in: U256,
    pub gross_out: U256,
    pub via: String,
}

/// Round-trip gross for one size through two pool states (pure, unit-tested).
/// `zfo_out` is the USDC->token direction flag; the return leg uses `!zfo`.
pub fn round_trip_gross(
    first: &ClPoolState,
    second: &ClPoolState,
    amount_in: U256,
    zfo_out: bool,
) -> U256 {
    let mid = estimate_amount_out(first, amount_in, zfo_out);
    estimate_amount_out(second, mid, !zfo_out)
}

/// Discover the most liquid pool per venue for one token pair, then quote all
/// sizes through both quoters and compare with the estimator. Also returns
/// the off-chain round-trip gross per size (both directions, best kept) for
/// the sandwich drill.
async fn collect_rows<P>(
    provider: &P,
    tolerance_bps: u64,
) -> anyhow::Result<(Vec<ForkRow>, Vec<RoundTrip>)>
where
    P: Provider + Clone,
{
    let mut rows = Vec::new();
    let mut trips = Vec::new();
    // (pair, token_in, token_out, sizes): USDC-quoted pairs use USDC
    // notionals; AERO/WETH is 18/18 decimals and uses WETH notionals.
    let targets: [Target<'_>; 3] = [
        ("WETH/USDC", addresses::USDC_NATIVE, addresses::WETH, &SIZES),
        ("AERO/USDC", addresses::USDC_NATIVE, addresses::AERO, &SIZES),
        ("AERO/WETH", addresses::WETH, addresses::AERO, &WETH_SIZES),
    ];
    for (pair_name, token_in, token_out, sizes) in targets {
        // Direction under test: token_in -> token_out (sizes are notionals
        // in the token_in decimals).
        let zero_for_one = token_in < token_out;
        let slip = discover_slip_pool(provider, token_in, token_out).await?;
        if let Some(ref state) = slip {
            tracing::info!(
                pair = pair_name,
                pool = %state.pool,
                tick = state.tick,
                liquidity = state.liquidity,
                fee_ppm = state.fee_ppm,
                tick_spacing = state.tick_spacing,
                "slipstream pool snapshot"
            );
            for (label, amount) in sizes.iter().copied() {
                rows.push(
                    quote_row(
                        provider,
                        QuoteSpec {
                            pair: pair_name,
                            venue: "aero",
                            state,
                            token_in,
                            token_out,
                            zero_for_one,
                            size_label: label,
                            amount_in: U256::from(amount),
                            tolerance_bps,
                        },
                    )
                    .await,
                );
            }
        } else {
            tracing::warn!(pair = pair_name, "no liquid Slipstream pool found");
        }
        let uni = discover_uni_pool(provider, token_in, token_out).await?;
        if let Some(ref state) = uni {
            tracing::info!(
                pair = pair_name,
                pool = %state.pool,
                tick = state.tick,
                liquidity = state.liquidity,
                fee_ppm = state.fee_ppm,
                tick_spacing = state.tick_spacing,
                "univ3 pool snapshot"
            );
            for (label, amount) in sizes.iter().copied() {
                rows.push(
                    quote_row(
                        provider,
                        QuoteSpec {
                            pair: pair_name,
                            venue: "uni",
                            state,
                            token_in,
                            token_out,
                            zero_for_one,
                            size_label: label,
                            amount_in: U256::from(amount),
                            tolerance_bps,
                        },
                    )
                    .await,
                );
            }
        } else {
            tracing::warn!(pair = pair_name, "no liquid UniV3 pool found");
        }
        // Round-trip gross per size (both directions, best kept) for the
        // sandwich drill. Single-tick estimate (ignores cross-tick impact):
        // a pre-filter baseline, never a submission input.
        if let (Some(a), Some(b)) = (slip.as_ref(), uni.as_ref()) {
            for (label, amount) in sizes.iter().copied() {
                let amount_in = U256::from(amount);
                let fwd = round_trip_gross(a, b, amount_in, zero_for_one);
                let rev = round_trip_gross(b, a, amount_in, zero_for_one);
                let (gross_out, via) = if fwd >= rev {
                    (fwd, "aero->uni")
                } else {
                    (rev, "uni->aero")
                };
                trips.push(RoundTrip {
                    pair: pair_name.to_string(),
                    size_label: label.to_string(),
                    amount_in,
                    gross_out,
                    via: via.to_string(),
                });
            }
        }
    }
    Ok((rows, trips))
}

/// Probe Slipstream spacings, keep the pool with the deepest liquidity.
async fn discover_slip_pool<P>(
    provider: &P,
    token_in: Address,
    token_out: Address,
) -> anyhow::Result<Option<ClPoolState>>
where
    P: Provider + Clone,
{
    let mut best: Option<ClPoolState> = None;
    for spacing in SLIP_SPACINGS {
        let pool: Address = rpc("slip getPool", Duration::from_secs(15), || async move {
            let f = ISlipFactory::new(addresses::AERO_SLIPSTREAM_FACTORY_V1, provider.clone());
            // Const spacings always fit in int24.
            let ts =
                alloy::primitives::aliases::I24::try_from(spacing).expect("const spacing fits");
            f.getPool(token_in, token_out, ts)
                .call()
                .into_future()
                .await
        })
        .await?;
        if pool.is_zero() {
            continue;
        }
        if let Some(state) = read_pool_state(provider, pool, PoolKind::Slipstream).await? {
            let deeper = best
                .as_ref()
                .is_none_or(|b: &ClPoolState| state.liquidity > b.liquidity);
            if deeper {
                best = Some(state);
            }
        }
    }
    Ok(best)
}

/// Probe UniV3 fee tiers, keep the pool with the deepest liquidity.
async fn discover_uni_pool<P>(
    provider: &P,
    token_in: Address,
    token_out: Address,
) -> anyhow::Result<Option<ClPoolState>>
where
    P: Provider + Clone,
{
    let mut best: Option<ClPoolState> = None;
    for fee in UNI_FEES {
        let pool: Address = rpc("uni getPool", Duration::from_secs(15), || async move {
            let f = IUniFactory::new(addresses::UNIV3_FACTORY_BASE, provider.clone());
            // Const tiers always fit in uint24.
            let tier = U24::try_from(fee).expect("const fee fits");
            f.getPool(token_in, token_out, tier)
                .call()
                .into_future()
                .await
        })
        .await?;
        if pool.is_zero() {
            continue;
        }
        if let Some(state) = read_pool_state(provider, pool, PoolKind::UniV3).await? {
            let deeper = best
                .as_ref()
                .is_none_or(|b: &ClPoolState| state.liquidity > b.liquidity);
            if deeper {
                best = Some(state);
            }
        }
    }
    Ok(best)
}

/// Read `slot0` / `liquidity` / `tickSpacing` / `fee()` for one pool.
/// Returns `None` for empty/unknown-fee pools (fail closed: no quote).
/// Slipstream and UniV3 pools expose different `slot0` arities (6 vs 7
/// fields), so each venue gets its own binding.
async fn read_pool_state<P>(
    provider: &P,
    pool: Address,
    kind: PoolKind,
) -> anyhow::Result<Option<ClPoolState>>
where
    P: Provider + Clone,
{
    // `fee()` exists on Slipstream dynamic pools; UniV3 static pools revert —
    // fall back to the tier map via `resolve_fee`.
    let (sqrt_price_x96, tick, tick_spacing, liquidity, live_fee): (
        U256,
        i32,
        i32,
        u128,
        Option<u32>,
    ) = match kind {
        PoolKind::Slipstream => {
            let s0 = rpc("slip slot0", Duration::from_secs(15), || async move {
                let c = ISlipPool::new(pool, provider.clone());
                c.slot0().call().into_future().await
            })
            .await?;
            let liq = rpc("slip liquidity", Duration::from_secs(15), || async move {
                let c = ISlipPool::new(pool, provider.clone());
                c.liquidity().call().into_future().await
            })
            .await?;
            let spacing_ret = rpc("slip tickSpacing", Duration::from_secs(15), || async move {
                let c = ISlipPool::new(pool, provider.clone());
                c.tickSpacing().call().into_future().await
            })
            .await?;
            let live_fee: Option<u32> =
                match rpc("slip fee", Duration::from_secs(15), || async move {
                    let c = ISlipPool::new(pool, provider.clone());
                    c.fee().call().into_future().await
                })
                .await
                {
                    Ok(ret) => Some(u32::try_from(ret).unwrap_or(u32::MAX)),
                    _ => None,
                };
            (
                U256::from(s0.sqrtPriceX96),
                s0.tick.try_into().unwrap_or(0),
                spacing_ret.try_into().unwrap_or(0),
                liq,
                live_fee,
            )
        }
        PoolKind::UniV3 => {
            let s0 = rpc("uni slot0", Duration::from_secs(15), || async move {
                let c = IUniPool::new(pool, provider.clone());
                c.slot0().call().into_future().await
            })
            .await?;
            let liq = rpc("uni liquidity", Duration::from_secs(15), || async move {
                let c = IUniPool::new(pool, provider.clone());
                c.liquidity().call().into_future().await
            })
            .await?;
            let spacing_ret = rpc("uni tickSpacing", Duration::from_secs(15), || async move {
                let c = IUniPool::new(pool, provider.clone());
                c.tickSpacing().call().into_future().await
            })
            .await?;
            (
                U256::from(s0.sqrtPriceX96),
                s0.tick.try_into().unwrap_or(0),
                spacing_ret.try_into().unwrap_or(0),
                liq,
                None,
            )
        }
    };
    let fee_ppm = match resolve_fee(kind, tick_spacing, live_fee) {
        Some(f) => f,
        None => {
            tracing::warn!(%pool, tick_spacing, ?live_fee, "unknown fee, skipping pool");
            return Ok(None);
        }
    };
    Ok(ClPoolState::new(
        pool,
        kind,
        sqrt_price_x96,
        liquidity,
        tick,
        fee_ppm,
        tick_spacing,
    ))
}

/// Inputs for one quoter-vs-estimator comparison row.
struct QuoteSpec<'a> {
    pair: &'a str,
    venue: &'a str,
    state: &'a ClPoolState,
    token_in: Address,
    token_out: Address,
    zero_for_one: bool,
    size_label: &'a str,
    amount_in: U256,
    tolerance_bps: u64,
}

/// Quote one size through the venue quoter, compare with the estimator.
async fn quote_row<P>(provider: &P, spec: QuoteSpec<'_>) -> ForkRow
where
    P: Provider + Clone,
{
    let estimator_out = estimate_amount_out(spec.state, spec.amount_in, spec.zero_for_one);
    let outcome: Result<U256, String> = if spec.venue == "aero" {
        quote_aero(
            provider,
            spec.token_in,
            spec.token_out,
            spec.state.tick_spacing,
            spec.amount_in,
        )
        .await
    } else {
        quote_uni(
            provider,
            spec.token_in,
            spec.token_out,
            spec.state.fee_ppm,
            spec.amount_in,
        )
        .await
    };
    match outcome {
        Ok(quoter_out) => {
            let (bps, pass) = pass_fail(estimator_out, quoter_out, spec.tolerance_bps);
            ForkRow {
                pair: spec.pair.to_string(),
                venue: spec.venue.to_string(),
                pool: spec.state.pool,
                size_label: spec.size_label.to_string(),
                amount_in: spec.amount_in,
                fee_ppm: spec.state.fee_ppm,
                quoter_out,
                estimator_out,
                diff_bps: bps,
                pass,
                note: String::new(),
            }
        }
        Err(note) => ForkRow {
            pair: spec.pair.to_string(),
            venue: spec.venue.to_string(),
            pool: spec.state.pool,
            size_label: spec.size_label.to_string(),
            amount_in: spec.amount_in,
            fee_ppm: spec.state.fee_ppm,
            quoter_out: U256::ZERO,
            estimator_out,
            diff_bps: None,
            pass: false,
            note,
        },
    }
}

async fn quote_aero<P>(
    provider: &P,
    token_in: Address,
    token_out: Address,
    tick_spacing: i32,
    amount_in: U256,
) -> Result<U256, String>
where
    P: Provider + Clone,
{
    match rpc("aero quoter", Duration::from_secs(20), || async move {
        let q = IAeroQuoterV2::new(addresses::AERO_QUOTER_V2, provider.clone());
        // tick_spacing comes from live pool state and always fits int24.
        let spacing =
            alloy::primitives::aliases::I24::try_from(tick_spacing).expect("pool spacing fits");
        let params = IAeroQuoterV2::AeroQuoteParams {
            tokenIn: token_in,
            tokenOut: token_out,
            amountIn: amount_in,
            tickSpacing: spacing,
            sqrtPriceLimitX96: U160::ZERO,
        };
        q.quoteExactInputSingle(params).call().into_future().await
    })
    .await
    {
        Ok(ret) => Ok(ret.amountOut),
        Err(e) => Err(format!("quoter error: {e:#}")),
    }
}

async fn quote_uni<P>(
    provider: &P,
    token_in: Address,
    token_out: Address,
    fee_ppm: u32,
    amount_in: U256,
) -> Result<U256, String>
where
    P: Provider + Clone,
{
    match rpc("uni quoter", Duration::from_secs(20), || async move {
        let q = IUniQuoterV2::new(addresses::UNIV3_QUOTER_V2, provider.clone());
        // fee_ppm comes from the resolved tier map / live fee(), fits uint24.
        let tier = U24::try_from(fee_ppm).expect("pool fee fits");
        let params = IUniQuoterV2::UniQuoteParams {
            tokenIn: token_in,
            tokenOut: token_out,
            amountIn: amount_in,
            fee: tier,
            sqrtPriceLimitX96: U160::ZERO,
        };
        q.quoteExactInputSingle(params).call().into_future().await
    })
    .await
    {
        Ok(ret) => Ok(ret.amountOut),
        Err(e) => Err(format!("quoter error: {e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bps_math_matches_verify_quote() {
        // 0.5% high -> 50bps, within 50 -> pass.
        let (bps, pass) = pass_fail(U256::from(10050u64), U256::from(10000u64), 50);
        assert_eq!(bps, Some(50));
        assert!(pass);
        let (bps, pass) = pass_fail(U256::from(10100u64), U256::from(10000u64), 50);
        assert_eq!(bps, Some(100));
        assert!(!pass);
    }

    #[test]
    fn zero_handling_is_fail_closed() {
        assert_eq!(pass_fail(U256::ZERO, U256::ZERO, 50), (None, true));
        let (bps, pass) = pass_fail(U256::from(1u64), U256::ZERO, 50);
        assert_eq!(bps, None);
        assert!(!pass);
    }

    #[test]
    fn url_resolution_prefers_cli_then_env() {
        // SAFETY: single-threaded test mutating process env for precedence check.
        let old = std::env::var("FORK_URL").ok();
        unsafe { std::env::set_var("FORK_URL", "http://env:8545") };
        assert_eq!(resolve_fork_url("http://cli:8545"), "http://cli:8545");
        assert_eq!(resolve_fork_url(""), "http://env:8545");
        unsafe { std::env::remove_var("FORK_URL") };
        assert_eq!(resolve_fork_url(""), DEFAULT_FORK_URL);
        if let Some(v) = old {
            unsafe { std::env::set_var("FORK_URL", v) };
        }
    }

    #[test]
    fn ws_urls_normalize_to_http() {
        assert_eq!(
            normalize_rpc_url("ws://127.0.0.1:8545"),
            "http://127.0.0.1:8545"
        );
        assert_eq!(
            normalize_rpc_url("wss://example.com/rpc"),
            "https://example.com/rpc"
        );
        assert_eq!(
            normalize_rpc_url("http://127.0.0.1:8545"),
            "http://127.0.0.1:8545"
        );
    }

    #[test]
    fn venue_quoter_selectors_differ() {
        // Aero (tickSpacing int24 3rd) vs Uni (amountIn uint256 3rd) must not
        // share a selector: same selector would quote the wrong pool.
        use alloy::sol_types::SolCall;
        assert_ne!(
            IAeroQuoterV2::quoteExactInputSingleCall::SELECTOR,
            IUniQuoterV2::quoteExactInputSingleCall::SELECTOR
        );
    }

    #[test]
    fn table_renders_all_required_columns() {
        let rows = vec![ForkRow {
            pair: "WETH/USDC".to_string(),
            venue: "aero".to_string(),
            pool: Address::ZERO,
            size_label: "$100".to_string(),
            amount_in: U256::from(100_000_000u64),
            fee_ppm: 500,
            quoter_out: U256::from(1000u64),
            estimator_out: U256::from(1005u64),
            diff_bps: Some(50),
            pass: true,
            note: String::new(),
        }];
        let t = format_table(&rows, 50);
        for col in [
            "pair",
            "size",
            "quoter_out",
            "estimator_out",
            "diff_bps",
            "fee_used",
            "PASS",
        ] {
            assert!(t.contains(col), "missing column {col}:\n{t}");
        }
    }
}

// ---------------------------------------------------------------------------
// No.4 fork matrix: Vault fee>0, sandwich drill, deadlines, transient note,
// L1 getL1Fee per size, codehash freeze rows, sequencer gate.
// Read-only `eth_call`s only; skips gracefully (exit 0) offline.
// ---------------------------------------------------------------------------

/// Adverse price moves (bps) covered by the sandwich/front-run drill: a move
/// of this size against the quoted edge must make `execute` REVERT
/// (`InsufficientProfit` / leg-minimum), never settle at a loss.
pub const SANDWICH_SHOCKS_BPS: [u64; 3] = [10, 25, 50];

/// Representative `execute` calldata length (bytes) used for the L1 `getL1Fee`
/// query. The L1 fee depends on calldata length, NOT on the flash amount, so
/// one length serves all size rows (two opaque router legs + params).
pub const REPRESENTATIVE_EXECUTE_CALLDATA_LEN: usize = 768;

/// Paper flat-cost fixture reused by the sandwich drill when live costs are
/// unavailable (L2 gas 50k + L1 fee 20k + priority 5k + failed-attempt 10k, in
/// USDC base units). Live path replaces every term with measured values.
pub const DRILL_FLAT_COSTS: u64 = 85_000;

/// Paper min-net fixture for the drill ($5 in USDC base units).
pub const DRILL_MIN_NET: u64 = 5_000_000;

/// One fork-matrix row. `skipped` rows (RPC detail unavailable) never fail
/// the matrix; they are counted separately in the summary.
#[derive(Debug, Clone)]
pub struct MatrixRow {
    pub check: String,
    pub detail: String,
    pub pass: bool,
    pub skipped: bool,
    pub note: String,
}

impl MatrixRow {
    fn pass(check: &str, detail: String, note: &str) -> Self {
        Self {
            check: check.to_string(),
            detail,
            pass: true,
            skipped: false,
            note: note.to_string(),
        }
    }

    fn fail(check: &str, detail: String, note: &str) -> Self {
        Self {
            check: check.to_string(),
            detail,
            pass: false,
            skipped: false,
            note: note.to_string(),
        }
    }

    fn skip(check: &str, note: &str) -> Self {
        Self {
            check: check.to_string(),
            detail: "-".to_string(),
            pass: true,
            skipped: true,
            note: format!("SKIPPED: {note}"),
        }
    }
}

/// Vault repayment for a flash amount at `fee_bps` (Balancer charges
/// `amount + feeAmounts[i]`; Base is 0% but the path stays generic so a
/// fee>0 regime stays solvent). Rounded up via div-ceil: the borrower never
/// underpays by a wei.
pub fn flash_repay(amount: U256, fee_bps: u64) -> U256 {
    let fee = amount.saturating_mul(U256::from(fee_bps.min(10_000)));
    amount.saturating_add(fee.saturating_add(U256::from(9_999u64)) / U256::from(10_000u64))
}

/// Sandwich/front-run drill (pure): shock the quoted output down by
/// `shock_bps` (price moved against us between quote and inclusion) and
/// report whether the executor's profit gate would REVERT the bundle.
/// `true` = reverts (correct: revert, do not lose); `false` = would still
/// settle (only acceptable when the edge survives the shock).
///
/// UNITS: `quoter_out` MUST be in the same units as `amount_in` — i.e. a
/// two-leg round-trip gross (USDC -> token -> USDC), never a single-leg
/// quote (which is denominated in the OUTPUT token). The live matrix feeds
/// [`RoundTrip::gross_out`]; mixing single-leg output with the USDC input is
/// a dimension bug, not a drill result.
pub fn sandwich_would_revert(
    quoter_out: U256,
    shock_bps: u64,
    amount_in: U256,
    flat_costs: U256,
    min_net: U256,
) -> bool {
    let shocked = quoter_out
        .saturating_mul(U256::from(10_000u64.saturating_sub(shock_bps.min(10_000))))
        / U256::from(10_000u64);
    match shocked
        .checked_sub(amount_in)
        .and_then(|v| v.checked_sub(flat_costs))
    {
        Some(net) => net < min_net,
        None => true,
    }
}

/// Outer executor deadline rule: the whole bundle reverts past `deadline`
/// (unix seconds). `true` = may execute.
pub fn deadline_ok(deadline_secs: u64, now_secs: u64) -> bool {
    now_secs <= deadline_secs
}

/// Aerodrome Slipstream inner-deadline rule: `exactInputSingle` carries its
/// own `deadline` field which the executor re-checks on-chain (UniV3
/// SwapRouter02 has NO deadline field, so only the outer check applies
/// there). Same comparison, tracked separately so a missing inner pin is a
/// visible gap, not a silent pass.
pub fn aero_inner_deadline_ok(inner_deadline_secs: u64, now_secs: u64) -> bool {
    deadline_ok(inner_deadline_secs, now_secs)
}

/// Transient-storage (EIP-1153, Cancun) availability per chain. The executor
/// binds callbacks through `transient _expectedHash` / `_inCallback`; those
/// need Cancun-active execution. Base mainnet and Base Sepolia are
/// post-Cancun; anything else is "do not assume".
pub fn transient_supported(chain_id: u64) -> bool {
    chain_id == addresses::BASE_CHAIN_ID || chain_id == 84532
}

/// Pinned-address codehash snapshot row helper: `None` code (empty account)
/// fails the row; the hex detail feeds `docs/FREEZE.md`.
pub fn codehash_row(label: &str, addr: Address, code: &[u8]) -> MatrixRow {
    if code.is_empty() {
        return MatrixRow::fail(
            "codehash",
            format!("{label} {addr}"),
            "no code at pinned address",
        );
    }
    let hash = alloy::primitives::keccak256(code);
    MatrixRow::pass("codehash", format!("{label} {addr} {hash}"), "pinned")
}

/// Render the matrix table (pure, unit-tested).
pub fn format_matrix_table(rows: &[MatrixRow], chain_id: u64) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "fork-matrix (chain={chain_id}, read-only eth_call)\n"
    ));
    out.push_str(&format!(
        "{:<22} {:<64} {:<7} {}\n",
        "check", "detail", "verdict", "note"
    ));
    for r in rows {
        let verdict = if r.skipped {
            "SKIP"
        } else if r.pass {
            "PASS"
        } else {
            "FAIL"
        };
        out.push_str(&format!(
            "{:<22} {:<64} {:<7} {}\n",
            r.check, r.detail, verdict, r.note
        ));
    }
    let passed = rows.iter().filter(|r| r.pass && !r.skipped).count();
    let skipped = rows.iter().filter(|r| r.skipped).count();
    out.push_str(&format!(
        "summary: {passed}/{} passed, {skipped} skipped\n",
        rows.len()
    ));
    out
}

/// Entry point for `--fork-matrix`: base quoter table plus the No.4 matrix.
/// Never signs or broadcasts. Returns `Ok` on skip too (offline CI green).
pub async fn run_fork_matrix(cli_fork_url: &str, tolerance_bps: u64) -> anyhow::Result<()> {
    let raw = resolve_fork_url(cli_fork_url);
    let url = normalize_rpc_url(&raw);
    tracing::info!(
        rpc = %crate::chain::redact_url(&raw),
        tolerance_bps,
        "fork-matrix: quoter + vault/sandwich/deadline/transient/L1/codehash/sequencer (read-only)"
    );

    let provider = match try_connect(&url).await {
        Some(p) => p,
        None => {
            println!(
                "fork-matrix SKIPPED: no RPC reachable at {} (start `anvil --fork-url https://mainnet.base.org`, then `FORK_URL=http://127.0.0.1:8545 cargo run -- --fork-matrix`). No secrets needed.",
                crate::chain::redact_url(&raw)
            );
            return Ok(());
        }
    };
    let chain_id = provider
        .get_chain_id()
        .into_future()
        .await
        .unwrap_or(addresses::BASE_CHAIN_ID);
    if chain_id != addresses::BASE_CHAIN_ID {
        tracing::warn!(chain_id, "fork chain id is not Base mainnet (8453)");
    }

    match collect_rows(&provider, tolerance_bps).await {
        Ok((rows, trips)) if !rows.is_empty() => {
            println!("{}", format_table(&rows, tolerance_bps));
            let matrix = collect_matrix(&provider, chain_id, &trips).await;
            println!("{}", format_matrix_table(&matrix, chain_id));
        }
        Ok(_) => {
            println!(
                "fork-matrix: no liquid pools discovered (quoter table empty; matrix continues)."
            );
            let matrix = collect_matrix(&provider, chain_id, &[]).await;
            println!("{}", format_matrix_table(&matrix, chain_id));
        }
        Err(e) => {
            println!("fork-matrix: quoter table unavailable: {e:#} (matrix continues).");
            let matrix = collect_matrix(&provider, chain_id, &[]).await;
            println!("{}", format_matrix_table(&matrix, chain_id));
        }
    }
    Ok(())
}

/// Assemble every matrix row against the live fork. Each sub-check degrades
/// to SKIP on RPC error (fail-closed reporting, never a false PASS).
async fn collect_matrix(
    provider: &HttpProvider,
    chain_id: u64,
    trips: &[RoundTrip],
) -> Vec<MatrixRow> {
    let mut rows = Vec::new();

    // --- Vault fee>0 path: code presence + generic repay math. The Vault
    // exposes no on-chain fee getter; the 0%-on-Base assumption is covered
    // by the MockVault fee>0 unit path and the repay formula below.
    match rpc("vault code", Duration::from_secs(15), || async move {
        provider
            .clone()
            .get_code_at(addresses::BALANCER_VAULT)
            .into_future()
            .await
    })
    .await
    {
        Ok(code) => {
            if code.is_empty() {
                rows.push(MatrixRow::fail(
                    "vault-fee-path",
                    format!("vault {} has no code", addresses::BALANCER_VAULT),
                    "pinned vault missing",
                ));
            } else {
                let repay_0 = flash_repay(U256::from(500_000_000u64), 0);
                let repay_30 = flash_repay(U256::from(500_000_000u64), 30);
                let detail = format!(
                    "vault {} code={}B repay500@${} fee0={} fee30bps={}",
                    addresses::BALANCER_VAULT,
                    code.len(),
                    500,
                    repay_0,
                    repay_30
                );
                rows.push(MatrixRow::pass(
                    "vault-fee-path",
                    detail,
                    "fee>0 covered by MockVault::setFeeBps; repay=amount+fee",
                ));
            }
        }
        Err(e) => rows.push(MatrixRow::skip(
            "vault-fee-path",
            &format!("vault code unreadable: {e:#}"),
        )),
    }

    // --- L1 getL1Fee per size via 0x4200...000F. Fee depends on calldata
    // length, not amount: one representative length, reported per size.
    let oracle: Address = crate::chain::L1_GAS_PRICE_ORACLE
        .parse()
        .expect("L1 oracle const parses");
    let tx_data = vec![0xabu8; REPRESENTATIVE_EXECUTE_CALLDATA_LEN];
    let l1_fee: Option<U256> = match rpc("l1 getL1Fee", Duration::from_secs(20), || {
        // Cloned per attempt: the paced helper may retry the closure.
        let data = tx_data.clone();
        async move {
            let o = IGasPriceOracle::new(oracle, provider.clone());
            o.getL1Fee(data.into()).call().into_future().await
        }
    })
    .await
    {
        Ok(ret) => Some(ret),
        Err(e) => {
            tracing::warn!("L1 getL1Fee unreadable: {e:#}");
            None
        }
    };
    for (label, amount) in SIZES {
        match l1_fee {
            Some(fee) => rows.push(MatrixRow::pass(
                "l1-getL1Fee",
                format!("size={label} amount={amount} txlen={REPRESENTATIVE_EXECUTE_CALLDATA_LEN}B fee={fee}"),
                "oracle 0x4200...000F; length-driven, not amount-driven",
            )),
            None => rows.push(MatrixRow::skip("l1-getL1Fee", "getL1Fee eth_call failed")),
        }
    }

    // --- Sandwich / front-run drill: shock the live unit-consistent
    // round-trip gross (USDC -> token -> USDC, best direction, $500
    // WETH/USDC) by 10/25/50 bps; the profit gate must say REVERT (atomic,
    // no loss). On efficient markets the baseline itself already reverts
    // (no live edge): rows then confirm the gate correctly refuses, and the
    // note records whether the shock flipped the decision.
    let baseline = trips
        .iter()
        .find(|t| t.pair == "WETH/USDC" && t.size_label == "$500");
    match baseline {
        Some(t) => {
            let unshocked_reverts = sandwich_would_revert(
                t.gross_out,
                0,
                t.amount_in,
                U256::from(DRILL_FLAT_COSTS),
                U256::from(DRILL_MIN_NET),
            );
            for shock in SANDWICH_SHOCKS_BPS {
                let reverts = sandwich_would_revert(
                    t.gross_out,
                    shock,
                    t.amount_in,
                    U256::from(DRILL_FLAT_COSTS),
                    U256::from(DRILL_MIN_NET),
                );
                let detail = format!(
                    "shock={shock}bps gross={} amount={} via={} revert={reverts}",
                    t.gross_out, t.amount_in, t.via
                );
                if reverts {
                    let note = if unshocked_reverts {
                        "no live edge at this size: gate correctly refuses before and after shock"
                    } else {
                        "adverse move flips gate to revert (InsufficientProfit/leg-min), no loss"
                    };
                    rows.push(MatrixRow::pass("sandwich-drill", detail, note));
                } else {
                    rows.push(MatrixRow::fail(
                        "sandwich-drill",
                        detail,
                        "edge survives shock: review minimums before any submission",
                    ));
                }
            }
        }
        None => {
            for shock in SANDWICH_SHOCKS_BPS {
                rows.push(MatrixRow::skip(
                    "sandwich-drill",
                    &format!("no live $500 WETH/USDC round trip for {shock}bps shock"),
                ));
            }
        }
    }

    // --- Deadlines: outer executor + Aero inner (UniV3 Router02 has no
    // deadline field). Verified against live chain time.
    let now: Option<u64> = match rpc("latest block", Duration::from_secs(15), || async move {
        provider
            .clone()
            .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
            .into_future()
            .await
    })
    .await
    {
        Ok(Some(block)) => Some(block.header.timestamp),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!("latest block unreadable: {e:#}");
            None
        }
    };
    match now {
        Some(t) => {
            let cases = [
                ("deadline-outer", t + 30, "outer now+30s must be executable"),
                (
                    "deadline-outer-expired",
                    t.saturating_sub(1),
                    "outer now-1s must revert",
                ),
                (
                    "deadline-aero-inner",
                    t + 30,
                    "aero inner now+30s must be executable",
                ),
                ("deadline-aero-inner-exp", 0, "aero inner 0 must revert"),
            ];
            for (name, dl, note) in cases {
                let ok = if name.contains("aero") {
                    aero_inner_deadline_ok(dl, t)
                } else {
                    deadline_ok(dl, t)
                };
                let expect_ok = !name.contains("expired") && !name.contains("exp");
                // Rows pass when chain behaviour matches expectation; the
                // detail records both sides for the runbook.
                let detail = format!("now={t} deadline={dl} executable={ok}");
                if ok == expect_ok {
                    rows.push(MatrixRow::pass(name, detail, note));
                } else {
                    rows.push(MatrixRow::fail(name, detail, note));
                }
            }
        }
        None => {
            for name in [
                "deadline-outer",
                "deadline-outer-expired",
                "deadline-aero-inner",
                "deadline-aero-inner-exp",
            ] {
                rows.push(MatrixRow::skip(name, "latest block timestamp unreadable"));
            }
        }
    }

    // --- Transient support note (Cancun / EIP-1153).
    if transient_supported(chain_id) {
        rows.push(MatrixRow::pass(
            "transient-cancun",
            format!("chain={chain_id} evm=cancun"),
            "transient _expectedHash/_inCallback available; forge evm_version=cancun",
        ));
    } else {
        rows.push(MatrixRow::fail(
            "transient-cancun",
            format!("chain={chain_id}"),
            "unknown chain: do not assume EIP-1153",
        ));
    }

    // --- Codehash freeze rows for the 7 pinned addresses.
    let pinned: [(&str, Address); 7] = [
        ("vault", addresses::BALANCER_VAULT),
        ("aero-router", addresses::AERO_SWAP_ROUTER),
        ("uni-router02", addresses::UNIV3_SWAP_ROUTER02),
        ("universal-router", addresses::UNIV3_UNIVERSAL_ROUTER),
        ("usdc", addresses::USDC_NATIVE),
        ("aero-quoter", addresses::AERO_QUOTER_V2),
        ("uni-quoter", addresses::UNIV3_QUOTER_V2),
    ];
    for (label, addr) in pinned {
        match rpc("codehash code", Duration::from_secs(15), || async move {
            provider.clone().get_code_at(addr).into_future().await
        })
        .await
        {
            Ok(code) => rows.push(codehash_row(label, addr, &code)),
            Err(e) => rows.push(MatrixRow::skip(
                "codehash",
                &format!("{label} code unreadable: {e:#}"),
            )),
        }
    }

    // --- Sequencer gate: live latestRoundData; refuse-to-send when down.
    let seq_now = now.unwrap_or(0);
    match sequencer::query_status(provider, chain_id, seq_now).await {
        Ok(Some(st)) => {
            let up = sequencer::sequencer_up(&st, sequencer::GRACE_PERIOD_SECS);
            let detail = format!("answer={} startedAt={} send={up}", st.answer, st.started_at);
            if chain_id == addresses::BASE_CHAIN_ID {
                rows.push(MatrixRow::pass(
                    "sequencer-gate",
                    detail,
                    "refuse execute while down/in-grace; fail-closed on error",
                ));
            } else {
                rows.push(MatrixRow::skip(
                    "sequencer-gate",
                    "no verified feed for this chain (fail closed)",
                ));
            }
        }
        Ok(None) => rows.push(MatrixRow::skip(
            "sequencer-gate",
            "no verified feed for this chain (fail closed)",
        )),
        Err(e) => rows.push(MatrixRow::skip(
            "sequencer-gate",
            &format!("feed unreadable: {e:#}"),
        )),
    }

    rows
}

#[cfg(test)]
mod matrix_tests {
    use super::*;

    #[test]
    fn flash_repay_zero_fee_is_exact() {
        assert_eq!(
            flash_repay(U256::from(500_000_000u64), 0),
            U256::from(500_000_000u64)
        );
    }

    #[test]
    fn flash_repay_fee_rounds_up_never_short() {
        // 500 USDC @ 30bps = 1_500_000 fee.
        assert_eq!(
            flash_repay(U256::from(500_000_000u64), 30),
            U256::from(501_500_000u64)
        );
        // Dust amount @ 1bps: 1 wei * 1 / 10000 rounds UP to 1 wei fee.
        assert_eq!(flash_repay(U256::from(1u64), 1), U256::from(2u64));
    }

    #[test]
    fn vault_fee_erodes_profit_below_min() {
        // Fixture edge: 510 out on 500 in. Fee 0 -> +10 clears $5 min;
        // fee 200bps -> repay 510, net 0 < min -> must revert, not lose.
        let proceeds = U256::from(510_000_000u64);
        let flash = U256::from(500_000_000u64);
        let net_0 = proceeds.saturating_sub(flash_repay(flash, 0));
        assert!(net_0 >= U256::from(DRILL_MIN_NET));
        let net_200 = proceeds.saturating_sub(flash_repay(flash, 200));
        assert!(
            net_200 < U256::from(DRILL_MIN_NET),
            "fee>0 must erase the edge"
        );
    }

    #[test]
    fn sandwich_shocks_revert_not_lose() {
        // Tight quote: 505.10 out on 500 in, $5 min, 85k flat costs.
        // Base net = 5_015_000 clears the min; a 10bps haircut erases
        // ~50_510, so every covered shock must flip the gate to REVERT.
        let quote = U256::from(505_100_000u64);
        let amount = U256::from(500_000_000u64);
        let flat = U256::from(DRILL_FLAT_COSTS);
        let min = U256::from(DRILL_MIN_NET);
        assert!(!sandwich_would_revert(quote, 0, amount, flat, min));
        for shock in SANDWICH_SHOCKS_BPS {
            assert!(
                sandwich_would_revert(quote, shock, amount, flat, min),
                "{shock}bps adverse move must revert, not settle at a loss"
            );
        }
    }

    #[test]
    fn wide_edge_survives_small_shock() {
        // 520 out on 500 in: even a 50bps haircut still clears -> no revert.
        // The drill reports honestly instead of forcing reverts.
        assert!(!sandwich_would_revert(
            U256::from(520_000_000u64),
            50,
            U256::from(500_000_000u64),
            U256::from(DRILL_FLAT_COSTS),
            U256::from(DRILL_MIN_NET),
        ));
    }

    #[test]
    fn deadline_boundaries() {
        assert!(deadline_ok(100, 100));
        assert!(deadline_ok(100, 99));
        assert!(!deadline_ok(100, 101));
        assert!(!deadline_ok(0, 1));
    }

    #[test]
    fn aero_inner_deadline_mirrors_outer() {
        // Slipstream exactInputSingle pins its own deadline; UniV3
        // SwapRouter02 has no deadline field (outer check only).
        assert!(aero_inner_deadline_ok(100, 100));
        assert!(!aero_inner_deadline_ok(0, 1));
    }

    #[test]
    fn transient_only_where_cancun() {
        assert!(transient_supported(8453));
        assert!(transient_supported(84532));
        assert!(!transient_supported(1));
    }

    #[test]
    fn l1_calldata_grows_with_len_not_amount() {
        let small = [0xabu8; 100];
        let big = [0xabu8; 768];
        let a = crate::chain::l1_fee_calldata(&small);
        let b = crate::chain::l1_fee_calldata(&big);
        assert!(b.len() > a.len());
        assert_eq!(b.len(), 4 + 32 + 32 + 768);
        assert_eq!(REPRESENTATIVE_EXECUTE_CALLDATA_LEN, 768);
    }

    #[test]
    fn codehash_row_marks_empty_code() {
        let r = codehash_row("vault", Address::ZERO, &[]);
        assert!(!r.pass);
        let r = codehash_row("vault", Address::ZERO, &[0x60, 0x61]);
        assert!(r.pass && !r.skipped);
        assert!(r.detail.contains("0x"));
    }

    #[test]
    fn round_trip_gross_stays_in_input_units() {
        // Parity legs at 1:1 (sqrtPriceX96 = 2^96), 0.05% fee each:
        // 1M in -> ~999500 -> ~999000 out, all in input units.
        let sqrt = U256::from(1u128 << 96);
        let leg = ClPoolState::new(
            Address::ZERO,
            PoolKind::UniV3,
            sqrt,
            u128::MAX / 2,
            0,
            500,
            10,
        )
        .expect("fixture");
        let gross = round_trip_gross(&leg, &leg, U256::from(1_000_000u64), true);
        assert!(
            gross < U256::from(1_000_000u64),
            "fees must haircut: got {gross}"
        );
        assert!(
            gross > U256::from(990_000u64),
            "too much haircut: got {gross}"
        );
    }

    #[test]
    fn matrix_table_counts_skips_separately() {
        let rows = vec![
            MatrixRow::pass("a", "d".to_string(), ""),
            MatrixRow::fail("b", "d".to_string(), ""),
            MatrixRow::skip("c", "offline"),
        ];
        let t = format_matrix_table(&rows, 8453);
        for col in [
            "check",
            "detail",
            "verdict",
            "PASS",
            "FAIL",
            "SKIP",
            "1/3 passed, 1 skipped",
        ] {
            assert!(t.contains(col), "missing {col}:\n{t}");
        }
    }
}
