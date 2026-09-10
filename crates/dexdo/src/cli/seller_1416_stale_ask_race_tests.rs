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
    ChainBackend, ChainError, DealChainSnapshot, Match, Note, OfferListing, OrderBookOrder,
    SellOffer, Settlement, StreamSnapshot, TokenContract,
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
    /// Flipped mid-test: the ask was resting, and now a buyer has crossed it.
    matched: AtomicBool,
    /// The generation the BOOK holds. A relist advances it; the pool's captured copy does not.
    book_order_id: Mutex<u128>,
    /// Which generation each cancel was aimed at.
    cancel_targets: Mutex<Vec<u128>>,
    /// How many times the deal was actually SERVED -- `serve_watched_match` reads the deal snapshot
    /// first, so this counts real attempts to discharge a match rather than mentions of one.
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
}

#[async_trait::async_trait]
impl ChainBackend for StaleAskBackend {
    /// The book's own rows. Empty once crossed -- the row is gone -- and otherwise the CURRENT
    /// generation, which a relist has moved.
    async fn raw_resting_sell_orders_for_tc(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Vec<OrderBookOrder>, ChainError> {
        if token_contract != &self.token_contract || self.book.matched.load(Ordering::SeqCst) {
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
    /// test can say WHICH generation was aimed at.
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

    /// The FIRST chain read `serve_watched_match` performs, through `read_coherent_deal_capacity`.
    /// Counting it is how these tests tell "the match was served" from "the match was mentioned".
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
        buyer_pubkey: dexdo_core::LocalNote::generate().pubkey(),
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
    let backend = StaleAskBackend {
        token_contract: token_contract.to_string(),
        book,
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
            cursor_path: std::env::temp_dir().join(format!(
                "dexdo-1416-cursor-{}-{}.json",
                std::process::id(),
                token_contract.len()
            )),
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

    // The ask is crossed AFTER the entry was built -- the state the first stand could not express.
    book.cross();

    let error = sweep_unconfirmed_resting_offers(&seller, survivors).await;

    assert_eq!(
        book.serves(),
        1,
        "the sweep must ATTEMPT to serve the buyer, not report that nobody did"
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

/// POINT 3. `Stopped { identity: None }` used to leave the gateway route registered -- the one branch
/// that neither seated anything for the sweep nor released what it had taken. Nothing to sweep and
/// nothing to watch means the route goes back, exactly as the isolated-failure branch releases it.
#[test]
fn an_unproven_start_with_no_identity_releases_the_route_instead_of_leaking_it() {
    let unproven = CancellationDisposition::UnknownFailure {
        known_result: String::new(),
    };
    for reason in [
        RestingStopReason::Shutdown,
        RestingStopReason::Watcher("upstream died".to_string()),
    ] {
        assert_eq!(
            plan_unproven_start(None, &reason, &unproven),
            UnprovenStartAction::Release,
            "with no identity there is nothing to keep: {reason:?}"
        );
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
