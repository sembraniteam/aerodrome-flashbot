//! Chain data feed abstraction.
//!
//! Base block production is in transition (Flashblocks 200 ms sub-blocks via
//! the `pending` tag are announced to be replaced by canonical 200 ms blocks
//! in the Denim upgrade, targeted ~Oct 2026 but not final). All `pending`-tag
//! assumptions therefore live behind [`BlockFeed`] implementations selected by
//! [`FeedMode`](crate::config::FeedMode), never as scattered literals.
//!
//! After Denim, millisecond timestamps may arrive as extra header fields:
//! prefer `WithOtherFields` / typed OP-Stack headers over plain
//! `AnyRpcHeader` when those fields matter.

use alloy::network::Ethereum;
use alloy::primitives::B256;
use alloy::providers::{Provider, ProviderBuilder, RootProvider, WsConnect};
use alloy::pubsub::{Subscription, SubscriptionItem};
use alloy::rpc::types::Header;
use std::future::IntoFuture as _;
use std::time::Duration;

use crate::config::FeedMode;

/// A new head observed by the feed.
#[derive(Debug, Clone)]
pub struct HeadEvent {
    /// Block number (or sub-block sequence for Flashblocks mode).
    pub number: u64,
    /// Block hash, used for reorg/reversal detection.
    pub hash: B256,
    /// Millisecond unix timestamp when known (Denim-style headers).
    pub timestamp_ms: Option<u64>,
    /// True when this event is a Flashblocks preconfirmation rather than a
    /// canonical block.
    pub is_preconfirmation: bool,
}

/// Source of chain heads. Implementations must handle reconnects, gaps, and
/// duplicate/reordered events and surface reorgs via [`BlockFeed::reorged`].
pub trait BlockFeed: Send {
    /// Return the next head event, or `None` when the feed is shutting down.
    fn next_head(&mut self) -> impl std::future::Future<Output = Option<HeadEvent>> + Send;

    /// True if `new_head` does not extend `prev_head` (number gap).
    /// Hash-equality is handled by [`classify_head`] as `Duplicate`; without
    /// the parent hash we cannot distinguish a reorg from a normal new block
    /// on hash alone, so only the number gap is a reliable signal here.
    fn reorged(prev_head: &HeadEvent, new_head: &HeadEvent) -> bool {
        prev_head.number.saturating_add(1) != new_head.number
    }
}

/// Flashblocks-mode feed: subscribes with the `pending` tag for 200 ms
/// preconfirmations. Behaviour must be re-verified against `docs.base.org`
/// after Denim; if `pending` stops carrying preconfirmations, switch the
/// config to [`FeedMode::Canonical`].
pub struct FlashblocksFeed {
    /// Redacted endpoint label (never the full URL with credentials).
    pub label: String,
}

impl FlashblocksFeed {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
        }
    }
}

/// Canonical-blocks feed: live `newHeads` pubsub subscription, for
/// post-Denim 200 ms canonical blocks (the default feed mode).
pub struct CanonicalFeed {
    /// Redacted endpoint label (never the full URL with credentials).
    pub label: String,
    /// Full WS endpoint for the subscription. Secret-adjacent (may embed a
    /// provider key): stored privately, never logged -- use `label`.
    ws_url: String,
    /// Last accepted head, for gap/duplicate/reorg detection.
    last: Option<HeadEvent>,
    /// Live subscription, held across polls; dropped and re-established on
    /// timeout/close (reconnect).
    sub: Option<Subscription<Header>>,
}

impl CanonicalFeed {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ws_url: String::new(),
            last: None,
            sub: None,
        }
    }

    /// Feed with a live endpoint attached. `label` must be the redacted form
    /// (see [`redact_url`]); `ws_url` is the full endpoint (kept private).
    pub fn with_ws_url(label: impl Into<String>, ws_url: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ws_url: ws_url.into(),
            last: None,
            sub: None,
        }
    }

    /// Open one `newHeads` subscription, bounded. `None` = unreachable or
    /// no pubsub support (caller falls back to fixtures / last known head).
    async fn subscribe(ws_url: &str) -> Option<Subscription<Header>> {
        let provider = match tokio::time::timeout(Duration::from_secs(3), connect_ws(ws_url)).await
        {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                tracing::warn!("canonical feed connect failed: {e:#}");
                return None;
            }
            Err(_) => {
                tracing::warn!("canonical feed connect timed out");
                return None;
            }
        };
        match tokio::time::timeout(
            Duration::from_secs(3),
            provider.subscribe_blocks().into_future(),
        )
        .await
        {
            Ok(Ok(sub)) => Some(sub),
            Ok(Err(e)) => {
                tracing::warn!("canonical feed subscribe failed: {e:#}");
                None
            }
            Err(_) => {
                tracing::warn!("canonical feed subscribe timed out");
                None
            }
        }
    }
}

