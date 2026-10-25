//! - the stale-ask race, and the three defects the first two attempts shipped.

//! Caught live on mainnet 2026-08-17 at cut `c95e2fea`: the seller retired a resting deal locally
//! 200 ms before a buyer matched the ask that was still executable in the book.

//! The chain needed nothing. `InferenceOrderBook.cancelOrder` is note-authorized and serialized
//! through the same queue as matching, and the matcher independently drops an expired maker inline
//! (`_isExpired(mk.deadline)`). What was broken is a contradiction inside the client, and the two
//! attempts to fix it added two more -- both of which this stand could not see, because it could not
//! be resting and matched at once and it never relisted. That blindness is the fourth defect and it
//! is why these tests are built the way they are: the stand carries MUTABLE state, and the
//! assertions are on the production units the pool actually calls.

use super::{
    apply_drained_outcome, decide_after_stop, plan_unproven_start, reseat_resting_after_stop,
    should_rearm_watcher, startup_stop_keeps_the_deal, sweep_unconfirmed_resting_offers, AfterStop,
    RestingEntry, SellerPoolDeal, UnprovenStartAction,
};
use dexdo::seller::liveness::{CancellationDisposition, RestingOfferIdentity, RestingStopReason};
use dexdo_core::{
    ChainBackend, ChainError, DealChainSnapshot, DealChainState, Match, MatchWatchCursor,
    MatchedFill, Note, OfferListing, OrderBookOrder, SellOffer, Settlement, StreamSnapshot,
    TokenContract,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const UNPROVEN_TC: &str = "0:02032c63003762ed0000000000000000000000000000000000000000000000ff";
const PROVEN_TC: &str = "0:9b81f701f6e94a12d2772607f3874d1ccde9459c46f7688cbceb244a4fe098bd";
const OWNER_NOTE: &str = "0:977936df3527a524516a796c619bfe4a40238a7bef76378d4f1aaba8016db438";

/// What the stand observed, shared with the test that set it up.

/// The first stand had none of this: `matched` was a construction-time `bool`, so an ask could be
/// resting OR crossed and never one then the other, and the successor identity was handed straight
/// to the function under test instead of arising from a relist. Both shipped defects lived in
/// exactly that gap.
#[derive(Default)]
struct BookState {
    pending: AtomicBool,
    pending_read_fails: AtomicBool,
    cancel_succeeds: AtomicBool,
    cancelled: AtomicBool,
    reads_fail: AtomicBool,
    reads: AtomicUsize,
    posts: AtomicUsize,
    /// Flipped mid-test: the ask was resting, and now a buyer has crossed it.
    matched: AtomicBool,
    /// The generation the BOOK holds. A relist advances it; the pool's captured copy does not.
    book_order_id: Mutex<u128>,
    /// Which generation each cancel was aimed at.
    cancel_targets: Mutex<Vec<u128>>,
    /// How many times serving reached its coherent deal snapshot, after authoritative fill recovery.
    /// This counts attempts to discharge a match rather than mentions of one.
    serve_attempts: AtomicUsize,
}

impl BookState {
    fn resting(order_id: u128) -> Arc<Self> {
        Arc::new(Self {
            matched: AtomicBool::new(false),
            book_order_id: Mutex::new(order_id),
            ..Default::default()
        })
    }

    /// A buyer crossed the ask. The row leaves the book and a match becomes readable -- the pair
    /// `cancel_and_confirm` reads as `AlreadyMatched`.
    fn cross(&self) {
        self.matched.store(true, Ordering::SeqCst);
    }

    /// The supervisor reaped the expired ask and posted a successor: the book now holds a new
    /// generation, and anything still naming the old one is naming something consumed.
    fn relist_to(&self, successor: u128) {
        *self.book_order_id.lock().expect("book order id") = successor;
    }

    fn cancels(&self) -> Vec<u128> {
        self.cancel_targets.lock().expect("cancel log").clone()
    }

    fn serves(&self) -> usize {
        self.serve_attempts.load(Ordering::SeqCst)
    }
}

struct StaleAskBackend {
    token_contract: String,
    book: Arc<BookState>,
    _cursor_dir: tempfile::TempDir,
}

#[async_trait::async_trait]
impl ChainBackend for StaleAskBackend {
    async fn has_pending_sell_offer(&self, _: &TokenContract) -> Result<bool, ChainError> {
        if self.book.pending_read_fails.load(Ordering::SeqCst) {
            return Err(ChainError::Chain("fixture unreadable SELL journal".into()));
        }
        Ok(self.book.pending.load(Ordering::SeqCst))
    }
    /// The book's own rows. Empty once crossed -- the row is gone -- and otherwise the CURRENT
    /// generation, which a relist has moved.
    async fn raw_resting_sell_orders_for_tc(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Vec<OrderBookOrder>, ChainError> {
        self.book.reads.fetch_add(1, Ordering::SeqCst);
        if self.book.reads_fail.load(Ordering::SeqCst) {
            return Err(ChainError::Transport("fixture getter outage".into()));
        }
        if token_contract != &self.token_contract
            || self.book.matched.load(Ordering::SeqCst)
            || self.book.cancelled.load(Ordering::SeqCst)
        {
            return Ok(Vec::new());
        }
        Ok(vec![OrderBookOrder {
            order_id: *self.book.book_order_id.lock().expect("book order id"),
            owner_note: OWNER_NOTE.to_string(),
            token_contract: Some(self.token_contract.clone()),
            is_buy: false,
            price_per_tick: 1_000_000_000,
            ticks: 2,
            escrow: 0,
            deadline: u64::MAX,
            flags: 0,
            timestamp: 0,
        }])
    }

    /// Refused the way a full cancel queue refuses (`CANCEL_REJ_QUEUE_FULL`), and recorded so the
    /// test can say WHICH generation was aimed at. Tests can also accept and remove the exact ask.
    async fn cancel_resting_sell_order(
        &self,
        _token_contract: &TokenContract,
        order_id: u128,
    ) -> Result<(), ChainError> {
        self.book
            .cancel_targets
            .lock()
            .expect("cancel log")
            .push(order_id);
        if self.book.cancel_succeeds.load(Ordering::SeqCst) {
            self.book.cancelled.store(true, Ordering::SeqCst);
            return Ok(());
        }
        Err(ChainError::Chain(
            "order book cancel queue is full".to_string(),
        ))
    }

    /// What `target_state` reads once the row has left the book.
    async fn read_openable_match_now(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Match>, ChainError> {
        if !self.book.matched.load(Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(Some(crossed_match(token_contract)))
    }

    async fn deal_state(&self, _: &TokenContract) -> Result<Option<DealChainState>, ChainError> {
        self.book.reads.fetch_add(1, Ordering::SeqCst);
        if self.book.reads_fail.load(Ordering::SeqCst) {
            return Err(ChainError::Transport("fixture state outage".into()));
        }
        Ok(self
            .book
            .matched
            .load(Ordering::SeqCst)
            .then_some(DealChainState {
                funded: true,
                opened: false,
                probe_accepted: false,
                disputed: false,
                deposit: dexdo_core::required_escrow_for_buy(2, dexdo_core::PRICE_STEP),
                finalized_owed: 0,
                tokens_final: 0,
                tokens_pending: 0,
                probe_tick: 0,
                funded_time: Some(1),
                probe_time: 0,
                last_claim_time: 0,
                dispute_time: 0,
            }))
    }

    async fn poll_seller_fills(
        &self,
        _: &dyn Note,
        _: &mut MatchWatchCursor,
    ) -> Result<Vec<MatchedFill>, ChainError> {
        Ok(if self.book.matched.load(Ordering::SeqCst) {
            vec![MatchedFill {
                order_id: *self.book.book_order_id.lock().expect("book order id"),
                token_contract: self.token_contract.clone(),
                ticks: 2,
                price_per_tick: dexdo_core::PRICE_STEP,
            }]
        } else {
            Vec::new()
        })
    }

    async fn sell_offer_terms(&self, _: &TokenContract) -> Result<Option<(u64, u64)>, ChainError> {
        self.book.reads.fetch_add(1, Ordering::SeqCst);
        if self.book.reads_fail.load(Ordering::SeqCst) {
            return Err(ChainError::Transport("fixture terms outage".into()));
        }
        Ok(Some((
            u64::try_from(dexdo_core::PRICE_STEP).expect("canonical price fits the getter"),
            2,
        )))
    }

    /// The serve's coherent capacity read follows durable authoritative fill recovery.
    /// Counting it tells "the match was served" from "the match was mentioned".
    async fn deal_snapshot(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Option<DealChainSnapshot>, ChainError> {
        self.book.serve_attempts.fetch_add(1, Ordering::SeqCst);
        Err(ChainError::Chain(
            "this stand does not carry a full deal snapshot".to_string(),
        ))
    }

    async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
        Ok(Vec::new())
    }
    async fn post_offer(&self, _offer: SellOffer, _note: &dyn Note) -> Result<(), ChainError> {
        self.book.posts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn place_buy(
        &self,
        _token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<(), ChainError> {
        Ok(())
    }
    async fn read_match(&self, token_contract: &TokenContract) -> Result<Match, ChainError> {
        if !self.book.matched.load(Ordering::SeqCst) {
            return Err(ChainError::Chain("no match".to_string()));
        }
        Ok(crossed_match(token_contract))
    }
    async fn snapshot(&self, _token_contract: &TokenContract) -> Option<StreamSnapshot> {
        None
    }
    async fn open_stream(
        &self,
        _token_contract: &TokenContract,
        _endpoint_cipher: Vec<u8>,
        _note: &dyn Note,
    ) -> Result<(), ChainError> {
        Ok(())
    }
    async fn read_handover(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Option<Vec<u8>>, ChainError> {
        Ok(None)
    }
    async fn claim_tokens(
        &self,
        _token_contract: &TokenContract,
        _note: &dyn Note,
        _tokens: u128,
    ) -> Result<(), ChainError> {
        Ok(())
    }
    async fn stop(
        &self,
        _token_contract: &TokenContract,
        _note: &dyn Note,
    ) -> Result<Settlement, ChainError> {
        Err(ChainError::Chain("not stoppable in this stand".to_string()))
    }
}

fn crossed_match(token_contract: &TokenContract) -> Match {
    Match {
        token_contract: token_contract.clone(),
        // Cancellation and fill recovery must observe the same simulated buyer.
        buyer_pubkey: dexdo_core::LocalNote::from_seed(&[0x42; 32]).pubkey(),
        price_per_tick: 1_000_000_000,
    }
}

fn identity(token_contract: &str, order_id: u128) -> RestingOfferIdentity {
    RestingOfferIdentity {
        owner_note: OWNER_NOTE.to_string(),
        token_contract: token_contract.to_string(),
        order_id,
    }
}

/// A `RunningSeller` with no gateway behind it. These tests never speak to one -- they exercise the
/// pool's bookkeeping and the serve CALL -- so standing up TLS and tonic would add a second thing
/// that can fail for reasons unrelated to what is being proved.
fn test_seller() -> dexdo::seller::RunningSeller {
    dexdo::seller::RunningSeller {
        state: Arc::new(dexdo::seller::gateway::GatewayState::new()),
        note: Arc::new(dexdo_core::LocalNote::generate()),
        server_task: tokio::spawn(async {}),
        listen_addr: "127.0.0.1:0".parse().expect("a placeholder listen address"),
        tls_fingerprint: "00".repeat(32),
    }
}

fn deal_with(token_contract: &str, book: Arc<BookState>) -> SellerPoolDeal {
    let cursor_dir = tempfile::tempdir().expect("private stale-ask fixture cursor directory");
    let cursor_path = cursor_dir.path().join("seller.cursor.json");
    let backend = StaleAskBackend {
        token_contract: token_contract.to_string(),
        book,
        _cursor_dir: cursor_dir,
    };
    SellerPoolDeal {
        chain: Arc::new(backend) as Arc<dyn ChainBackend>,
        cfg: dexdo::seller::SellerConfig {
            token_contract: token_contract.to_string(),
            price_per_tick: 1_000_000_000,
            max_ticks: 2,
            subscription: false,
            gateway_advertise: "127.0.0.1:0".to_string(),
            mock_token_count: 0,
        },
        watch: dexdo::seller::SellerMatchWatchConfig {
            cursor_path,
            poll_interval: std::time::Duration::from_millis(1),
        },
        upstream: dexdo::seller::UpstreamConfig::Mock,
        nonce: 1416,
        market: None,
    }
}

fn entry(token_contract: &str, book: Arc<BookState>, order_id: u128) -> RestingEntry {
    (
        deal_with(token_contract, book),
        identity(token_contract, order_id),
    )
}

// -------------------------------------------------------------------------
// REVIEW FINDING 1 -- a match found with no watcher left must be SERVED.
// -------------------------------------------------------------------------

/// The ask was resting; a buyer crossed it while the cancel was in flight; every watcher is gone.
/// The sweep is the last thing that can answer him.

/// Red on the previous attempt: `AfterStop::ServeMatch` was a bare marker, so the sweep had a name
/// for the obligation and nothing to discharge it with, and settled for reporting that nobody
/// answered. `serve_attempts` was 0.
#[tokio::test(start_paused = true)]
async fn a_match_found_at_shutdown_is_served_not_merely_reported() {
    let seller = test_seller();
    let book = BookState::resting(29);
    let survivors = vec![entry(UNPROVEN_TC, Arc::clone(&book), 29)];
    let deal = survivors[0].0.clone();

    // The ask is crossed AFTER the entry was built -- the state the first stand could not express.
    book.cross();

    let error = sweep_unconfirmed_resting_offers(&seller, survivors).await;

    assert_eq!(
        book.serves(),
        1,
        "the sweep must ATTEMPT to serve the buyer, not report that nobody did"
    );
    let fill =
        dexdo::seller::read_seller_fill_lineage(&deal.watch.cursor_path, &deal.cfg.token_contract)
            .expect("read saved fill")
            .expect("the authoritative fill must precede the serve attempt");
    assert_eq!(
        (fill.order_id, fill.matched_ticks, fill.residual_ticks),
        (29, 2, 0)
    );
    let rendered = error
        .expect("this stand cannot complete a serve, so it must surface")
        .to_string();
    assert!(
        rendered.contains("could not serve"),
        "a failed serve must say so, got: {rendered}"
    );
}

/// The decision itself must separate the two questions it used to conflate.
#[test]
fn already_matched_is_unmatchable_for_the_book_and_still_owed_by_the_pool() {
    let matched = CancellationDisposition::AlreadyMatched(crossed_match(&UNPROVEN_TC.to_string()));
    assert!(
        matched.proven_unmatchable(),
        "the BOOK question stays yes -- that is why one answer for two questions was the defect"
    );
    assert_eq!(decide_after_stop(&matched).kind(), "serve_match");
    assert!(should_rearm_watcher(
        &decide_after_stop(&matched),
        &RestingStopReason::Shutdown
    ));
}

// -------------------------------------------------------------------------
// REVIEW FINDING 2 -- the identity must follow a real relist.
// -------------------------------------------------------------------------

/// The supervisor relisted onto a successor and then stopped without proving it gone. The pool
/// captured the predecessor before the watch began. Whatever the sweep cancels must be the
/// generation the BOOK holds.

/// Red on the previous attempt: the drained identity was discarded with the watcher, so the sweep
/// aimed at a consumed predecessor and refused, green, about somebody else's order.
#[tokio::test(start_paused = true)]
async fn a_drained_watcher_reseats_the_generation_the_book_actually_holds() {
    const PREDECESSOR: u128 = 29;
    const SUCCESSOR: u128 = 30;

    let seller = test_seller();
    let book = BookState::resting(PREDECESSOR);
    let deal = deal_with(UNPROVEN_TC, Arc::clone(&book));

    let mut resting = HashMap::<String, RestingEntry>::new();
    resting.insert(
        UNPROVEN_TC.to_string(),
        (deal.clone(), identity(UNPROVEN_TC, PREDECESSOR)),
    );

    // A real relist: the book moves to the successor, and the watcher reports the identity it ended
    // on -- which is exactly what the drain exists to collect.
    book.relist_to(SUCCESSOR);

    let mut first_error = None;
    apply_drained_outcome(
        &seller,
        deal,
        Some(identity(UNPROVEN_TC, SUCCESSOR)),
        Ok(dexdo::seller::liveness::RestingSellerOutcome::Stopped {
            reason: RestingStopReason::Watcher("upstream died".to_string()),
            disposition: CancellationDisposition::RejectedStillResting {
                known_result: "cancel_submit=rejected: queue full".to_string(),
            },
        }),
        &mut resting,
        &mut first_error,
    )
    .await;

    assert_eq!(
        resting[UNPROVEN_TC].1.order_id, SUCCESSOR,
        "the drained watcher's generation must win over the pool's captured copy"
    );

    let _ = sweep_unconfirmed_resting_offers(&seller, resting.into_values().collect()).await;
    let cancelled = book.cancels();
    assert!(
        cancelled.contains(&SUCCESSOR) && !cancelled.contains(&PREDECESSOR),
        "the sweep must cancel the successor {SUCCESSOR}, not the consumed {PREDECESSOR}: {cancelled:?}"
    );
}

// -------------------------------------------------------------------------
// REVIEW FINDING 3 -- an unproven START still obliges the pool.
// -------------------------------------------------------------------------

/// The startup path used to flatten every stop into an error, and `SellerStartupOutcome::Stopped`
/// carries an identity that the `..` pattern threw away. The pool then unregistered the stream with
/// nothing in `resting` and no watcher: an executable ask nobody watched and nobody swept.
#[test]
fn a_start_that_stopped_without_proving_its_ask_gone_keeps_the_deal() {
    for unproven in [
        CancellationDisposition::UnknownFailure {
            known_result: String::new(),
        },
        CancellationDisposition::RejectedStillResting {
            known_result: String::new(),
        },
    ] {
        assert!(
            startup_stop_keeps_the_deal(&unproven),
            "an unproven start must keep the deal: {unproven}"
        );
    }
    for proven in [
        CancellationDisposition::Cancelled,
        CancellationDisposition::AlreadyAbsent,
        CancellationDisposition::NotAttemptedExpired,
    ] {
        assert!(
            !startup_stop_keeps_the_deal(&proven),
            "a proven start owes nothing: {proven}"
        );
    }
}

// -------------------------------------------------------------------------
// The original defect, and the guard that must still let healthy deals go.
// -------------------------------------------------------------------------

/// An unproven cancellation survives to the sweep and the sweep raises. Red before the first fix:
/// the running path retired on every disposition, so the entry never reached the sweep.
#[tokio::test(start_paused = true)]
async fn an_unproven_cancellation_survives_to_the_sweep_and_the_sweep_raises() {
    let seller = test_seller();
    for round in 0..50u32 {
        let book = BookState::resting(29);
        let survivors = vec![entry(UNPROVEN_TC, Arc::clone(&book), 29)];
        let error = sweep_unconfirmed_resting_offers(&seller, survivors)
            .await
            .unwrap_or_else(|| {
                panic!(
                    "round {round}: the sweep raised nothing, so an ask never proven off the book \
                     left the pool silently -- this is "
                )
            });
        assert!(
            error.to_string().contains("could not confirm cancellation"),
            "round {round}: {error}"
        );
    }
}

/// A guard that never lets anything through is not a guard: a proven-gone ask must be retired, or a
/// healthy seller accumulates entries forever and every shutdown fails.
#[tokio::test]
async fn a_proven_cancellation_is_retired_and_the_sweep_stays_silent() {
    let seller = test_seller();
    assert_eq!(
        decide_after_stop(&CancellationDisposition::Cancelled).kind(),
        "retire"
    );
    assert!(sweep_unconfirmed_resting_offers(&seller, Vec::new())
        .await
        .is_none());
}

/// expiry is proven WITHOUT a write, because the matcher drops an expired maker inline on
/// every crossing. Covers only the proven arms, so it discriminates: a change to the unproven
/// fallback must redden the consequence test and not this one.
#[test]
fn an_expired_ask_counts_as_proven_because_the_matcher_refuses_it() {
    for proven in [
        CancellationDisposition::NotAttemptedExpired,
        CancellationDisposition::Cancelled,
        CancellationDisposition::AlreadyAbsent,
    ] {
        assert_eq!(decide_after_stop(&proven).kind(), "retire", "{proven}");
    }
}

/// The re-arm table: who watches next, and the one exception that lets a shutdown terminate.
#[test]
fn a_deal_that_is_still_owed_something_goes_back_under_a_watcher() {
    let health = RestingStopReason::Watcher("upstream died".to_string());
    assert!(!should_rearm_watcher(&AfterStop::Retire, &health));
    assert!(should_rearm_watcher(&AfterStop::Retain, &health));
    assert!(!should_rearm_watcher(
        &AfterStop::Retain,
        &RestingStopReason::Shutdown
    ));

    // Reseating is what keeps the sweep able to see it at all.
    let book = BookState::resting(29);
    let mut resting = HashMap::<String, RestingEntry>::new();
    reseat_resting_after_stop(
        UNPROVEN_TC,
        Some(identity(UNPROVEN_TC, 29)),
        Some(entry(UNPROVEN_TC, book, 29)),
        &mut resting,
    );
    assert_eq!(resting.len(), 1);
}

// -------------------------------------------------------------------------
// FOURTH REVIEW -- the half of finding 3 the previous commit claimed and did not deliver.
// -------------------------------------------------------------------------

/// POINT 1. The startup path must reach the same ANSWER as the running path on the same input.

/// The name says `plans`, and that is the whole of what is asserted: `plan_unproven_start` returns
/// `Keep { rearm: true }`. Whether the loop then performs that `watched.push` is NOT observed here
/// and is not observed anywhere -- see the NOT PROVEN note in the commit body. An earlier name
/// claimed the deal "goes back under a watcher", which a build that never arms one passes.

/// `RestingStopReason::Health` with an unproven disposition is the pair on which the two diverged:
/// the running path asks `should_rearm_watcher`, gets `true`, and puts the deal back under a
/// watcher; the startup path seated the deal and stopped. Bookkeeping and a gateway route with no
/// observer is precisely what `should_rearm_watcher`'s own doc comment rules out.

/// Red before this change: the branch never consulted the shared question at all.
#[test]
fn an_unproven_start_plans_the_same_rearm_the_running_path_plans() {
    let health = RestingStopReason::Health(dexdo::seller::liveness::HealthFailure::new(
        dexdo::seller::liveness::HealthComponent::GatewayTask,
        false,
        "gateway stopped",
    ));
    let unproven = CancellationDisposition::UnknownFailure {
        known_result: String::new(),
    };
    let id = identity(UNPROVEN_TC, 29);

    assert_eq!(
        plan_unproven_start(Some(&id), &health, &unproven),
        UnprovenStartAction::Keep { rearm: true },
        "the startup path must re-arm on the same pair the running path re-arms on"
    );
    // And it must agree with the running path by ASKING it, not by repeating its table.
    assert!(should_rearm_watcher(&decide_after_stop(&unproven), &health));

    // Draining must still terminate: a shutdown seats the deal for the sweep and does not re-arm.
    assert_eq!(
        plan_unproven_start(Some(&id), &RestingStopReason::Shutdown, &unproven),
        UnprovenStartAction::Keep { rearm: false }
    );
}

/// An unknown submission retains pending mode independently of how its interruption was named.
#[test]
fn issue_2115_an_unproven_start_with_no_identity_keeps_pending_ownership() {
    let unproven = CancellationDisposition::UnknownFailure {
        known_result: String::new(),
    };
    for reason in [
        RestingStopReason::Shutdown,
        RestingStopReason::Watcher("upstream died".to_string()),
    ] {
        assert_eq!(
            plan_unproven_start(None, &reason, &unproven),
            UnprovenStartAction::WatchPending,
            "an unknown SELL remains an obligation: {reason:?}"
        );
    }
}

#[test]
fn issue_2115_an_unresolved_sell_without_order_id_gets_its_own_pool_watcher() {
    assert_eq!(
        plan_unproven_start(
            None,
            &RestingStopReason::OfferPost("exact submission remains unresolved".into()),
            &CancellationDisposition::UnknownFailure {
                known_result: "book vacant".into()
            }
        ),
        UnprovenStartAction::WatchPending
    );
}

#[tokio::test]
async fn issue_2115_pending_watcher_shutdown_preserves_identity_or_reports_missing_one() {
    let seller = test_seller();
    for (current, pending) in [
        (None, false),
        (Some(identity(UNPROVEN_TC, 37)), false),
        (Some(identity(UNPROVEN_TC, 37)), true),
    ] {
        let has_identity = current.is_some();
        let book = BookState::resting(37);
        book.pending.store(pending, Ordering::SeqCst);
        let deal = deal_with(UNPROVEN_TC, book);
        let mut resting = HashMap::new();
        let mut error = None;
        apply_drained_outcome(
            &seller,
            deal,
            current,
            Ok(dexdo::seller::liveness::RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Shutdown,
                disposition: CancellationDisposition::UnknownFailure {
                    known_result: "exact SELL submission still unreadable".into(),
                },
            }),
            &mut resting,
            &mut error,
        )
        .await;
        if has_identity {
            assert_eq!(resting[UNPROVEN_TC].1.order_id, 37);
            assert_eq!(error.is_some(), pending);
        } else {
            assert!(resting.is_empty());
            assert!(error.unwrap().to_string().contains("SELL submission"));
        }
    }
}

/// POINT 2. The sweep's documented behaviour -- every entry swept regardless, the FIRST failure
/// returned -- had no test at all once the entries stopped being batched. A later entry must still
/// be cancelled rather than abandoned behind an earlier failure.

/// Red if the sweep returns on its first error: the second book never sees a cancel.
#[tokio::test(start_paused = true)]
async fn the_sweep_cancels_every_entry_and_reports_only_the_first_failure() {
    let seller = test_seller();
    let first = BookState::resting(29);
    let second = BookState::resting(30);

    let error = sweep_unconfirmed_resting_offers(
        &seller,
        vec![
            entry(UNPROVEN_TC, Arc::clone(&first), 29),
            entry(PROVEN_TC, Arc::clone(&second), 30),
        ],
    )
    .await
    .expect("both entries are unconfirmable, so the sweep must raise");

    assert_eq!(
        first.cancels(),
        vec![29],
        "the first entry must be cancelled"
    );
    assert_eq!(
        second.cancels(),
        vec![30],
        "the second entry must be cancelled TOO, not abandoned behind the first failure"
    );
    // Matched on the BARE account id: the reporter renders addresses in self-dapp form, so the
    // `0:` prefix the constant carries is not in the output. Asserting the constant verbatim would
    // fail on the rendering rather than on the behaviour.
    let bare_first = &UNPROVEN_TC[2..18];
    let bare_second = &PROVEN_TC[2..18];
    let rendered = error.to_string();
    assert!(
        rendered.contains(bare_first),
        "the reported failure must be the FIRST one, got: {rendered}"
    );
    assert!(
        !rendered.contains(bare_second),
        "only the first failure is reported, got: {rendered}"
    );
}

#[tokio::test(start_paused = true)]
async fn issue_2115_pool_start_and_residual_start_preserve_pending_mode() {
    use super::{
        prepare_pool_deal, prepare_pool_deal_reporting_identity, watch_pool_deal, PoolDealStart,
        SellerPoolContext,
    };
    use futures::FutureExt as _;
    let seller = test_seller();
    let book = BookState::resting(37);
    book.pending.store(true, Ordering::SeqCst);
    book.reads_fail.store(true, Ordering::SeqCst);
    let deal = deal_with(UNPROVEN_TC, book.clone());
    let root = tempfile::tempdir().unwrap();
    let context = SellerPoolContext {
        deals_dir: Some(root.path()),
        contracts: std::path::Path::new("unused-manifest.json"),
        note_addr: OWNER_NOTE,
        frame_model: "mock",
        gateway_advertise: "127.0.0.1:0",
        advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::default(),
    };
    let shutdown = futures::future::pending::<()>().fuse();
    tokio::pin!(shutdown);
    let mut stopped = false;
    let produced = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        prepare_pool_deal_reporting_identity(
            &seller,
            &deal,
            &context,
            false,
            shutdown.as_mut(),
            &mut stopped,
        ),
    )
    .await
    .expect("pending journal cannot block another deal's startup")
    .unwrap();
    assert!(matches!(
        produced,
        PoolDealStart::StoppedUnproven {
            identity: None,
            disposition: CancellationDisposition::UnknownFailure { .. },
            ..
        }
    ));
    assert_eq!(
        book.reads.load(Ordering::SeqCst),
        0,
        "journal phase precedes failing inspection"
    );
    let healthy = deal_with(PROVEN_TC, BookState::resting(38));
    assert!(matches!(
        prepare_pool_deal_reporting_identity(
            &seller,
            &healthy,
            &context,
            true,
            shutdown.as_mut(),
            &mut stopped
        )
        .await
        .unwrap(),
        PoolDealStart::Watching(None)
    ));
    let (identity, pending_owner) = prepare_pool_deal(
        &seller,
        &deal,
        &context,
        false,
        shutdown.as_mut(),
        &mut stopped,
    )
    .await
    .unwrap();
    assert!(identity.is_none());
    assert_eq!(pending_owner.as_deref(), Some(OWNER_NOTE));
    let (fills, mut fill_rx) = tokio::sync::mpsc::unbounded_channel();
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let watcher = watch_pool_deal(
        &seller,
        deal,
        identity,
        pending_owner,
        fills,
        context.advertise_probe,
        stop_rx,
    );
    tokio::pin!(watcher);
    tokio::select! {
        result = &mut watcher => panic!("getter outage must retain pending phase: {:?}", result.2),
        _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
    }
    assert!(book.reads.load(Ordering::SeqCst) > 0);
    assert!(fill_rx.try_recv().is_err());
    stop_tx.send(true).unwrap();
    let (_, identity, result) = watcher.await;
    assert!(identity.is_none());
    assert!(matches!(
        result.unwrap(),
        dexdo::seller::liveness::RestingSellerOutcome::Stopped {
            reason: RestingStopReason::Shutdown,
            disposition: CancellationDisposition::UnknownFailure { .. }
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn issue_2115_all_pool_startup_entries_keep_pending_before_getter_errors() {
    use super::{
        admit_seller_startup_deals, load_seller_pool_deals_with_scope, run_seller_pool,
        save_pool_deal_handle, seller_deal_capacity, sweep_seller_startup_offers,
        SellerPoolContext,
    };
    use futures::FutureExt as _;
    let mut seller = test_seller();
    seller.server_task.abort();
    seller.server_task = tokio::spawn(std::future::pending());
    let root = tempfile::tempdir().unwrap();
    let context = SellerPoolContext {
        deals_dir: Some(root.path()),
        contracts: std::path::Path::new("unused-manifest.json"),
        note_addr: OWNER_NOTE,
        frame_model: "mock",
        gateway_advertise: "127.0.0.1:0",
        advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::default(),
    };
    let books = [BookState::resting(37), BookState::resting(38)];
    let mut deals = Vec::new();
    let mut backends = HashMap::new();
    for (index, (tc, book)) in [UNPROVEN_TC, PROVEN_TC].into_iter().zip(&books).enumerate() {
        book.pending.store(true, Ordering::SeqCst);
        book.reads_fail.store(true, Ordering::SeqCst);
        let mut deal = deal_with(tc, book.clone());
        deal.nonce += index as u64;
        deal.market = Some(dexdo_core::MarketManifest {
            network: deal.chain.network().into(),
            frame_model: "mock".into(),
            model_hash: dexdo_core::model_hash_for("mock"),
            inference_order_book: OWNER_NOTE.into(),
            root_model: OWNER_NOTE.into(),
            token_contract: tc.into(),
            seller_note: OWNER_NOTE.into(),
            nonce: deal.nonce,
            price_per_tick: dexdo_core::PRICE_STEP,
            max_ticks: 2,
        });
        save_pool_deal_handle(&context, &deal).unwrap();
        backends.insert(super::deals::normalize_addr(tc), deal.chain.clone());
        deals.push(deal);
    }
    let backend = |market: &dexdo_core::MarketManifest| -> anyhow::Result<Arc<dyn ChainBackend>> {
        Ok(backends[&super::deals::normalize_addr(&market.token_contract)].clone())
    };
    let retained = sweep_seller_startup_offers(&context, &deals[0].chain, UNPROVEN_TC, backend)
        .await
        .unwrap();
    assert_eq!(retained.len(), 2);
    let capacity = seller_deal_capacity(context.deals_dir, OWNER_NOTE, backend)
        .await
        .unwrap();
    assert_eq!((capacity.working, capacity.candidates), (2, 0));
    let loaded = load_seller_pool_deals_with_scope(
        &context,
        deals.remove(0),
        0,
        Some(&retained),
        |market| Ok((backend(market)?, dexdo::seller::UpstreamConfig::Mock)),
    )
    .await
    .unwrap();
    let policy = super::policy::SellerRuntimePolicy {
        after_deal_done: super::policy::SellerAfterDealDoneAction::Retire,
        buyer_no_show: super::policy::SellerBuyerNoShowAction::RetireGateway,
        dispute_against_me: super::policy::SellerDisputeAgainstMeAction::ReleaseIfClean,
        max_open_deals: 2,
    };
    let admitted = admit_seller_startup_deals(loaded, &context, &policy)
        .await
        .unwrap();
    assert_eq!(admitted.len(), 2);
    assert!(books
        .iter()
        .all(|book| book.reads.load(Ordering::SeqCst) == 0));
    let stop = tokio::time::sleep(std::time::Duration::from_secs(1)).fuse();
    tokio::pin!(stop);
    let mut observed_stop = false;
    let provisions = AtomicUsize::new(0);
    let counter = &provisions;
    let mut no_provision = move |_: String, _: u64, _: u64, _: u64| async move {
        counter.fetch_add(1, Ordering::SeqCst);
        Err(anyhow::anyhow!("unexpected provision"))
    };
    let result = run_seller_pool(
        &seller,
        admitted,
        context,
        &policy,
        &mut no_provision,
        stop.as_mut(),
        &mut observed_stop,
    )
    .await;
    assert!(
        result.unwrap_err().to_string().contains("SELL submission"),
        "shutdown must report unresolved submissions"
    );
    assert_eq!(provisions.load(Ordering::SeqCst), 0);
    assert!(observed_stop);
    assert!(books
        .iter()
        .all(|book| book.reads.load(Ordering::SeqCst) > 0));
    assert!(books
        .iter()
        .all(|book| book.posts.load(Ordering::SeqCst) == 0));
    seller.server_task.abort();
}

#[tokio::test]
async fn issue_2115_gateway_start_without_order_cannot_report_pending_sell_absent() {
    for unreadable in [false, true] {
        for shutdown in [true, false] {
            let book = BookState::resting(37);
            book.pending.store(!unreadable, Ordering::SeqCst);
            book.pending_read_fails.store(unreadable, Ordering::SeqCst);
            book.reads_fail.store(true, Ordering::SeqCst);
            let deal = deal_with(UNPROVEN_TC, book.clone());
            let gateway = async {
                if shutdown {
                    std::future::pending::<()>().await;
                }
                Err(anyhow::anyhow!("fixture gateway cannot start"))
            };
            let stop = async {
                if !shutdown {
                    std::future::pending::<()>().await;
                }
            };
            let result = super::start_seller_gateway_with_liveness(
                gateway,
                deal.chain.as_ref(),
                &deal.cfg,
                None,
                stop,
            )
            .await;
            match result {
                Ok(super::SellerGatewayStartup::Stopped { disposition, .. }) => {
                    assert!(
                        matches!(disposition, CancellationDisposition::UnknownFailure { .. }),
                        "an outstanding/unreadable SELL cannot become absent: {disposition:?}"
                    );
                }
                Err(_) => assert!(!shutdown, "shutdown must report its unresolved disposition"),
                Ok(super::SellerGatewayStartup::Ready(_)) => panic!("interrupted startup"),
            }
            assert_eq!(book.reads.load(Ordering::SeqCst), 0);
            assert_eq!(book.posts.load(Ordering::SeqCst), 0);
            assert_eq!(book.pending.load(Ordering::SeqCst), !unreadable);
        }
    }
}

#[tokio::test]
async fn issue_2115_gateway_start_exact_cancel_cannot_release_pending_execution() {
    for unreadable in [false, true] {
        for shutdown in [true, false] {
            let book = BookState::resting(37);
            book.pending.store(!unreadable, Ordering::SeqCst);
            book.pending_read_fails.store(unreadable, Ordering::SeqCst);
            book.cancel_succeeds.store(true, Ordering::SeqCst);
            let deal = deal_with(UNPROVEN_TC, book.clone());
            let exact = identity(UNPROVEN_TC, 37);
            let gateway = async {
                if shutdown {
                    std::future::pending::<()>().await;
                }
                Err(anyhow::anyhow!("fixture gateway cannot start"))
            };
            let stop = async {
                if !shutdown {
                    std::future::pending::<()>().await;
                }
            };
            let result = super::start_seller_gateway_with_liveness(
                gateway,
                deal.chain.as_ref(),
                &deal.cfg,
                Some(&exact),
                stop,
            )
            .await
            .unwrap();
            assert!(
                matches!(
                    result,
                    super::SellerGatewayStartup::Stopped {
                        disposition: CancellationDisposition::UnknownFailure { .. },
                        ..
                    }
                ),
                "removing one ask cannot prove its outstanding execution is terminal"
            );
            assert_eq!(book.cancels(), vec![exact.order_id]);
            assert!(book.cancelled.load(Ordering::SeqCst));
            assert_eq!(book.pending.load(Ordering::SeqCst), !unreadable);
            assert_eq!(book.posts.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn issue_2115_gateway_start_proven_submission_keeps_clean_shutdown() {
    for has_identity in [false, true] {
        let book = BookState::resting(37);
        book.cancel_succeeds.store(true, Ordering::SeqCst);
        book.cancelled.store(!has_identity, Ordering::SeqCst);
        let deal = deal_with(UNPROVEN_TC, book.clone());
        let exact = identity(UNPROVEN_TC, 37);
        let result = super::start_seller_gateway_with_liveness(
            std::future::pending::<anyhow::Result<dexdo::seller::RunningSeller>>(),
            deal.chain.as_ref(),
            &deal.cfg,
            has_identity.then_some(&exact),
            std::future::ready(()),
        )
        .await
        .unwrap();
        let super::SellerGatewayStartup::Stopped { disposition, .. } = result else {
            panic!("interrupted startup")
        };
        if has_identity {
            assert!(matches!(disposition, CancellationDisposition::Cancelled));
            assert_eq!(book.cancels(), vec![exact.order_id]);
        } else {
            assert!(matches!(
                disposition,
                CancellationDisposition::AlreadyAbsent
            ));
            assert!(book.cancels().is_empty());
        }
        assert_eq!(book.posts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn issue_2115_gateway_start_cancel_race_retains_the_funded_buyer() {
    for unreadable in [false, true] {
        let book = BookState::resting(37);
        book.pending.store(true, Ordering::SeqCst);
        book.pending_read_fails.store(unreadable, Ordering::SeqCst);
        let deal = deal_with(UNPROVEN_TC, book.clone());
        let exact = identity(UNPROVEN_TC, 37);
        let startup = super::start_seller_gateway_with_liveness(
            std::future::pending::<anyhow::Result<dexdo::seller::RunningSeller>>(),
            deal.chain.as_ref(),
            &deal.cfg,
            Some(&exact),
            std::future::ready(()),
        );
        let buyer = async {
            while book.cancels().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            book.cross();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(startup, buyer)
        })
        .await
        .expect("the funded cancel race must resolve within the existing cancel poll cadence");
        let super::SellerGatewayStartup::Stopped {
            disposition: CancellationDisposition::AlreadyMatched(matched),
            ..
        } = result.unwrap()
        else {
            panic!("a funded buyer must retain its existing recovery obligation")
        };
        assert_eq!(matched.token_contract, exact.token_contract);
        assert_eq!(book.cancels(), vec![exact.order_id]);
        assert!(book.pending.load(Ordering::SeqCst));
        assert_eq!(book.posts.load(Ordering::SeqCst), 0);
    }
}
