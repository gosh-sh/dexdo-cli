//! Seller-driven tick-advance orchestrator.
//! After the buyer is served the probe, the deal sits in `Probe` until the seller drives `accept_probe`
//! (after the probe window) and then `advance_tick` on the per-deal settle cadence. Without this loop the
//! deal never reaches `Streaming`, so a STOP burns instead of settling by-fact --
//! that is the gap reported in. `advance()` is **seller-only** on-chain
//! (`TokenContract.advance` is `onlyOwnerPubkey(_sellerPubkey)`), so the seller process owns this loop;
//! the buyer's role is to stay silent(no dispute) through the window.
//! The windows are **per-deal**: production mirrors them from
//! `TokenContract.getConfig()` -- `PROBE_WINDOW`(fixed 180s) for probe acceptance and `_settleWindow`
//! (scaled by price) for the stream cadence; tests inject short windows. This module is the
//! offline-verifiable core of the orchestrator; wiring it into the live serve path and the session-scoped

use dexdo_core::{ChainBackend, ChainError, Note, TokenContract};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Per-deal advance windows, mirrored from `TokenContract.getConfig()`.
#[derive(Debug, Clone, Copy)]
pub struct AdvanceWindows {
    /// Probe-acceptance window(`PROBE_WINDOW`, fixed 180s on-chain): wait for buyer silence, then accept.
    pub probe: Duration,
    /// Stream-phase cadence(`_settleWindow`, dynamic by price): the gap between `advance_tick` claims.
    pub settle: Duration,
}

/// The probe-acceptance window is **fixed** on-chain: `advance()` uses
/// `advanceWindow = _probeAccepted ? _settleWindow: PROBE_WINDOW`, and `PROBE_WINDOW` is a constant
/// 180s -- it is NOT part of `getConfig()`(which returns only the dynamic `settleWindow` / `streamTimeout`).
/// So the driver pairs this fixed probe window with the per-deal `settleWindow` read from the deal.
pub const PROBE_WINDOW: Duration = Duration::from_secs(180);

impl AdvanceWindows {
    /// Build the per-deal windows from a deal's `getConfig().settleWindow`(the only dynamic input): the
    /// probe phase always waits the fixed [`PROBE_WINDOW`], the stream phase uses the deal's
    /// `settleWindow`. This is the seller driver's per-deal window construction (issue, increment 2:
    /// "read `getConfig()` per deal").
    pub fn from_settle_window(settle: Duration) -> Self {
        Self {
            probe: PROBE_WINDOW,
            settle,
        }
    }

    /// Build the windows from a deal's `getConfig()` getter result (the JSON object returned by
    /// `RealChainBackend::token_contract_config`): `settleWindow` is a uint64 decimal string. Pairs the
    /// fixed [`PROBE_WINDOW`] with that per-deal value. Returns `None` if `settleWindow` is
    /// absent/unparseable(the caller then falls back or fails -- it must not silently use a wrong cadence).
    pub fn from_config_value(config: &serde_json::Value) -> Option<Self> {
        let settle = config["settleWindow"].as_str()?.parse::<u64>().ok()?;
        Some(Self::from_settle_window(Duration::from_secs(settle)))
    }
}