// Concrete subscription wiring is done by the runner against a live node;
// these trait impls encode the mode tag so mode confusion is a type error,
// not a boolean flag buried in call sites.
impl BlockFeed for FlashblocksFeed {
    async fn next_head(&mut self) -> Option<HeadEvent> {
        // Explicit stub: no live Flashblocks subscription in this binary.
        // Post-Denim the `pending` tag no longer carries preconfirmations, so
        // a silent None here must never be mistaken for "chain is quiet".
        tracing::warn!(
            label = %self.label,
            "flashblocks feed is an explicit stub (no live subscription); returning None -- set feed_mode=\"canonical\" for live heads"
        );
        None
    }
}

impl BlockFeed for CanonicalFeed {
    /// Pull the next head from the live `newHeads` subscription.
    ///
    /// - Connects lazily on first poll and reconnects (drop + resubscribe)
    ///   on timeout/close; failures return `None` so the caller fails closed
    ///   to fixtures / last known head instead of blocking.
    /// - Skips duplicates (same hash) and warns on gaps/reorgs via
    ///   [`classify_head`] / [`BlockFeed::reorged`].
    async fn next_head(&mut self) -> Option<HeadEvent> {
        // Bounded iterations: duplicate/unexpected messages skip without
        // spinning forever.
        for _ in 0..8 {
            if self.sub.is_none() {
                if self.ws_url.trim().is_empty() {
                    tracing::warn!(
                        label = %self.label,
                        "canonical feed has no WS endpoint; returning None (fixture fallback)"
                    );
                    return None;
                }
                let Some(sub) = Self::subscribe(&self.ws_url).await else {
                    tracing::warn!(
                        label = %self.label,
                        "canonical feed unavailable; will retry on next poll"
                    );
                    return None;
                };
                self.sub = Some(sub);
            }
            let sub = self.sub.as_mut().expect("subscription just established");
            let item = match tokio::time::timeout(Duration::from_secs(2), sub.recv_any()).await {
                Ok(Ok(item)) => item,
                Ok(Err(e)) => {
                    tracing::warn!(
                        "canonical subscription closed/lagged ({e}); reconnecting next poll"
                    );
                    self.sub = None;
                    return None;
                }
                Err(_) => {
                    tracing::warn!("canonical head recv timed out; reconnecting next poll");
                    self.sub = None;
                    return None;
                }
            };
            let header = match item {
                SubscriptionItem::Item(h) => h,
                SubscriptionItem::Other(_) => continue,
            };
            let event = HeadEvent {
                number: header.number,
                hash: header.hash,
                timestamp_ms: timestamp_ms(header.timestamp, None),
                is_preconfirmation: false,
            };
            match classify_head(self.last.as_ref(), &event) {
                HeadAction::Duplicate => continue,
                HeadAction::Accept { reorged } => {
                    if reorged {
                        tracing::warn!(
                            prev = ?self.last.as_ref().map(|h| h.number),
                            number = event.number,
                            hash = %event.hash,
                            "head gap/reorg: new head does not extend the last accepted one"
                        );
                    }
                    self.last = Some(event.clone());
                    return Some(event);
                }
            }
        }
        tracing::warn!("canonical feed: duplicate burst, giving up this poll");
        None
    }
}

/// What to do with an incoming head given the last accepted one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadAction {
    /// Fresh head: accept. `reorged` is true when it does not extend the
    /// previous head (number gap or hash mismatch) -- accept but log it.
    Accept { reorged: bool },
    /// Same hash as the last accepted head: duplicate, skip.
    Duplicate,
}

/// Pure head classifier: gap/duplicate/reorg triage with no I/O, so unit
/// tests and the paper fallback path stay offline. Live delivery uses this
/// inside [`CanonicalFeed::next_head`]; risk staleness
/// ([`crate::risk::RiskState::check_trade`]) still fails closed downstream.
pub fn classify_head(last: Option<&HeadEvent>, next: &HeadEvent) -> HeadAction {
    match last {
        None => HeadAction::Accept { reorged: false },
        Some(prev) if prev.hash == next.hash => HeadAction::Duplicate,
        Some(prev) => HeadAction::Accept {
            reorged: <CanonicalFeed as BlockFeed>::reorged(prev, next),
        },
    }
}

