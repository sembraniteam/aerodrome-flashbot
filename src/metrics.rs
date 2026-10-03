//! Observability: counters, rates, latency histograms, CSV export.
//!
//! Hot-path rules: no locks held across `.await` (this type is lock-free by
//! construction: plain counters updated on the hot task, flushed
//! asynchronously by the writer), bounded memory (latency ring capped).

use std::collections::{BTreeMap, VecDeque};

/// Per-pair/direction counters for the CSV breakdown section.
#[derive(Debug, Clone, Default)]
pub struct PairStats {
    /// Candidates seen for this (pair, direction).
    pub seen: u64,
    /// Candidates that reached simulation.
    pub simulated: u64,
    /// Simulations that cleared (profitable).
    pub won: u64,
    /// Best net profit observed (profit-token base units, signed).
    pub best_net: i128,
}

/// Rolling metrics for the paper/live runner.
#[derive(Debug, Default)]
pub struct Metrics {
    pub opportunities_seen: u64,
    pub attempts_simulated: u64,
    pub attempts_won: u64,
    pub reverts: u64,
    pub quotes_diverged: u64,
    pub rejected_by_risk: u64,
    /// Net PnL in profit-token base units (signed).
    pub net_pnl: i128,
    /// Total gas + L1 spend (base units).
    pub fee_spend: u128,
    feed_lag_ms_samples: VecDeque<u64>,
    stage_latency_us: Vec<StageLatency>,
    /// Breakdown keyed by (pair, direction). Bounded in practice by the
    /// allowlist size times two directions; directions are short labels
    /// (`A->B->A`, `B->A->B`, or `-` when undecided).
    pair_stats: BTreeMap<(String, String), PairStats>,
}

#[derive(Debug, Clone)]
pub struct StageLatency {
    pub stage: &'static str,
    pub micros: u64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_seen(&mut self) {
        self.opportunities_seen += 1;
    }

    pub fn record_simulated(&mut self, won: bool, reverted: bool, net: i128, fees: u128) {
        self.attempts_simulated += 1;
        if won {
            self.attempts_won += 1;
        }
        if reverted {
            self.reverts += 1;
        }
        self.net_pnl = self.net_pnl.saturating_add(net);
        self.fee_spend = self.fee_spend.saturating_add(fees);
    }

    pub fn record_diverged(&mut self) {
        self.quotes_diverged += 1;
    }

    pub fn record_risk_reject(&mut self) {
        self.rejected_by_risk += 1;
    }

    /// Record that a (pair, direction) candidate was seen (pre-simulation).
    /// `direction` is `-` when undecided (early exits before the
    /// two-direction harness runs).
    pub fn record_pair_seen(&mut self, pair: &str, direction: &str) {
        self.pair_stats
            .entry((pair.to_string(), direction.to_string()))
            .or_default()
            .seen += 1;
    }

    /// Record a (pair, direction) simulation outcome. `net` is the clearing
    /// net on wins, 0 otherwise; `best_net` keeps the maximum.
    pub fn record_pair_sim(&mut self, pair: &str, direction: &str, won: bool, net: i128) {
        let entry = self
            .pair_stats
            .entry((pair.to_string(), direction.to_string()))
            .or_default();
        entry.simulated += 1;
        if won {
            entry.won += 1;
        }
        entry.best_net = entry.best_net.max(net);
    }

    /// Read-only access to one breakdown cell (for tests/callers).
    pub fn pair_stats(&self, pair: &str, direction: &str) -> Option<&PairStats> {
        self.pair_stats
            .get(&(pair.to_string(), direction.to_string()))
    }

    pub fn record_feed_lag_ms(&mut self, lag_ms: u64) {
        // Bounded ring: keep the last 10k samples, drop stale work.
        if self.feed_lag_ms_samples.len() >= 10_000 {
            self.feed_lag_ms_samples.pop_front();
        }
        self.feed_lag_ms_samples.push_back(lag_ms);
    }

    pub fn record_stage_latency(&mut self, stage: &'static str, micros: u64) {
        if self.stage_latency_us.len() < 50_000 {
            self.stage_latency_us.push(StageLatency { stage, micros });
        }
    }

    /// Fraction of simulated attempts that were profitable.
    pub fn hit_rate(&self) -> f64 {
        if self.attempts_simulated == 0 {
            return 0.0;
        }
        self.attempts_won as f64 / self.attempts_simulated as f64
    }

    /// Fraction of simulated attempts that would revert.
    pub fn revert_rate(&self) -> f64 {
        if self.attempts_simulated == 0 {
            return 0.0;
        }
        self.reverts as f64 / self.attempts_simulated as f64
    }

    /// p95 latency in microseconds for a stage (0.0 when no samples).
    /// Floats are display-only; never feed back into the profit path.
    pub fn p95_latency_us(&self, stage: &str) -> f64 {
        let mut vals: Vec<u64> = self
            .stage_latency_us
            .iter()
            .filter(|s| s.stage == stage)
            .map(|s| s.micros)
            .collect();
        if vals.is_empty() {
            return 0.0;
        }
        vals.sort_unstable();
        let idx = ((vals.len() as f64) * 0.95).ceil() as usize - 1;
        vals[idx.min(vals.len() - 1)] as f64
    }