/// Drive one deal from `Probe` to by-fact `Streaming`: wait the probe window, `accept_probe` (the first
/// delivered tick is finalized to the seller and the two-tick invariant kicks in), then `advance_tick` on
/// the settle cadence until `tick_budget` is reached or the deal stops/disputes/exhausts (any
/// `advance_tick` error ends the loop -- the deal was closed or hit its `max_ticks` ceiling). Returns the
/// number of ticks finalized(>=1 once the probe is accepted).
/// Seller-only: `seller_note` authorizes the on-chain `advance()` (`onlyOwnerPubkey(_sellerPubkey)`).
#[allow(clippy::too_many_arguments)]
pub async fn drive_advance(
    chain: &dyn ChainBackend,
    token_contract: &TokenContract,
    seller_note: &dyn Note,
    windows: AdvanceWindows,
    tick_budget: u128,
    tick_size: u64,
    delivered: Arc<AtomicU64>,
    delivery_done: Arc<AtomicBool>,
) -> Result<u128, ChainError> {
    // +: finalized ticks must NEVER exceed delivered canonical ticks. The delivery counter is
    // in tokens; billing rounds any non-empty partial tick up to one tick, then adds one tick per full
    // TICK_SIZE boundary. The first token may arrive AFTER the probe window,
    // so wait/poll for it rather than bailing -- finalize NOTHING only on a TRUE zero-delivery terminal (the
    // session is done, or the deal closed, before the first token). Acquire ordering: the producer publishes
    // `delivered` then `delivery_done` with Release, so a `done` observed here implies the matching `delivered`
    // count is visible(no stale under-read).
    assert!(tick_size > 0, "tick_size must be non-zero");
    tokio::time::sleep(windows.probe).await;
    loop {
        if billed_ticks(delivered.load(Ordering::Acquire), tick_size) >= 1 {
            break; // at least one token delivered -- accept the probe below
        }
        if delivery_done.load(Ordering::Acquire) || deal_closed(chain, token_contract).await {
            // Done/closed observed -- re-read `delivered`(Acquire): the producer publishes `delivered`
            // BEFORE `delivery_done`(Release), so a token delivered just before `done` is now visible and
            // its probe must still be finalized(no premature 0 from the load1/load2 race).
            if billed_ticks(delivered.load(Ordering::Acquire), tick_size) >= 1 {
                break;
            }
            return Ok(0); // truly zero delivery: no probe, nothing finalized
        }
        tokio::time::sleep(windows.settle).await; // poll for the first delivered token
    }
    chain.accept_probe(token_contract).await?;
    let mut finalized: u128 = 1; // the first delivered partial/full tick is finalized on acceptance

    // each subsequent tick is finalized after the(dynamic) settle window, but ONLY up to the
    // number of canonical ticks actually delivered -- a timer firing does not entitle the seller to a tick
    // the buyer never received. The loop ends on a clean external close(buyer STOP / self-destruct), the
    // `max_ticks`/deposit ceiling(`ChainError::Limit`), or once delivery is complete and finalized has
    // caught up. Any other `advance_tick` failure is a genuine fault and MUST propagate(never claim success).
    loop {
        if finalized >= tick_budget {
            break;
        }
        if deal_closed(chain, token_contract).await {
            break; // closed externally(e.g. buyer STOP) -- nothing more to advance
        }
        let target = billed_ticks(delivered.load(Ordering::Acquire), tick_size).min(tick_budget);
        if finalized >= target {
            if delivery_done.load(Ordering::Acquire) {
                // Re-read `delivered` after observing `done` (producer publishes `delivered` BEFORE `done`,
                // Release): tokens delivered just before `done` must still be finalized -- break only once
                // finalized has caught up to the FINAL delivered count, otherwise fall through and advance
                // toward the refreshed target (no `load(target)`/`load(done)`-race under-finalize).
                let refreshed =
                    billed_ticks(delivered.load(Ordering::Acquire), tick_size).min(tick_budget);
                if finalized >= refreshed {
                    break;
                }
            } else {
                // nothing newly delivered this window -- wait for more delivery.
                tokio::time::sleep(windows.settle).await;
                continue;
            }
        }
        tokio::time::sleep(windows.settle).await;
        match chain.advance_tick(token_contract, seller_note).await {
            Ok(()) => finalized += 1,
            Err(ChainError::Limit(_)) => break, // max_ticks / deposit ceiling -- expected exhaustion
            Err(e) => {
                // A close can race the advance(buyer STOP between the snapshot and the call); re-check
                // before surfacing. Otherwise propagate the real error -- do NOT claim success.
                if deal_closed(chain, token_contract).await {
                    break;
                }
                return Err(e);
            }
        }
    }
    Ok(finalized)
}

fn billed_ticks(tokens: u64, tick_size: u64) -> u128 {
    if tokens == 0 {
        0
    } else {
        tokens.div_ceil(tick_size) as u128
    }
}

