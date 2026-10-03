//! Discord alerts transport (dry-run safe).
//!
//! - Pure formatter ([`format_discord_message`]) + best-effort
//!   [`WebhookSender`]. Amounts only, never secrets.
//! - Webhook URL comes ONLY from `DISCORD_WEBHOOK_URL` at runtime. It is
//!   never logged (see [`redact_webhook_url`] / [`WebhookSender::endpoint_label`]),
//!   never read from a config file, never committed.
//! - Empty URL = log-to-stdout fallback so `cargo test` and drills stay
//!   offline.
//! - Rate-limit: minimum 5 s between posts; excess alerts are dropped (never
//!   queued, never blocking the trading hot path).
//! - Send failures are swallowed (warn only): alerts must never break the
//!   paper loop or the kill switch.

use std::time::{Duration, Instant};

/// Minimum interval between two webhook POSTs. Excess alerts are dropped.
pub const MIN_ALERT_INTERVAL: Duration = Duration::from_secs(5);

/// Upper bound for a Discord message produced here (Discord caps at 2000;
/// we stay phone-friendly well below it).
pub const MAX_MESSAGE_CHARS: usize = 1800;

/// Opportunity alert: amounts only, no keys, no URLs with tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpportunityAlert {
    /// Allowlisted pair label, e.g. "WETH/USDC".
    pub pair: String,
    /// Direction label, e.g. "Slipstream->UniV3" or "A->B".
    pub direction: String,
    /// Estimated spread in basis points (100 bps = 1%).
    pub spread_bps: u64,
    /// Flash size in USDC base units (6 decimals).
    pub size_usdc: u64,
    /// Estimated net profit in USDC base units (6 decimals).
    pub net_usdc: u64,
    /// Human fee description, e.g. "slip 3bps + v3 5bps". Amounts only.
    pub fee_pp: String,
    /// Placeholder link / label. Paper mode uses a non-URL placeholder
    /// (e.g. "paper-dry-run:no-tx"); live drills use a Basescan URL WITHOUT
    /// any secret query string.
    pub tx_link: String,
}

/// Format USDC base units (6 decimals) as `$D.CC` without floats.
pub fn fmt_usdc(base_units: u64) -> String {
    let dollars = base_units / 1_000_000;
    let cents = (base_units % 1_000_000) / 10_000;
    format!("${dollars}.{cents:02}")
}

/// Truncate to `max` chars on a char boundary.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

/// Phone-friendly English alert text. Always < [`MAX_MESSAGE_CHARS`]
/// chars, amounts only, no secrets.
pub fn format_discord_message(a: &OpportunityAlert) -> String {
    let pair = truncate_chars(a.pair.trim(), 64);
    let pair = if pair.is_empty() { "?" } else { pair.as_str() };
    let dir = truncate_chars(a.direction.trim(), 64);
    let dir = if dir.is_empty() { "?" } else { dir.as_str() };
    let fee = truncate_chars(a.fee_pp.trim(), 128);
    let fee = if fee.is_empty() { "-" } else { fee.as_str() };
    let link = truncate_chars(a.tx_link.trim(), 256);
    let link = if link.is_empty() {
        "paper-dry-run:no-tx"
    } else {
        link.as_str()
    };
    let msg = format!(
        "🤖 ARB OPPORTUNITY (paper/dry-run)\n\
         Pair: {pair}\n\
         Direction: {dir}\n\
         Spread: {} bps\n\
         Size: {}\n\
         Est net: {}\n\
         Fee: {fee}\n\
         Link: {link}\n\
         ⚠️ Dry-run: no funds moved.",
        a.spread_bps,
        fmt_usdc(a.size_usdc),
        fmt_usdc(a.net_usdc),
    );
    if msg.chars().count() <= MAX_MESSAGE_CHARS {
        return msg;
    }
    // Defensive: hard-truncate on a char boundary (fields above already
    // bound the length, so this is unreachable in practice).
    truncate_chars(&msg, MAX_MESSAGE_CHARS)
}

/// Redact a Discord webhook URL for logs: `scheme://host` only. The path
/// holds `{id}/{token}`, so it must never appear in logs or metrics.
/// Public helper for ops/debugging; unit-tested below.
#[allow(dead_code)]
pub fn redact_webhook_url(url: &str) -> String {
    let scheme = url.split_once("://").map(|(s, _)| s).unwrap_or("https");
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    // Drop userinfo if ever present, then drop path/query (id + token).
    let no_userinfo = after.split('@').next_back().unwrap_or(after);
    let host = no_userinfo.split(['/', '?']).next().unwrap_or("?");
    format!("{scheme}://{host}")
}