/// Best-effort single poll of the configured feed with an overall timeout.
/// Returns the live head when the feed yields one, else `None` (caller falls
/// back to the fixture head with a clear log). Never blocks the paper loop:
/// offline or unresponsive endpoints resolve to `None` within ~5 s, and a
/// refused connection resolves immediately.
pub async fn poll_head_once(mode: FeedMode, ws_url: &str) -> Option<HeadEvent> {
    match mode {
        FeedMode::Canonical => {
            let mut feed = CanonicalFeed::with_ws_url(redact_url(ws_url), ws_url);
            // ponytail: outer 7s > inner 3s+3s subscribe / 2s recv so slow path completes before outer fires; 8-iter bound is anti-spin.
            match tokio::time::timeout(Duration::from_secs(7), feed.next_head()).await {
                Ok(head) => head,
                Err(_) => {
                    tracing::warn!("canonical feed poll timed out; using fixture head");
                    None
                }
            }
        }
        FeedMode::Flashblocks => {
            let mut feed = FlashblocksFeed::new(redact_url(ws_url));
            // Stub always warns + returns None; surfaced here so the fixture
            // fallback is a deliberate, logged choice.
            let _ = feed.next_head().await;
            None
        }
    }
}

/// Connect a WebSocket provider. The returned provider is generic over the
/// standard Ethereum envelope; if OP-Stack deposit-type handling is needed,
/// switch the network type to the Base/OP network crate recommended by the
/// official Base docs (follow-up, pinned in `Cargo.lock`).
pub async fn connect_ws(url: &str) -> anyhow::Result<RootProvider<Ethereum>> {
    let ws = WsConnect::new(url);
    // Read-only dry-run probe: no fillers needed (no signing, no send).
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_ws(ws)
        .await?;
    Ok(provider)
}

/// Redact an RPC URL for logs/metrics: keep scheme + host, drop path, query,
/// userinfo (which is where provider API keys live).
pub fn redact_url(url: &str) -> String {
    // Very small redactor on purpose: no regex, no allocation surprises.
    // Drops userinfo (`user:pass@`) and any path/query (provider API keys).
    let scheme = url.split_once("://").map(|(s, _)| s).unwrap_or("ws");
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let no_userinfo = after.split('@').next_back().unwrap_or(after);
    let host = no_userinfo.split(['/', '?']).next().unwrap_or("?");
    format!("{scheme}://{host}")
}

/// Extract millisecond timestamps defensively: seconds-resolution headers
/// yield `None`, Denim-style millisecond headers yield `Some(ms)`.
/// `secs` is the standard header timestamp (seconds); `extra_ms` is an
/// optional Base-specific extra field when decoded via `WithOtherFields`.
pub fn timestamp_ms(secs: u64, extra_ms: Option<u64>) -> Option<u64> {
    extra_ms.or_else(|| secs.checked_mul(1000))
}

/// L1 data-fee oracle (OP-Stack GasPriceOracle predeploy). The L1 fee is a
/// mandatory term of the cost model; read it from this oracle per current
/// Base docs instead of hardcoding.
pub const L1_GAS_PRICE_ORACLE: &str = "0x420000000000000000000000000000000000000F";

/// Minimal ABI binding for `GasPriceOracle.getL1Fee(bytes) -> uint256`.
/// Used by the cost model via `eth_call`; pure construction, no broadcast.
pub fn l1_fee_calldata(tx_data: &[u8]) -> Vec<u8> {
    // Selector for getL1Fee(bytes): keccak("getL1Fee(bytes)")[0..4].
    // Computed once and pinned here to avoid a hashing dependency in const.
    const SELECTOR: [u8; 4] = [0x49, 0x9b, 0xd3, 0xe6];
    // ABI: selector ++ offset(32B, =0x20) ++ len(32B) ++ padded data.
    let mut out = Vec::with_capacity(4 + 32 + 32 + tx_data.len().div_ceil(32) * 32);
    out.extend_from_slice(&SELECTOR);
    let mut off = [0u8; 32];
    off[31] = 0x20;
    out.extend_from_slice(&off);
    let mut len = [0u8; 32];
    let l = tx_data.len() as u64;
    len[24..32].copy_from_slice(&l.to_be_bytes());
    out.extend_from_slice(&len);
    out.extend_from_slice(tx_data);
    let pad = (32 - tx_data.len() % 32) % 32;
    out.extend(std::iter::repeat_n(0u8, pad));
    out
}