    /// One-line summary for stdout logs (no secrets: counts and rates only).
    pub fn summary(&self) -> String {
        format!(
            "seen={} sim={} won={} hit_rate={:.3} revert_rate={:.3} risk_rejects={} diverged={} net_pnl={} fee_spend={}",
            self.opportunities_seen,
            self.attempts_simulated,
            self.attempts_won,
            self.hit_rate(),
            self.revert_rate(),
            self.rejected_by_risk,
            self.quotes_diverged,
            self.net_pnl,
            self.fee_spend
        )
    }

    /// Append a summary row to a CSV writer (stdout/file sink owned by the
    /// caller; hot path never blocks on it).
    pub fn write_csv_row(&self, w: &mut csv::Writer<impl std::io::Write>) -> csv::Result<()> {
        w.write_record([
            self.opportunities_seen.to_string(),
            self.attempts_simulated.to_string(),
            self.attempts_won.to_string(),
            format!("{:.4}", self.hit_rate()),
            format!("{:.4}", self.revert_rate()),
            self.rejected_by_risk.to_string(),
            self.quotes_diverged.to_string(),
            self.net_pnl.to_string(),
            self.fee_spend.to_string(),
        ])?;
        w.flush()?;
        Ok(())
    }

    /// Append the per-pair/direction breakdown section after the summary row:
    /// a `pair,direction,...` header plus one row per observed cell. The
    /// summary row written by [`Metrics::write_csv_row`] is always kept.
    /// Widths differ between sections, so the caller must build the writer
    /// with `csv::WriterBuilder::new().flexible(true)`.
    pub fn write_csv_breakdown(&self, w: &mut csv::Writer<impl std::io::Write>) -> csv::Result<()> {
        w.write_record(["pair", "direction", "seen", "simulated", "won", "best_net"])?;
        for ((pair, direction), stats) in &self.pair_stats {
            w.write_record([
                pair.as_str(),
                direction.as_str(),
                &stats.seen.to_string(),
                &stats.simulated.to_string(),
                &stats.won.to_string(),
                &stats.best_net.to_string(),
            ])?;
        }
        w.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_empty_is_zero() {
        let m = Metrics::new();
        assert_eq!(m.hit_rate(), 0.0);
        assert_eq!(m.revert_rate(), 0.0);
        assert_eq!(m.p95_latency_us("sim"), 0.0);
    }

    #[test]
    fn hit_and_revert_rates() {
        let mut m = Metrics::new();
        m.record_seen();
        m.record_simulated(true, false, 100, 10);
        m.record_simulated(false, true, -50, 10);
        assert!((m.hit_rate() - 0.5).abs() < 1e-9);
        assert!((m.revert_rate() - 0.5).abs() < 1e-9);
        assert_eq!(m.net_pnl, 50);
        assert_eq!(m.fee_spend, 20);
    }

    #[test]
    fn p95_picks_tail() {
        let mut m = Metrics::new();
        for i in 1u64..=100 {
            m.record_stage_latency("sim", i);
        }
        assert_eq!(m.p95_latency_us("sim"), 95.0);
        assert_eq!(m.p95_latency_us("feed"), 0.0);
    }

    #[test]
    fn pair_breakdown_tracks_seen_sim_won_best() {
        let mut m = Metrics::new();
        m.record_pair_seen("WETH/USDC", "-");
        m.record_pair_seen("WETH/USDC", "A->B->A");
        m.record_pair_sim("WETH/USDC", "A->B->A", true, 100);
        m.record_pair_sim("WETH/USDC", "A->B->A", false, 0);
        m.record_pair_sim("AERO/USDC", "B->A->B", true, 50);
        let weth = m.pair_stats("WETH/USDC", "A->B->A").expect("cell exists");
        assert_eq!(weth.seen, 1);
        assert_eq!(weth.simulated, 2);
        assert_eq!(weth.won, 1);
        assert_eq!(weth.best_net, 100);
        let undecided = m.pair_stats("WETH/USDC", "-").expect("cell exists");
        assert_eq!(undecided.seen, 1);
        assert_eq!(undecided.simulated, 0);
        assert!(m.pair_stats("WETH/USDC", "nope").is_none());
    }

    #[test]
    fn csv_keeps_summary_row_then_breakdown() {
        let mut m = Metrics::new();
        m.record_seen();
        m.record_simulated(true, false, 100, 10);
        m.record_pair_seen("WETH/USDC", "-");
        m.record_pair_sim("WETH/USDC", "A->B->A", true, 100);
        let mut buf = Vec::new();
        {
            // Flexible widths: summary (9 fields) + breakdown (6 fields).
            let mut w = csv::WriterBuilder::new()
                .flexible(true)
                .from_writer(&mut buf);
            m.write_csv_row(&mut w).expect("summary writes");
            m.write_csv_breakdown(&mut w).expect("breakdown writes");
        }
        let body = String::from_utf8(buf).expect("csv is utf8");
        let lines: Vec<&str> = body.lines().collect();
        // summary row first (9 fields), then breakdown header + 2 detail rows.
        assert_eq!(
            lines.len(),
            1 + 1 + 2,
            "summary + header + 2 cells:\n{body}"
        );
        assert!(lines[0].starts_with("1,1,1,"), "summary first:\n{body}");
        assert!(
            lines[1].starts_with("pair,direction,seen,"),
            "breakdown header:\n{body}"
        );
        assert!(
            body.contains("WETH/USDC,A->B->A,0,1,1,100"),
            "detail row:\n{body}"
        );
        assert!(
            body.contains("WETH/USDC,-,1,0,0,0"),
            "undecided row:\n{body}"
        );
    }
}