/// Best-effort Discord webhook sender.
///
/// - URL from `DISCORD_WEBHOOK_URL` only (see [`WebhookSender::from_env`]).
/// - Empty URL = stdout fallback (offline-safe).
/// - Rate-limited (drops, never blocks); failures only warn.
pub struct WebhookSender {
    /// Full webhook URL. NEVER logged; use [`WebhookSender::endpoint_label`].
    url: String,
    client: reqwest::Client,
    last_sent: Option<Instant>,
    min_interval: Duration,
}

impl WebhookSender {
    /// Build from an explicit URL string (env is read by [`Self::from_env`]).
    pub fn new(url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            url,
            client,
            last_sent: None,
            min_interval: MIN_ALERT_INTERVAL,
        }
    }

    /// Read `DISCORD_WEBHOOK_URL` from the environment. Missing/empty =
    /// stdout fallback (never an error, so tests stay offline).
    pub fn from_env() -> Self {
        Self::new(std::env::var("DISCORD_WEBHOOK_URL").unwrap_or_default())
    }

    /// True when a real webhook URL is configured.
    pub fn is_live(&self) -> bool {
        !self.url.trim().is_empty()
    }

    /// Log-safe endpoint label. NEVER contains the URL, id, or token.
    pub fn endpoint_label(&self) -> &'static str {
        if self.is_live() {
            "discord-webhook:configured"
        } else {
            "discord-webhook:stdout-fallback"
        }
    }

    /// Rate-gate: true when a post may be attempted now. Pure (no I/O) so
    /// unit tests stay offline.
    pub fn should_send_now(&self) -> bool {
        match self.last_sent {
            None => true,
            Some(t) => t.elapsed() >= self.min_interval,
        }
    }

    /// Test hook: override the minimum interval.
    #[cfg(test)]
    fn with_interval(mut self, d: Duration) -> Self {
        self.min_interval = d;
        self
    }

    /// Best-effort send. Never returns an error: empty URL prints to stdout,
    /// rate-limited posts are dropped, network failures only warn.
    pub async fn send(&mut self, msg: &str) {
        if !self.is_live() {
            // Offline fallback: stdout only, full text is safe (no secrets
            // by construction of `format_discord_message`).
            println!("[discord-stdout-fallback]\n{msg}");
            tracing::info!(endpoint = self.endpoint_label(), "alert to stdout fallback");
            return;
        }
        if !self.should_send_now() {
            tracing::warn!(
                endpoint = self.endpoint_label(),
                "alert rate-limited, dropped (min 5s between posts)"
            );
            return;
        }
        self.last_sent = Some(Instant::now());
        let body = serde_json::json!({ "content": msg });
        match self.client.post(self.url.clone()).json(&body).send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    tracing::info!(endpoint = self.endpoint_label(), "alert posted");
                } else {
                    // Status only: never log headers/body (may echo infra).
                    tracing::warn!(
                        endpoint = self.endpoint_label(),
                        status = %resp.status(),
                        "discord webhook non-2xx, dropped"
                    );
                }
            }
            Err(e) => {
                // `reqwest::Error` Display never includes the URL; still, log
                // only the status-ish string, not the full error chain.
                tracing::warn!(endpoint = self.endpoint_label(), error = %e, "alert post failed, dropped");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> OpportunityAlert {
        OpportunityAlert {
            pair: "WETH/USDC".to_string(),
            direction: "Slipstream->UniV3".to_string(),
            spread_bps: 200,
            size_usdc: 500_000_000,
            net_usdc: 5_120_000,
            fee_pp: "slip 3bps + v3 5bps".to_string(),
            tx_link: "paper-dry-run:no-tx".to_string(),
        }
    }

    #[test]
    fn fmt_usdc_no_floats() {
        assert_eq!(fmt_usdc(500_000_000), "$500.00");
        assert_eq!(fmt_usdc(5_120_000), "$5.12");
        assert_eq!(fmt_usdc(0), "$0.00");
        assert_eq!(fmt_usdc(1), "$0.00"); // sub-cent truncates, never rounds up
        assert_eq!(fmt_usdc(10_000), "$0.01");
    }

    #[test]
    fn formatter_english_phone_friendly() {
        let msg = format_discord_message(&sample());
        assert!(msg.chars().count() < MAX_MESSAGE_CHARS, "too long");
        assert!(msg.chars().count() < 1800, "must stay <1800 chars");
        assert!(msg.contains("OPPORTUNITY"), "English header: {msg}");
        assert!(msg.contains("Direction:"), "direction label: {msg}");
        assert!(msg.contains("WETH/USDC"), "pair: {msg}");
        assert!(msg.contains("$500.00"), "size: {msg}");
        assert!(msg.contains("$5.12"), "net: {msg}");
        assert!(msg.contains("no funds moved"), "dry-run disclaimer: {msg}");
    }

    #[test]
    fn formatter_bounds_untrusted_fields() {
        // Callers must pass non-secret labels (paper uses a placeholder);
        // the formatter's guarantee is a hard length bound so an over-long
        // field can never smuggle a full token into a message.
        let long_token = "S".repeat(300);
        let mut a = sample();
        a.fee_pp = "x".repeat(5000);
        a.tx_link = format!("https://discord.com/api/webhooks/ID/{long_token}");
        let msg = format_discord_message(&a);
        assert!(msg.chars().count() <= MAX_MESSAGE_CHARS, "bounded");
        assert!(
            !msg.contains(&long_token),
            "over-long field must be truncated"
        );
    }

    #[test]
    fn formatter_handles_empty_fields() {
        let a = OpportunityAlert {
            pair: String::new(),
            direction: String::new(),
            spread_bps: 0,
            size_usdc: 0,
            net_usdc: 0,
            fee_pp: String::new(),
            tx_link: String::new(),
        };
        let msg = format_discord_message(&a);
        assert!(msg.contains("?"), "placeholder for empty: {msg}");
        assert!(msg.contains("paper-dry-run:no-tx"), "link fallback: {msg}");
    }

    #[test]
    fn rate_limiter_drops_fast_repeat() {
        let live = WebhookSender::new("https://discord.com/api/webhooks/ID/TOKEN".to_string());
        assert!(live.should_send_now(), "first post allowed");
        let mut limited =
            WebhookSender::new("https://discord.com/api/webhooks/ID/TOKEN".to_string());
        limited.last_sent = Some(Instant::now());
        assert!(!limited.should_send_now(), "immediate repeat dropped");
        let mut old = WebhookSender::new("https://discord.com/api/webhooks/ID/TOKEN".to_string())
            .with_interval(Duration::from_millis(1));
        old.last_sent = Some(Instant::now() - Duration::from_secs(60));
        assert!(old.should_send_now(), "old timestamp allowed");
    }

    #[test]
    fn empty_url_is_stdout_fallback() {
        let s = WebhookSender::new(String::new());
        assert!(!s.is_live());
        assert_eq!(s.endpoint_label(), "discord-webhook:stdout-fallback");
        let live = WebhookSender::new("https://discord.com/api/webhooks/ID/TOKEN".to_string());
        assert!(live.is_live());
        assert_eq!(live.endpoint_label(), "discord-webhook:configured");
    }

    #[test]
    fn url_redaction_strips_token() {
        // Obviously-fake placeholders (non-numeric id) so the repo hygiene
        // grep for real webhook URLs stays green.
        let red = redact_webhook_url("https://discord.com/api/webhooks/WEBHOOK_ID/FAKE_TOKEN_XYZ");
        assert!(!red.contains("FAKE_TOKEN_XYZ"), "token leaked: {red}");
        assert!(!red.contains("WEBHOOK_ID"), "webhook id leaked: {red}");
        assert!(red.contains("discord.com"), "host kept: {red}");
        assert_eq!(red, "https://discord.com");
    }

    #[test]
    fn redaction_never_panics_on_garbage() {
        // Empty/garbage input must not panic; output keeps scheme://host
        // shape only (the path, where id/token live, is always dropped).
        assert_eq!(redact_webhook_url(""), "https://");
        assert_eq!(
            redact_webhook_url("ws://127.0.0.1:8545"),
            "ws://127.0.0.1:8545"
        );
    }

    #[tokio::test]
    async fn send_with_empty_url_stays_offline() {
        // No network: empty URL path only prints to stdout.
        let mut s = WebhookSender::new(String::new());
        s.send("offline drill").await;
        assert!(!s.is_live());
    }
}