/// Whether the deal's stream is closed(settled / self-destructed) -- an expected, non-error terminal
/// condition for the advance loop. A **missing** snapshot is treated as *not* a clean close(`false`), so
/// an `advance_tick` failure with no observable close still propagates as a real error.
async fn deal_closed(chain: &dyn ChainBackend, token_contract: &TokenContract) -> bool {
    chain
        .snapshot(token_contract)
        .await
        .map(|s| s.closed)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dexdo_core::{LocalNote, Match, OfferListing, SellOffer, Settlement, StreamSnapshot};

    /// `from_settle_window` pairs the **fixed** probe window(180s) with the deal's **dynamic** settle
    /// window (from `getConfig().settleWindow`) -- the seller driver's per-deal window construction.
    #[test]
    fn advance_windows_from_settle_window_fixes_probe_keeps_settle() {
        let w = AdvanceWindows::from_settle_window(Duration::from_secs(600));
        assert_eq!(
            w.probe, PROBE_WINDOW,
            "probe window is the fixed constant"
        );
        assert_eq!(w.probe, Duration::from_secs(180));
        assert_eq!(
            w.settle,
            Duration::from_secs(600),
            "settle window is the per-deal dynamic value"
        );
    }

    /// `from_config_value` reads `settleWindow` out of the live `getConfig()` JSON(uint64 as a string),
    /// and returns `None` when the field is absent(no silent wrong-cadence fallback).
    #[test]
    fn advance_windows_from_get_config_value() {
        let cfg = serde_json::json!({
            "platformFeeBps": 250,
            "settleWindow": "600",
            "streamTimeout": "3600",
            "disputeWindow": "180"
        });
        let w = AdvanceWindows::from_config_value(&cfg).expect("parse getConfig");
        assert_eq!(w.probe, PROBE_WINDOW);
        assert_eq!(w.settle, Duration::from_secs(600));
        assert!(
            AdvanceWindows::from_config_value(&serde_json::json!({})).is_none(),
            "missing settleWindow -> None (caller must not use a wrong cadence)"
        );
    }

    /// A fake backend whose `advance_tick` fails with a **real** chain error(not a terminal condition),
    /// used to prove `drive_advance` propagates it instead of reporting a successful loop ( money-path
    /// safety, PR review). Probe acceptance succeeds; the stream reports no snapshot(not a close).
    struct ExplodingBackend;

    #[async_trait::async_trait]
    impl ChainBackend for ExplodingBackend {
        async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
            unimplemented!()
        }
        async fn post_offer(&self, _: SellOffer, _: &dyn Note) -> Result<(), ChainError> {
            unimplemented!()
        }
        async fn place_buy(&self, _: &TokenContract, _: &dyn Note) -> Result<(), ChainError> {
            unimplemented!()
        }
        async fn read_match(&self, _: &TokenContract) -> Result<Match, ChainError> {
            unimplemented!()
        }
        async fn open_stream(
            &self,
            _: &TokenContract,
            _: Vec<u8>,
            _: &dyn Note,
        ) -> Result<(), ChainError> {
            unimplemented!()
        }
        async fn read_handover(&self, _: &TokenContract) -> Result<Option<Vec<u8>>, ChainError> {
            unimplemented!()
        }
        async fn advance_tick(&self, _: &TokenContract, _: &dyn Note) -> Result<(), ChainError> {
            Err(ChainError::Chain("boom".to_string()))
        }
        async fn accept_probe(&self, _: &TokenContract) -> Result<(), ChainError> {
            Ok(())
        }
        async fn stop(&self, _: &TokenContract, _: &dyn Note) -> Result<Settlement, ChainError> {
            unimplemented!()
        }
        async fn seller_timeout(&self, _: &TokenContract) -> Result<Settlement, ChainError> {
            unimplemented!()
        }
        async fn snapshot(&self, _: &TokenContract) -> Option<StreamSnapshot> {
            None
        }
    }

    /// A real `advance_tick` failure (e.g. `Chain("boom")`) must propagate -- NOT be swallowed as a clean
    /// terminal condition. Otherwise the seller path would claim an advance succeeded with nothing
    /// finalized on-chain.
    #[tokio::test]
    async fn drive_advance_propagates_real_advance_error() {
        let backend = ExplodingBackend;
        let note = LocalNote::generate();
        let windows = AdvanceWindows {
            probe: Duration::ZERO,
            settle: Duration::ZERO,
        };
        let res = drive_advance(
            &backend,
            &"tc-boom".to_string(),
            &note,
            windows,
            4,
            1,
            Arc::new(AtomicU64::new(4)),
            Arc::new(AtomicBool::new(false)),
        )
        .await;
        match res {
            Err(ChainError::Chain(msg)) => assert_eq!(msg, "boom"),
            other => panic!("expected propagated Chain(\"boom\"), got {other:?}"),
        }
    }

    /// A backend whose `advance_tick` always succeeds and counts the calls -- used to prove `drive_advance`
    /// never finalizes more ticks than were delivered.
    struct CountingBackend {
        advances: AtomicU64,
    }
    impl CountingBackend {
        fn new() -> Self {
            Self {
                advances: AtomicU64::new(0),
            }
        }
    }
    #[async_trait::async_trait]
    impl ChainBackend for CountingBackend {
        async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
            unimplemented!()
        }
        async fn post_offer(&self, _: SellOffer, _: &dyn Note) -> Result<(), ChainError> {
            unimplemented!()
        }
        async fn place_buy(&self, _: &TokenContract, _: &dyn Note) -> Result<(), ChainError> {
            unimplemented!()
        }
        async fn read_match(&self, _: &TokenContract) -> Result<Match, ChainError> {
            unimplemented!()
        }
        async fn open_stream(
            &self,
            _: &TokenContract,
            _: Vec<u8>,
            _: &dyn Note,
        ) -> Result<(), ChainError> {
            unimplemented!()
        }
        async fn read_handover(&self, _: &TokenContract) -> Result<Option<Vec<u8>>, ChainError> {
            unimplemented!()
        }
        async fn advance_tick(&self, _: &TokenContract, _: &dyn Note) -> Result<(), ChainError> {
            self.advances.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        async fn accept_probe(&self, _: &TokenContract) -> Result<(), ChainError> {
            Ok(())
        }
        async fn stop(&self, _: &TokenContract, _: &dyn Note) -> Result<Settlement, ChainError> {
            unimplemented!()
        }
        async fn seller_timeout(&self, _: &TokenContract) -> Result<Settlement, ChainError> {
            unimplemented!()
        }
        async fn snapshot(&self, _: &TokenContract) -> Option<StreamSnapshot> {
            None // never a clean close -- the bound relies on `delivered`/`delivery_done`, not a stop
        }
    }

    #[test]
    fn delivered_tokens_round_up_to_canonical_ticks() {
        assert_eq!(billed_ticks(0, 4), 0);
        assert_eq!(billed_ticks(1, 4), 1);
        assert_eq!(billed_ticks(4, 4), 1);
        assert_eq!(billed_ticks(5, 4), 2);
        assert_eq!(billed_ticks(8, 4), 2);
    }

    /// money-path safety: `drive_advance` finalizes **at most** the delivered tick count, never the
    /// timer budget -- including the no-request / short-session cases.
    #[tokio::test]
    async fn drive_advance_never_finalizes_beyond_delivered_ticks() {
        let note = LocalNote::generate();
        let tc = "tc".to_string();
        let w = AdvanceWindows {
            probe: Duration::ZERO,
            settle: Duration::ZERO,
        };

        // Zero delivered(no-request / errored before the first token) -> finalize NOTHING(no probe tick).
        let n = drive_advance(
            &CountingBackend::new(),
            &tc,
            &note,
            w,
            10,
            4,
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(true)),
        )
        .await
        .unwrap();
        assert_eq!(
            n, 0,
            "zero delivered -> zero finalized (probe not accepted)"
        );

        // Delivered 5 tokens at tick_size=4: finalized is capped at 2 billed ticks, not 5 per-token ticks.
        let b = CountingBackend::new();
        let n = drive_advance(
            &b,
            &tc,
            &note,
            w,
            10,
            4,
            Arc::new(AtomicU64::new(5)),
            Arc::new(AtomicBool::new(true)),
        )
        .await
        .unwrap();
        assert_eq!(
            n, 2,
            "5 tokens at tick_size=4 -> 2 billed ticks, not 5 per-token ticks"
        );
        assert_eq!(
            b.advances.load(Ordering::Relaxed),
            1,
            "probe(1) + 1 advance_tick = 2 finalized"
        );

        // Budget below delivered: finalized capped at the budget(still <= delivered).
        let n = drive_advance(
            &CountingBackend::new(),
            &tc,
            &note,
            w,
            3,
            4,
            Arc::new(AtomicU64::new(20)),
            Arc::new(AtomicBool::new(true)),
        )
        .await
        .unwrap();
        assert_eq!(n, 3, "finalized <= budget (3) <= delivered ticks (5)");
    }

    /// the first token can arrive AFTER the probe window -- the driver must WAIT for it
    /// (not return 0 prematurely) and finalize the probe once `delivered >= 1`. Only a true zero-delivery
    /// terminal(`delivery_done`/closed before the first token) returns 0.
    #[tokio::test]
    async fn drive_advance_waits_for_first_token_then_finalizes_probe() {
        let note = LocalNote::generate();
        let tc = "tc".to_string();
        let w = AdvanceWindows {
            probe: Duration::ZERO,
            settle: Duration::from_millis(2),
        };
        let delivered = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicBool::new(false));
        // Producer: deliver the first token AFTER the probe window, then mark the session done. `delivered`
        // is published(Release) BEFORE `done`, matching the driver's Acquire re-read -- so even on the
        // load1/load2 race the driver sees the token and does not return a premature 0.
        let (d2, done2) = (delivered.clone(), done.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            d2.store(1, Ordering::Release);
            done2.store(true, Ordering::Release);
        });
        let n = drive_advance(
            &CountingBackend::new(),
            &tc,
            &note,
            w,
            10,
            4,
            delivered,
            done,
        )
        .await
        .unwrap();
        assert_eq!(
            n, 1,
            "waited for the first delivered token, then finalized the probe (not a premature 0)"
        );
    }

    /// regression: after `open_stream`, a matched buyer may not have connected yet. Empty delivery is not a
    /// seller failure and must not drive by-fact advancement or end the seller while the session is still open.
    #[tokio::test]
    async fn drive_advance_keeps_waiting_when_buyer_not_connected_yet() {
        let note = LocalNote::generate();
        let tc = "tc".to_string();
        let backend = CountingBackend::new();
        let res = tokio::time::timeout(
            Duration::from_millis(25),
            drive_advance(
                &backend,
                &tc,
                &note,
                AdvanceWindows {
                    probe: Duration::from_millis(1),
                    settle: Duration::from_millis(5),
                },
                10,
                4,
                Arc::new(AtomicU64::new(0)),
                Arc::new(AtomicBool::new(false)),
            ),
        )
        .await;

        assert!(
            res.is_err(),
            "no buyer connection / zero delivery must keep the seller waiting, not return"
        );
        assert_eq!(
            backend.advances.load(Ordering::Relaxed),
            0,
            "advance_tick must not fire before any delivered token"
        );
    }

    /// tokens delivered just before `done` must be finalized -- the driver re-reads
    /// `delivered` after observing `done` and advances to the FINAL count, never under-finalizing.
    #[tokio::test]
    async fn drive_advance_finalizes_ticks_delivered_up_to_done() {
        let note = LocalNote::generate();
        let tc = "tc".to_string();
        let w = AdvanceWindows {
            probe: Duration::ZERO,
            settle: Duration::from_millis(2),
        };
        let delivered = Arc::new(AtomicU64::new(1)); // the first partial tick
        let done = Arc::new(AtomicBool::new(false));
        // Producer: deliver enough tokens for two more billed ticks, then mark done (Release -- delivered
        // published before done).
        let (d, dn) = (delivered.clone(), done.clone());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(15)).await;
            d.store(9, Ordering::Release);
            dn.store(true, Ordering::Release);
        });
        let n = drive_advance(
            &CountingBackend::new(),
            &tc,
            &note,
            w,
            100,
            4,
            delivered,
            done,
        )
        .await
        .unwrap();
        assert_eq!(
            n, 3,
            "all delivered ticks (incl. those delivered just before done) are finalized -- no under-finalize"
        );
    }
}