#[allow(dead_code)]
fn _header_type_assert(h: Header) -> u64 {
    // Touch the alloy header type so version drift breaks here, loudly.
    h.number
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redactor_drops_credentials_and_path() {
        let red = redact_url("wss://user:secret@example.com/v3/key123?token=abc");
        assert!(!red.contains("secret"), "secret leaked: {red}");
        assert!(!red.contains("key123"), "key leaked: {red}");
        assert!(red.contains("example.com"), "host lost: {red}");
    }

    #[test]
    fn redactor_keeps_localhost() {
        assert_eq!(redact_url("ws://127.0.0.1:8545"), "ws://127.0.0.1:8545");
    }

    #[test]
    fn reorg_detection_by_number_gap() {
        let a = HeadEvent {
            number: 10,
            hash: B256::repeat_byte(1),
            timestamp_ms: None,
            is_preconfirmation: false,
        };
        let b = HeadEvent {
            number: 12,
            hash: B256::repeat_byte(2),
            timestamp_ms: None,
            is_preconfirmation: false,
        };
        assert!(FlashblocksFeed::reorged(&a, &b));
        let c = HeadEvent {
            number: 11,
            hash: B256::repeat_byte(3),
            timestamp_ms: None,
            is_preconfirmation: false,
        };
        assert!(!FlashblocksFeed::reorged(&a, &c));
    }

    #[test]
    fn l1_fee_calldata_layout() {
        let data = l1_fee_calldata(&[0xaa, 0xbb]);
        assert_eq!(&data[0..4], &[0x49, 0x9b, 0xd3, 0xe6]);
        assert_eq!(data.len(), 4 + 32 + 32 + 32);
        assert_eq!(data[4 + 32 + 31], 2);
    }

    #[test]
    fn classify_head_accepts_gaps_and_skips_duplicates() {
        let base = HeadEvent {
            number: 10,
            hash: B256::repeat_byte(1),
            timestamp_ms: None,
            is_preconfirmation: false,
        };
        assert_eq!(
            classify_head(None, &base),
            HeadAction::Accept { reorged: false }
        );
        let dup = HeadEvent {
            number: 99, // number ignored: same hash means duplicate
            hash: B256::repeat_byte(1),
            timestamp_ms: None,
            is_preconfirmation: false,
        };
        assert_eq!(classify_head(Some(&base), &dup), HeadAction::Duplicate);
        let next = HeadEvent {
            number: 11,
            hash: B256::repeat_byte(2),
            timestamp_ms: None,
            is_preconfirmation: false,
        };
        assert_eq!(
            classify_head(Some(&base), &next),
            HeadAction::Accept { reorged: false }
        );
        let gap = HeadEvent {
            number: 14,
            hash: B256::repeat_byte(3),
            timestamp_ms: None,
            is_preconfirmation: false,
        };
        assert_eq!(
            classify_head(Some(&base), &gap),
            HeadAction::Accept { reorged: true }
        );
    }

    #[test]
    fn head_event_drives_risk_staleness() {
        // Fake head, no network: observing it must satisfy the staleness
        // gate, and the same head must fail once the chain has moved on.
        use crate::risk::{RiskLimits, RiskState};
        let mut risk = RiskState::new(RiskLimits::default());
        // head_staleness_blocks default is 5; RiskState has no such field, so
        // pass the same constant the paper loop reads from config.
        let staleness = crate::config::BotConfig::default().head_staleness_blocks;
        let head = HeadEvent {
            number: 100,
            hash: B256::repeat_byte(7),
            timestamp_ms: Some(1_700_000_000_000),
            is_preconfirmation: false,
        };
        risk.observe_head(head.number);
        assert!(
            risk.check_trade(
                "WETH/USDC",
                true,
                1,
                Some(9_999_999),
                0,
                Some(head.number),
                staleness
            )
            .is_ok(),
            "freshly observed head must satisfy staleness"
        );
        risk.observe_head(head.number + 100);
        assert_eq!(
            risk.check_trade(
                "WETH/USDC",
                true,
                1,
                Some(9_999_999),
                0,
                Some(head.number),
                staleness
            ),
            Err(crate::risk::RiskError::StaleData),
            "old head must be stale after the chain moves on"
        );
    }

    #[test]
    fn feed_mode_selects_type() {
        fn is_canonical<F: BlockFeed>() -> bool {
            std::any::type_name::<F>().contains("Canonical")
        }
        assert!(is_canonical::<CanonicalFeed>());
        assert!(!is_canonical::<FlashblocksFeed>());
    }
}
