//! On-chain abstraction and the mock implementation for.

//! brings up **only what e2e needs**: offer/match, `open_stream` (holding the exact `2P`
//! seller bond + writing the enc-endpoint to the endpoints file), `claim_tokens`/`finalize`,
//! `read_handover`, `stop`, and dispute settlement. No networked on-chain.

//! Opening a stream pays no one: it freezes one probe tick inside the buyer's escrow, and the seller earns
//! that tick only after probe acceptance. Later output is paid only through promoted consumption claims.

use crate::machine::Settlement;
use crate::note::{Note, NotePubkey};
use crate::params::{ProtocolConsts, Shell};
use async_trait::async_trait;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

mod accounting;
mod deal_conservation;
mod mock;
mod types;
pub use accounting::*;
pub use deal_conservation::*;
pub use mock::*;
pub use types::*;

#[derive(Clone)]
pub struct HeartbeatGuard {
    generation: Arc<AtomicU64>,
    expected: u64,
}

impl HeartbeatGuard {
    pub fn new(generation: Arc<AtomicU64>) -> Self {
        let expected = generation.load(Ordering::SeqCst);
        Self {
            generation,
            expected,
        }
    }

    pub fn unchanged(&self) -> bool {
        self.generation.load(Ordering::SeqCst) == self.expected
    }
}

/// Validate the exact on-chain state and deal price used by seller resume before any write.
pub fn validate_seller_resume_state(
    token_contract: &TokenContract,
    state: DealChainState,
    price_per_tick: Shell,
) -> Result<(), ChainError> {
    let token_contract_display = crate::address::display_self_dapp(token_contract);
    let mut blockers = Vec::new();
    if !state.funded {
        blockers.push("funded=false".to_string());
    }
    if state.disputed {
        blockers.push("disputed".to_string());
    }
    if state.probe_accepted && !state.opened {
        blockers.push("probeAccepted without opened".to_string());
    }
    if !state.opened {
        if state.deposit < u128::from(price_per_tick) {
            blockers.push(format!(
                "deposit={}, price_per_tick={price_per_tick}: TokenContract cannot be opened",
                state.deposit
            ));
        }
        for (key, value) in [
            ("probeTick", state.probe_tick),
            ("finalizedOwed", state.finalized_owed),
            ("tokensFinal", state.tokens_final),
            ("tokensPending", state.tokens_pending),
        ] {
            if value > 0 {
                blockers.push(format!("{key}={value}"));
            }
        }
    }
    if blockers.is_empty() {
        return Ok(());
    }
    Err(ChainError::Chain(format!(
        "TokenContract {token_contract_display} is matched but not openable for seller resume \
         ({}) -- use a fresh --nonce for a new TokenContract, or close/destroy the old TokenContract",
        blockers.join(", ")
    )))
}

/// On-chain abstraction. In -- `MockChainBackend`; in -- the chain adapter.

/// Brings up the minimum for e2e: discovery/book/oracle and subscriptions are the horizon.
#[async_trait]
pub trait ChainBackend: Send + Sync {
    /// Network identity used by persisted records and structured reports.

    /// An offline adapter names no chain, because it is on none. Real adapters override this with
    /// the label their deployment manifest declares.

    /// It used to return a compiled-in network name, so a mock reported itself as being on the test
    /// chain -- and anything reading that label could not tell a mock from the real thing.
    fn network(&self) -> &str {
        "mock"
    }

    /// Book discovery: the list of current offers with their sellers. The buyer
    /// filters/ranks against its frame (`buyer::routing::eligible_ranked`). Mock -- all offers;
    /// real -- reading `InferenceOrderBook`.
    async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError>;
    /// The seller posts an offer from a note.
    async fn post_offer(&self, offer: SellOffer, note: &dyn Note) -> Result<(), ChainError>;
    /// ensure THIS backend's note carries the current contract code before publishing an offer. The
    /// seller daemon path (`run_seller -> post_offer`) does NOT go through `provision_market`'s note-current
    /// gate, so a note orphaned by a contract redeploy (stale code_hash) would hit a raw `TVM_ERROR` from
    /// `postSellOffer`. Default `Ok(())` (mock/buyer/deal backends are not gated); the real seller backend
    /// overrides with the on-chain code_hash check, failing closed with an actionable "re-mint" message.
    async fn assert_note_current(&self) -> Result<(), ChainError> {
        Ok(())
    }
    /// before any seller write that can reach `PrivateNote.postSellOffer`, ensure the note was not
    /// permanently withdrawn. A withdrawn note is final by contract semantics and `postSellOffer` would revert
    /// with `ERR_INVALID_STATE` 151; the real chain backend overrides this with a read-only `getDetails().hasWithdrawn`
    /// preflight so the CLI can report a clear "use a fresh note" error before submitting.
    async fn assert_note_can_post_sell_offer(&self) -> Result<(), ChainError> {
        Ok(())
    }
    /// E2E-ADV-14: prove the seller note's RECORD covers this deal's exact `2P` mirror bond before the
    /// seller advertises readiness or rests an ask. `TokenContract.open()` is unreachable without
    /// `fundDeal`, and `fundDeal` hard-requires `amount >= _bondAmount() == 2 * _pricePerTick`, so an
    /// ask resting on a deal the note cannot bond is a promise the seller cannot keep -- a fill would
    /// leave the buyer's escrow committed against collateral that can never be posted. The bond is paid
    /// from `getDetails().balance[2]`; the physical ECC[2] gas pocket is a different pot and never
    /// substitutes for it. Default `Ok(())` (mock/buyer/deal backends are not gated); the real seller
    /// backend overrides this with the on-chain read.
    async fn assert_note_covers_seller_bond(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<(), ChainError> {
        Ok(())
    }
    /// ensure the per-deal `TokenContract` being advertised is FRESH (deployed but unused) before resting
    /// an ask on it. A deterministic `(sellerPubkey, nonce)` TC is a single-use per-deal resource -- if a prior
    /// deal already `opened`/`funded`/`disputed` it (or left residual deposit/probe/finalized accounting), the
    /// seller's pre-stream steps (`fundDeal`/`open`) revert with a raw `TVM_ERROR` (`ERR_ALREADY_OPEN`
    /// 321 and kin). Default `Ok(())` (mock/buyer/deal backends are not gated); the real seller backend overrides
    /// with the on-chain `getState` check, failing closed with an actionable "use a fresh nonce / recover+destroy".
    async fn assert_token_contract_fresh(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<(), ChainError> {
        Ok(())
    }
    /// After `post_offer`, confirm the seller-visible on-chain outcome. Backends without a live IOB return `None`.
    async fn confirm_offer_outcome(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Option<SellOfferOutcome>, ChainError> {
        Ok(None)
    }
    /// Read the owner-note placement/match facts for one exact TokenContract
    /// from a durable event lower bound.  This is intentionally separate from
    /// `confirm_offer_outcome`: reconciliation after a service restart cannot
    /// depend on a backend-local "post started" timestamp that vanished with
    /// the old process.  A backend which cannot make this authoritative read
    /// must return an error so a persisted seller marker remains fail-closed.
    async fn seller_offer_outcome_since(
        &self,
        _token_contract: &TokenContract,
        _since_unix: u64,
    ) -> Result<Option<SellOfferOutcome>, ChainError> {
        Err(ChainError::Chain(
            "marker-bounded seller offer event reconciliation is not supported by this backend"
                .to_string(),
        ))
    }
    /// Read the exact deal's post-offer latch.  This is deliberately separate from a
    /// book row: a successful `postSellOffer` can be accepted before an index/read
    /// path catches up, and a returned placement value is not by itself a proof that
    /// a successor post is safe.  Backends which cannot read the live deal return
    /// `None`; a persisted real-seller submission must then remain unconfirmed.
    async fn seller_offer_latch(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Option<DealOfferLatch>, ChainError> {
        Ok(None)
    }
    /// Read the authoritative sell-offer terms for a real per-deal `TokenContract`. The real seller path uses
    /// this before posting an ask so CLI defaults/prompts cannot diverge from the already-deployed TC config.
    /// Mock backends have no on-chain TC config, so they return `None`.
    async fn sell_offer_terms(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Option<(u64, u64)>, ChainError> {
        Ok(None)
    }
    /// Raw active SELL rows for this exact TokenContract, before any buyer quote coalescing. Backends
    /// without a persistent authoritative book return an empty list; the real seller overrides this
    /// with a strict per-order `InferenceOrderBook` read.
    async fn raw_resting_sell_orders_for_tc(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Vec<OrderBookOrder>, ChainError> {
        Ok(Vec::new())
    }
    /// permissionlessly expire one exact order whose deadline has passed.

    /// `InferenceOrderBook.expireOrder(orderId)` is `public` and takes no owner argument -- a keeper,
    /// the counterparty or the seller itself may call it -- and it is idempotent: `_doExpire` silently
    /// no-ops on an order that is already gone AND on one that is still live, so it can be raced and
    /// spammed (`contracts/airegistry/InferenceOrderBook.sol:1679-1691`). For a SELL the removal
    /// frees the deal's `_offerPosted` latch through `onSellClosed` and emits `InferenceOrderExpired`
    /// (`contracts/airegistry/InferenceOrderBook.sol:1138-1149`), which is the transition a successor
    /// offer waits for.

    /// Because the call is silent in BOTH failure directions, `Ok(())` proves only that the request
    /// was submitted. The caller must confirm the resulting book and TokenContract state before
    /// treating the order as reaped.
    async fn expire_resting_sell_order(
        &self,
        token_contract: &TokenContract,
        order_id: u128,
    ) -> Result<(), ChainError> {
        Err(ChainError::Chain(format!(
            "permissionless order expiry is not supported for TokenContract {}, \
             order {order_id}",
            crate::address::display_self_dapp(token_contract)
        )))
    }
    /// the deal's own `getOffer()` latch -- the authoritative "may a successor ask rest here?".

    /// Backends that cannot read the deal return `None`, which is never proof that the latch is free:
    /// a caller that cannot read `offerPosted` must fail closed rather than post an offer
    /// `TokenContract.postFromNote` would silently drop.
    async fn token_contract_offer_latch(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Option<DealOfferLatch>, ChainError> {
        Ok(None)
    }
    /// Cancel one exact resting SELL owned by this seller backend. The caller must reread the
    /// authoritative book/match state before reporting a terminal outcome.
    async fn cancel_resting_sell_order(
        &self,
        token_contract: &TokenContract,
        order_id: u128,
    ) -> Result<(), ChainError> {
        Err(ChainError::Chain(format!(
            "exact resting SELL cancellation is not supported for TokenContract {}, order {order_id}",
            crate::address::display_self_dapp(token_contract)
        )))
    }
    /// Capture the backend's exact pre-submit event boundary and submit one owner-authorized cancel.

    /// The returned watch is the correlation boundary for a later terminal chain rejection. Backends
    /// without an event stream retain the existing submit behavior and return an empty marker.
    async fn begin_resting_sell_cancel(
        &self,
        token_contract: &TokenContract,
        order_id: u128,
    ) -> Result<RestingSellCancelWatch, RestingSellCancelStartError> {
        self.cancel_resting_sell_order(token_contract, order_id)
            .await
            .map_err(RestingSellCancelStartError::Submit)?;
        Ok(RestingSellCancelWatch::default())
    }
    /// Read an exact `InferenceOrderCancelRejected` emitted after `watch` for this order and owner.

    /// `None` means the chain has not emitted that terminal fact yet. Implementations must correlate
    /// all three facts: the pre-submit marker, `order_id`, and `owner_note`.
    async fn resting_sell_cancel_rejection_after(
        &self,
        _token_contract: &TokenContract,
        _order_id: u128,
        _owner_note: &str,
        _watch: &RestingSellCancelWatch,
    ) -> Result<Option<u8>, ChainError> {
        Ok(None)
    }
    /// The buyer sends a buy order; the order book records its pubkey into `token_contract`.
    async fn place_buy(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
    ) -> Result<(), ChainError>;
    /// Model-only buy: place a limit buy by the backend's `model_hash` WITHOUT a pre-known per-deal
    /// `TokenContract`, for the order the buyer CHOSE after seeing the book -- `ticks` at up to
    /// `max_price_per_tick`, funded by `escrow`. `placeInferenceBuy` is model-book-wide, so the buyer
    /// does not name a target -- it learns the matched TC afterwards from its OWN note's fill event
    /// ([`Self::wait_matched_token_contract`]). Default: unsupported; the real chain buyer backend overrides it.
    /// `deadline` is MANDATORY on-chain (absolute unix seconds): an order with no deadline is rejected, and
    /// once past it anyone may expire the order permissionlessly, returning the escrow.

    /// A subscription is selected by `flags` alone (`flags::AON | flags::SUBSCRIPTION`) -- the term is not a
    /// parameter, since every subscription is one month protocol-wide. The book then also requires a volume
    /// that divides evenly by [`SUBSCRIPTION_WEEKS`] and does not exceed [`SUBSCRIPTION_MAX_TICKS`].
    async fn place_buy_by_model(
        &self,
        _note: &dyn Note,
        _ticks: u128,
        _max_price_per_tick: u128,
        _escrow: u128,
        _flags: u8,
        _deadline: u64,
    ) -> Result<(), ChainError> {
        Err(ChainError::Chain(
            "place_buy_by_model: model-only buy is only supported on the real chain buyer backend"
                .into(),
        ))
    }
    /// Stable identity of the model-scoped order book used for durable recovery.
    fn model_buy_order_book_identity(&self) -> Option<String> {
        None
    }
    /// Submit one model buy while exposing the exact signed-BOC identity and final fill cursor
    /// immediately before the single money POST.
    #[allow(clippy::too_many_arguments)]
    async fn place_buy_by_model_with_submit_identity(
        &self,
        _note: &dyn Note,
        _quoted_order: Option<&OrderBookOrder>,
        _ticks: u128,
        _max_price_per_tick: u128,
        _escrow: u128,
        _cursor: &mut MatchWatchCursor,
        _before_post: &mut (dyn FnMut(String, MatchWatchCursor, u128) -> Result<(), ChainError>
                  + Send),
    ) -> Result<(), ChainError> {
        Err(ChainError::Chain(
            "backend cannot expose an exact pre-POST submit identity; no BOC was sent".into(),
        ))
    }
    /// Poll new buyer fills for an explicitly journaled order book.
    async fn poll_matched_model_buys_for_order_book(
        &self,
        _order_book: &str,
        _cursor: &mut MatchWatchCursor,
    ) -> Result<Vec<MatchedFill>, ChainError> {
        Err(ChainError::Chain(
            "buyer fill polling for an explicit order book is not supported by this backend".into(),
        ))
    }
    /// Poll owner-facing buyer fills with the buyer order id retained for subscription attribution.
    async fn poll_attributed_model_buys_for_order_book(
        &self,
        _order_book: &str,
        _cursor: &mut MatchWatchCursor,
    ) -> Result<Vec<(u128, MatchedFill)>, ChainError> {
        Err(ChainError::Chain(
            "attributed buyer fill polling is not supported by this backend".to_string(),
        ))
    }
    /// Accepted subscription placements at or above a pre-POST order-id floor.

    /// Reconciled from the ordinary `InferenceOrderPlaced` fact -- a subscription is a flagged BUY order,
    /// not a separate book primitive -- so the match criteria are the order's own terms plus its term length.
    #[allow(clippy::too_many_arguments)]
    async fn subscription_placements_since(
        &self,
        _order_book: &str,
        _buyer_note: &str,
        _order_id_floor: u128,
        _max_price_per_tick: u128,
        _ticks: u128,
    ) -> Result<Vec<InferenceSubscriptionPlacement>, ChainError> {
        Err(ChainError::Chain(
            "subscription placement reconciliation is not supported by this backend".to_string(),
        ))
    }
    /// Owner-facing BUY outcome facts this note has in one order book, oldest first.

    /// The book names the owning note on `InferenceOrderPlaced`, `InferenceOrderCancelled`,
    /// `InferenceOrderExpired` and `InferenceOrderRejected`, so a durable buyer submit record is resolved
    /// by the outcome that actually happened rather than by absence or by a clock.
    async fn buyer_order_facts_for_note(
        &self,
        _order_book: &str,
        _buyer_note: &str,
    ) -> Result<Vec<BuyerOrderFact>, ChainError> {
        Err(ChainError::Chain(
            "buyer order outcome facts are not supported by this backend".to_string(),
        ))
    }
    /// Whether one order still rests as a BUY owned by this note.
    async fn buyer_order_is_active_for_owner(
        &self,
        _order_book: &str,
        _order_id: u128,
        _buyer_note: &str,
    ) -> Result<bool, ChainError> {
        Err(ChainError::Chain(
            "buyer order activity reconciliation is not supported by this backend".to_string(),
        ))
    }
    /// Read-only guard for automated model-only buyer selection. The real chain backend
    /// `placeInferenceBuy(modelHash,...)` cannot name an order id or `TokenContract`, so an
    /// executable quote is submit-safe only when the raw on-chain price/time matcher reaches the
    /// same ask.
    async fn assert_model_buy_matches_executable_quote(
        &self,
        _ticks: u128,
        _max_price_per_tick: u128,
    ) -> Result<(), ChainError> {
        Ok(())
    }
    /// Return the authoritative submit-safe row for a model-only buyer selection. The real chain backend
    /// returns the full on-chain row so rendering, durable journaling, and the immediate
    /// pre-submit guard preserve one identity without a lossy listing round trip. Backends that
    /// cannot expose the on-chain row retain the legacy preflight + discovery path.
    async fn submit_safe_model_buy_quote_order(
        &self,
        ticks: u128,
        max_price_per_tick: u128,
    ) -> Result<Option<OrderBookOrder>, ChainError> {
        self.assert_model_buy_matches_executable_quote(ticks, max_price_per_tick)
            .await?;
        Ok(None)
    }
    /// Read-only guard for explicit `--token-contract` buyer selection. A displayed quote is submit-safe only
    /// if the model-wide `placeInferenceBuy(modelHash,...)` matcher would actually fund this TokenContract,
    /// and the selected TC is still unused. Default `Ok(())` keeps mock backends simple; the real chain backend fails
    /// closed before the CLI emits `quote_selected` or sends escrow.
    async fn assert_explicit_buy_matches_executable_quote(
        &self,
        _token_contract: &TokenContract,
        _ticks: u128,
        _max_price_per_tick: u128,
    ) -> Result<(), ChainError> {
        Ok(())
    }
    /// Return the submit-safe executable row for an explicit `--token-contract` buyer selection. Backends that
    /// cannot expose the row return `Ok(None)` and the CLI falls back to legacy quote rendering after the
    /// preflight above. Real the chain returns `Some(row)` so the displayed quote is identical to the row that
    /// the model-wide matcher can actually fund.
    async fn submit_safe_explicit_buy_quote_order(
        &self,
        _token_contract: &TokenContract,
        _ticks: u128,
        _max_price_per_tick: u128,
    ) -> Result<Option<OrderBookOrder>, ChainError> {
        Ok(None)
    }
    /// The current a real chain submit path requires raw price/time depth with per-fill TC state checks.
    /// Mock/backends without that order-book limitation keep the generic quote.
    fn requires_submit_safe_single_ask_quote(&self) -> bool {
        false
    }
    /// Whether a model-only BUY is intentionally allowed to rest without a currently executable ask.
    /// The ordinary CLI path remains fail-closed; the real chain buyer enables this only for the
    /// explicit `--wait-for-seller` mode.
    fn allows_resting_model_buy(&self) -> bool {
        false
    }
    /// After a model-only buy, learn the matched per-deal `TokenContract` from THIS note's owner-facing
    /// `InferenceFilledConfirmed` ext-out -- each side reads only its own note,
    /// no shared-book index. `since_unix` drops a prior deal's fill on a reused note. Default: unsupported;
    /// the real chain buyer backend overrides it.
    async fn wait_matched_token_contract(
        &self,
        _since_unix: i64,
        _timeout: std::time::Duration,
    ) -> Result<Option<MatchedFill>, ChainError> {
        Err(ChainError::Chain(
            "wait_matched_token_contract: only supported on the real chain buyer backend".into(),
        ))
    }
    /// After model-only resume recovers a matched `TokenContract` from this note's fill event, prove by
    /// chain facts that the deal still belongs to the current buyer/backend and is still resumable. The
    /// default fails closed; real chain buyer backends override it with `getState`/model/buyer checks.
    async fn assert_model_only_resume_target(
        &self,
        token_contract: &TokenContract,
    ) -> Result<(), ChainError> {
        Err(ChainError::Chain(format!(
            "model-only resume validation is not supported for {}",
            crate::address::display_self_dapp(token_contract)
        )))
    }
    /// Non-blocking seller resume probe. Returns `Some(match)` only when the per-deal TC is already matched and
    /// still openable by the seller (funded, no prior handover/stream state). This is intentionally separate
    /// from [`Self::read_match`], which may wait for a future match after the seller has posted an offer.
    async fn read_openable_match_now(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Option<Match>, ChainError> {
        Ok(None)
    }
    /// Poll all new owner-facing seller fills for this note/model without
    /// reducing them to a single [`Match`]. The authoritative event retains the
    /// filled tick count required for exact partial-capacity accounting.
    async fn poll_seller_fills(
        &self,
        _note: &dyn Note,
        _cursor: &mut MatchWatchCursor,
    ) -> Result<Vec<MatchedFill>, ChainError> {
        Ok(Vec::new())
    }
    /// The seller waits for/reads the match on its own `token_contract`.
    async fn read_match(&self, token_contract: &TokenContract) -> Result<Match, ChainError>;
    /// The seller opens a stream: holds the exact `2P` seller bond and writes
    /// `encrypt_to(buyer_pubkey, endpoint)` to the endpoints file. Moves no money -- the escrow stays whole
    /// and is earned only through promoted claims.
    async fn open_stream(
        &self,
        token_contract: &TokenContract,
        enc_endpoint: Vec<u8>,
        note: &dyn Note,
    ) -> Result<(), ChainError>;
    /// The buyer reads the endpoint ciphertext from the endpoints file.
    async fn read_handover(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<Vec<u8>>, ChainError>;
    /// Seller-only: accept the probe tick after `PROBE_WINDOW` of buyer silence.

    /// This is the gate on the whole deal: before it the trial tick is owed to nobody and `claim_tokens` is
    /// rejected outright, so a deal whose probe is never accepted can never pay the seller anything. Silence
    /// on a live endpoint is consent; a buyer who finds nothing there stops instead, and the trial tick burns
    /// on both sides. Default: unsupported.
    async fn accept_probe(&self, token_contract: &TokenContract) -> Result<(), ChainError> {
        Err(ChainError::Chain(format!(
            "accept_probe not supported for {}",
            crate::address::display_self_dapp(token_contract)
        )))
    }
    /// The seller claims CUMULATIVE consumption in tokens.

    /// `cumulative_tokens` is an absolute running total, never a delta: the contract rejects any value
    /// below the previous claim. It must also respect two bounds the caller is responsible for honouring,
    /// because the contract REJECTS rather than trims an out-of-bounds claim:
    /// - the claim ceiling ([`DealSubscription::claim_cap`]) -- the whole funded volume for an ordinary
    /// deal, one weekly quota per started week for a subscription;
    /// - the combined bound ([`ProtocolConsts::max_claim_delta`]) -- no more output than the elapsed time
    /// could physically have produced and never more than hard per-call `MAX_CLAIM_DELTA`, plus
    /// `MIN_CLAIM_INTERVAL` spacing between claims.

    /// Landing a claim advances the contract's three-stage final/superseded/pending pipeline, so the two
    /// newest cumulative claims retain their independent contest windows.
    async fn claim_tokens(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
        cumulative_tokens: u128,
    ) -> Result<(), ChainError>;
    /// Permissionless promotion of the pending claims after `CLAIM_PROMOTE_WINDOW` of buyer silence.

    /// This is what makes the LAST claim of a deal payable at all: nothing supersedes it, so without this
    /// call it would stay contestable forever. For an ordinary deal it also settles and closes once the
    /// funded volume is exhausted. Default: unsupported.
    async fn finalize(&self, token_contract: &TokenContract) -> Result<(), ChainError> {
        Err(ChainError::Chain(format!(
            "finalize not supported for {}",
            crate::address::display_self_dapp(token_contract)
        )))
    }
    /// Permissionless take-or-pay settlement of one crossed subscription week.

    /// The seller is credited the WHOLE weekly quota regardless of consumption -- a subscription buys
    /// reserved availability, not delivered volume. Idempotent per boundary; settling the final week closes
    /// the deal. Default: unsupported.
    async fn settle_week(&self, token_contract: &TokenContract) -> Result<(), ChainError> {
        Err(ChainError::Chain(format!(
            "settle_week not supported for {}",
            crate::address::display_self_dapp(token_contract)
        )))
    }
    /// Buyer ends the deal.

    /// This is also the buyer's remedy for an unresponsive seller: there is no inactivity gate, so a buyer
    /// facing silence stops immediately and keeps everything except the trusted consumption -- strictly
    /// better than waiting out any timeout. An ordinary deal settles by fact; a subscription pays the week
    /// already in progress in full and refunds only whole unstarted weeks. The contested tail is never
    /// paid on this path -- walking away IS the statement that it is disputed.
    async fn stop(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
    ) -> Result<Settlement, ChainError>;
    /// Observe an exact buyer-owned `StreamStopped` settlement, when the backend can read immutable
    /// contract events. Seller recovery uses this only as read-only attribution; it never submits STOP.
    async fn buyer_stop_settlement(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Option<(u128, u128)>, ChainError> {
        Ok(None)
    }
    /// Observe an exact `ProbeBurned` settlement, when the backend can read immutable contract
    /// events. `ProbeBurned` is the terminal a deal reaches when its probe was never accepted, and
    /// it is the one terminal the getters cannot describe afterwards: it settles and destroys the
    /// account, so `getState`/`getDeal` stop answering while the receipt stays queryable forever.
    /// A seller uses it only to recognise that a deal it can no longer read is already finished;
    /// it never submits anything. Returns `(burnedProbe, burnedBond, refundToBuyer)`.
    async fn probe_burned_settlement(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Option<(u128, u128, u128)>, ChainError> {
        Ok(None)
    }
    /// Observe the deal's one immutable terminal settlement after its TokenContract getters have
    /// disappeared. Real backends validate the complete current lifecycle in one read; the default
    /// preserves existing mock backends that expose only the older STOP/burn-specific readers.
    async fn terminal_settlement(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Option<DealTerminalSettlement>, ChainError> {
        let stopped = self.buyer_stop_settlement(token_contract).await?;
        let burned = self.probe_burned_settlement(token_contract).await?;
        match (stopped, burned) {
            (Some(_), Some(_)) => Err(ChainError::Chain(
                "TokenContract exposes contradictory StreamStopped and ProbeBurned terminal receipts"
                    .to_string(),
            )),
            (Some((to_seller, refund_to_buyer)), None) => {
                Ok(Some(DealTerminalSettlement::StreamStopped {
                    to_seller,
                    refund_to_buyer,
                }))
            }
            (None, Some((burned_probe, burned_bond, refund_to_buyer))) => {
                Ok(Some(DealTerminalSettlement::ProbeBurned {
                    burned_probe,
                    burned_bond,
                    refund_to_buyer,
                }))
            }
            (None, None) => Ok(None),
        }
    }
    /// The seller abandons the deal (hardware died, model pulled). Pays by FACT on every deal shape --
    /// a seller who walks out mid-week stopped reserving capacity, so take-or-pay does not apply to him.
    /// He forfeits the pending tail exactly as the buyer would, so quitting never pays better than
    /// delivering. Default: unsupported.
    async fn seller_stop(&self, token_contract: &TokenContract) -> Result<Settlement, ChainError> {
        Err(ChainError::Chain(format!(
            "seller_stop not supported for {}",
            crate::address::display_self_dapp(token_contract)
        )))
    }
    /// The buyer opens a dispute on the stream: this TC freezes
    /// the contested buyer amount and seller bond until resolution; other deals and both whole notes
    /// remain independent. Default implementation: STOP (lower bound -- scam revenue=0); backends with
    /// disputes (mock/chain) override it with the per-TC freeze.
    async fn dispute(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
    ) -> Result<Settlement, ChainError> {
        self.stop(token_contract, note).await
    }
    /// The seller **concedes the dispute**: `releaseDispute()` returns the frozen
    /// buyer amount and seller bond (on the probe -- without burn). Default: not supported (backends
    /// with disputes -- mock or chain -- override it). Symmetric to `dispute`.
    async fn release_dispute(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        Err(ChainError::Chain(format!(
            "release_dispute not supported for {}",
            crate::address::display_self_dapp(token_contract)
        )))
    }
    /// Permissionless terminal resolution after the persisted dispute window. Nobody conceded,
    /// so the same bounded dispute stake is burned on both sides and only the surviving balances
    /// are returned. Default: unsupported; the mock overrides this for offline lifecycle parity.
    async fn resolve_dispute_timeout(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        Err(ChainError::Chain(format!(
            "resolve_dispute_timeout not supported for {}",
            crate::address::display_self_dapp(token_contract)
        )))
    }
    /// Submit an automatic/inactivity-policy buyer exit only if accepted output has not advanced since the
    /// policy planned it.

    /// This guard is deliberately NOT used for an explicit operator/user STOP: a fresh output chunk must not
    /// silently veto a direct request to close the deal. It only keeps an automatic failure-policy decision
    /// honest when output resumes between that decision and the money POST. Real backends override this to
    /// place the comparison after transaction preflight and immediately before the send.
    async fn stop_if_heartbeat(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
        heartbeat: &HeartbeatGuard,
    ) -> Result<Option<Settlement>, ChainError> {
        if !heartbeat.unchanged() {
            return Ok(None);
        }
        self.stop(token_contract, note).await.map(Some)
    }
    /// The seller never opened a funded match: after MATCH_OPEN_TIMEOUT the buyer can clean up the unopened
    /// deal (`streamCleanup` -> `TC.cleanupUnopened`) and recover escrow. Default unsupported; real buyer
    /// backends override it.
    async fn cleanup_unopened(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        Err(ChainError::Chain(format!(
            "cleanup_unopened not supported for {}",
            crate::address::display_self_dapp(token_contract)
        )))
    }
    /// Read one coherent strict lifecycle/accounting view. Real backends read
    /// one complete getter set bracketed by the exact account BOC revision per
    /// bounded attempt; mock/unsupported backends have no such live view.
    async fn deal_snapshot(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Option<DealChainSnapshot>, ChainError> {
        Ok(None)
    }
    /// Read by-fact per-deal lifecycle flags and timeout anchors from the chain. Default `None` keeps mock and
    /// unsupported backends on their local/session fallback; the real chain buyer/deal backends override this so
    /// the long-running buyer monitor can derive cleanup and buyer-exit decisions from
    /// `TokenContract.getState`.
    async fn deal_state(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Option<DealChainState>, ChainError> {
        Ok(None)
    }
    /// Read the deal SHAPE (`getSubscription`): subscription term, weekly quota, weeks already settled.

    /// Required to claim correctly -- the claim ceiling of a subscription is a per-week allowance, not the
    /// whole funded volume. Default `None` keeps mock/unsupported backends on their local model.
    async fn deal_subscription(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Option<DealSubscription>, ChainError> {
        Ok(None)
    }
    /// Snapshot of locks and burned SHELL for the contract -- for e2e checks.
    async fn snapshot(&self, token_contract: &TokenContract) -> Option<StreamSnapshot>;

    /// Observability **from the note**: a snapshot of the note's state -- its own orders,
    /// deals (role + anonymous counterparty + by-fact), exposure. **Read only** (the monitor moves
    /// nothing). Default -- own offers from discovery (enumerating deals requires indexing on the
    /// backend side, so the mock overrides it with a full scan). "From whom" = the note's pubkey.
    async fn note_snapshot(&self, note: &NotePubkey) -> Result<NoteSnapshot, ChainError> {
        let note_id = note_id_hex(note);
        let offers: Vec<OfferListing> = self
            .discover_offers()
            .await?
            .into_iter()
            .filter(|o| o.seller_id == note_id)
            .collect();
        Ok(NoteSnapshot {
            note_id,
            offers,
            deals: Vec::new(),
            exposure: 0,
        })
    }

    /// Per-deal claim bounds read from the deal's on-chain `getConfig()`, for the seller-driven claim loop
    /// . Default: the canonical values (mock/fast paths and the buyer/deal backends); the real
    /// **seller** backend overrides it so a redeployed contract with different bounds cannot desync the
    /// driver from what the chain will actually accept.
    async fn deal_claim_bounds(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<ClaimBounds, ChainError> {
        Ok(ClaimBounds::canonical())
    }
}

/// Per-deal bounds on the consumption-claim loop, mirrored from `TokenContract.getConfig()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimBounds {
    /// Minimum spacing between accepted claims (`minClaimInterval`).
    pub min_claim_interval: std::time::Duration,
    /// Physical generation floor per tick (`minSecondsPerTick`); bounds the claimable delta.
    pub min_seconds_per_tick: std::time::Duration,
    /// Window after which a pending claim is promotable by anyone (`finalize`).
    pub promote_window: std::time::Duration,
    /// Buyer silence after which the seller may accept the probe. A FIXED protocol constant, deliberately
    /// absent from `getConfig()`.
    pub probe_window: std::time::Duration,
    /// Window after which an unresolved dispute is settleable by anyone (`disputeWindow`).
    pub dispute_window: std::time::Duration,
}

impl ClaimBounds {
    pub fn canonical() -> Self {
        let c = ProtocolConsts::canonical();
        Self {
            min_claim_interval: c.min_claim_interval,
            min_seconds_per_tick: c.min_seconds_per_tick,
            promote_window: c.claim_promote_window,
            probe_window: c.probe_window,
            dispute_window: c.dispute_window,
        }
    }

    /// Build from the deal's `getConfig()` result. `CLAIM_PROMOTE_WINDOW` is NOT part of that getter at
    /// either revision -- the getter answers `(platformFeeBps, minClaimInterval, minSecondsPerTick,
    /// disputeWindow)` and nothing else -- so this derivation is the ONLY source the client has, and a
    /// wrong one cannot be caught by reading the chain.

    /// Contracts 4.0.35 sets `CLAIM_PROMOTE_WINDOW = MIN_SECONDS_PER_TICK`
    /// (`contracts/airegistry/modifiers/modifiers.sol:56`), which also makes it equal to
    /// `MIN_CLAIM_INTERVAL` (`:62`). 4.0.34 defined it as TWICE the rate floor, and deriving that here
    /// now makes the client STRICTER than the chain: it withholds a `finalize()` the contract would
    /// accept, and every keeper deadline lands a full window late.

    /// Derived from the rate floor rather than pinned, so it stays correct if a deployment changes the
    /// underlying constant -- which is also why the two must be checked together on a version bump.
    pub fn from_config(
        min_claim_interval: u64,
        min_seconds_per_tick: u64,
        dispute_window: u64,
    ) -> Self {
        Self {
            min_claim_interval: std::time::Duration::from_secs(min_claim_interval),
            min_seconds_per_tick: std::time::Duration::from_secs(min_seconds_per_tick),
            promote_window: std::time::Duration::from_secs(min_seconds_per_tick),
            probe_window: crate::params::PROBE_WINDOW,
            dispute_window: std::time::Duration::from_secs(dispute_window),
        }
    }

    /// Largest cumulative increment claimable after `elapsed`, mirroring both the on-chain rate bound and
    /// the independent hard per-call `MAX_CLAIM_DELTA`.
    pub fn max_claim_delta(&self, elapsed: std::time::Duration) -> u128 {
        crate::params::claim_delta_limit(elapsed, self.min_seconds_per_tick)
    }
}

/// Anonymous note identifier used for seller blacklisting and counterparty display: hex of the ed-pubkey.
pub(crate) fn note_id_hex(pk: &NotePubkey) -> String {
    pk.ed.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note::LocalNote;
    use crate::params::{DobParams, ProtocolConsts, Shell, MATCH_OPEN_TIMEOUT_SECS};

    /// The promotion window has no getter, so this derivation is the only source the seller has, and
    /// it drifted a whole generation without anything going red -- the buyer modelled 60 s from
    /// `ProtocolConsts` while `from_config` handed the seller 120 s on the same deal.

    /// Pinned to the property rather than to the number: contracts 4.0.35 sets
    /// `CLAIM_PROMOTE_WINDOW = MIN_SECONDS_PER_TICK = MIN_CLAIM_INTERVAL`
    /// (`contracts/airegistry/modifiers/modifiers.sol:56,62`), and it is the EQUALITY that makes the
    /// next claim always arrive with the previous one ripe. A deployment that moved the rate floor
    /// would move all three together and this stays true; a client that went back to twice the floor
    /// would not, and would withhold a `finalize()` the chain accepts.
    #[test]
    fn the_derived_promote_window_is_the_rate_floor_and_the_claim_interval() {
        for floor in [60u64, 30, 90] {
            let bounds = ClaimBounds::from_config(floor, floor, 600);
            assert_eq!(
                bounds.promote_window,
                std::time::Duration::from_secs(floor),
                "CLAIM_PROMOTE_WINDOW is MIN_SECONDS_PER_TICK, not twice it"
            );
            assert_eq!(
                bounds.promote_window, bounds.min_claim_interval,
                "the window and the claim interval are one number, which is what leaves exactly one \
                 unpromoted tick"
            );
        }
        let canonical = ClaimBounds::canonical();
        assert_eq!(
            canonical.promote_window,
            ProtocolConsts::canonical().claim_promote_window,
            "the canonical bounds and the canonical constants must not disagree about the window"
        );
        assert_eq!(canonical.promote_window, canonical.min_claim_interval);
    }

    /// regression: the buyer must not resurrect the old "every shared-book ask must equal my TC"
    /// canonicality scan (it false-closed on unrelated valid sellers), but the real chain path now has a
    /// narrower target preflight because `placeInferenceBuy(modelHash,...)` cannot name a TokenContract.
    #[test]
    fn buyer_path_has_targeted_preflight_without_old_canonical_guard() {
        let source = include_str!("../chain/backends.rs");
        assert!(!source.contains("assert_matchable_asks_canonical"));
        assert!(!source.contains("check_asks_canonical"));
        assert!(source.contains("assert_expected_buy_target"));
        assert!(source.contains("placeInferenceBuy cannot target a TokenContract"));
    }

    /// Issue (track-1, negative): the book's deposit check charges 2.5 % ON TOP of the limit price, so
    /// `escrow = maxPricePerTick x ticks` (without headroom) NEVER passes. The client must reject
    /// such a configuration in advance, otherwise the SHELL will orphan in the book.
    #[test]
    fn deposit_headroom_rejects_insufficient_escrow() {
        // Original numbers from: ticks=2, maxPrice=50M, escrow=100M. Requires 2x50Mx1.025 = 102.5M.
        assert_eq!(required_escrow_for_buy(2, 50_000_000), 102_500_000);
        let err = check_buy_deposit_headroom(100_000_000, 2, 50_000_000).unwrap_err();
        assert!(err.contains("ERR_INSUFFICIENT_DEPOSIT"), "{err}");
        // Any `escrow == maxPrice x ticks` (the old bug `maxPrice = escrow/ticks`) -- always falls short.
        assert!(check_buy_deposit_headroom(2_000_000, 2, 1_000_000).is_err());
        // Exactly 1 SHELL below the minimum -- rejected (the boundary is strict, check `>=`).
        let req = required_escrow_for_buy(2, 1_000_000);
        assert!(check_buy_deposit_headroom(req - 1, 2, 1_000_000).is_err());
    }

    /// Issue + (positive/boundary): the escrow must equal EXACTLY `required` (fee-inclusive) --
    /// under-funding orphans, over-funding strands on a maker fill. Exactly-required passes.
    #[test]
    fn deposit_headroom_accepts_exactly_required() {
        assert_eq!(required_escrow_for_buy(2, 1_000_000), 2_050_000);
        assert!(check_buy_deposit_headroom(2_050_000, 2, 1_000_000).is_ok());
        let r50 = required_escrow_for_buy(2, 50_000_000);
        assert!(check_buy_deposit_headroom(r50, 2, 50_000_000).is_ok());
        let r8 = required_escrow_for_buy(8, 1_000_000);
        assert!(check_buy_deposit_headroom(r8, 8, 1_000_000).is_ok());
    }

    /// Issue (over-funding rejected): the surplus `escrow - required` is debited but NOT refunded when
    /// the buy rests and is filled as a maker (live-proven on 4.0.10) -- the client now fails-closed on it.
    #[test]
    fn deposit_headroom_rejects_over_funding() {
        let req = required_escrow_for_buy(2, 1_000_000); // 2_050_000
        assert!(check_buy_deposit_headroom(req + 1, 2, 1_000_000).is_err());
        // The exact case in: escrow=100M, 8 ticks x maxPrice 1M (required 8.2M) -> over-funded surplus.
        assert_eq!(required_escrow_for_buy(8, 1_000_000), 8_200_000);
        let err = check_buy_deposit_headroom(100_000_000, 8, 1_000_000).unwrap_err();
        assert!(err.contains(""), "{err}");
        // The old over-funded control (110M, ticks=2, maxPrice=50M; required 102.5M) is now rejected.
        assert!(check_buy_deposit_headroom(110_000_000, 2, 50_000_000).is_err());
    }

    #[test]
    fn subscription_reserve_is_exact_deposit_plus_separate_two_p_bond() {
        let price = 1_000_000_000;
        let ticks = 8;
        let reserve = subscription_buy_reserve(ticks, price).expect("checked subscription reserve");

        assert_eq!(reserve.deposit, required_escrow_for_buy(ticks, price));
        assert_eq!(reserve.deposit, 8_200_000_000);
        assert_eq!(reserve.buyer_bond, 2_000_000_000);
        assert_eq!(reserve.total_escrow, 10_200_000_000);
        assert_eq!(
            reserve.total_escrow - required_escrow_for_buy(ticks, price),
            reserve.buyer_bond,
            "ordinary BUY accounting remains bond-free"
        );
        assert!(check_subscription_buy_reserve(reserve.total_escrow, ticks, price).is_ok());
        assert!(check_subscription_buy_reserve(reserve.total_escrow - 1, ticks, price).is_err());
        assert!(check_subscription_buy_reserve(reserve.total_escrow + 1, ticks, price).is_err());
    }

    #[test]
    fn subscription_reserve_overflow_fails_closed_at_every_checked_step() {
        let largest_step = u128::MAX - (u128::MAX % crate::PRICE_STEP);
        assert!(subscription_buy_reserve(4, largest_step).is_err());
        assert!(check_subscription_buy_reserve(u128::MAX, 4, largest_step).is_err());

        let bond_overflow = u128::MAX / crate::SUBSCRIPTION_BUYER_BOND_TICKS + 1;
        assert!(subscription_buy_reserve(0, bond_overflow).is_err());

        let price = 10_000;
        let unit = required_escrow_for_buy(1, price);
        let ticks = (u128::MAX - 2 * price) / unit + 1;
        assert!(
            subscription_buy_reserve(ticks, price).is_err(),
            "deposit plus bond overflow must not wrap"
        );
    }

    #[test]
    fn subscription_lower_clearing_forwards_clearing_money_and_refunds_limit_remainder() {
        let ticks = 8;
        let limit = 2_000_000_000;
        let clearing = 1_000_000_000;
        let reserved = subscription_buy_reserve(ticks, limit).unwrap();
        let forwarded = subscription_buy_reserve(ticks, clearing).unwrap();
        let refund = subscription_buy_clearing_refund(ticks, limit, clearing).unwrap();

        assert_eq!(forwarded.deposit, 8_200_000_000);
        assert_eq!(forwarded.buyer_bond, 2_000_000_000);
        assert_eq!(refund, reserved.total_escrow - forwarded.total_escrow);
        assert_eq!(reserved.total_escrow, forwarded.total_escrow + refund);
        assert_eq!(subscription_buy_clearing_refund(ticks, limit, limit), Ok(0));
        assert!(subscription_buy_clearing_refund(ticks, clearing, limit).is_err());
    }

    fn ask(order_id: u128, tc: &str, price: u128, ticks: u128) -> OrderBookOrder {
        OrderBookOrder {
            order_id,
            owner_note: format!("0:seller{order_id}"),
            token_contract: Some(tc.to_string()),
            is_buy: false,
            price_per_tick: price,
            ticks,
            escrow: 0,
            deadline: 0,
            flags: 0,
            timestamp: 0,
        }
    }

    /// the immediate money boundary accepts only the exact row rendered to the buyer. Every
    /// matcher-relevant identity mutation remains an actionable pre-submit failure.
    #[test]
    fn pre_submit_quote_identity_rejects_real_matcher_head_changes() {
        let quoted = ask(154, "0:quoted", 1_000_000, 4);
        ensure_pre_submit_quote_unchanged(Some(&quoted), &quoted)
            .expect("an unchanged matcher head is submit-safe");

        let mut mutations = Vec::new();
        let mut changed = quoted.clone();
        changed.order_id += 1;
        mutations.push(changed);
        let mut changed = quoted.clone();
        changed.token_contract = Some("0:changed".to_string());
        mutations.push(changed);
        let mut changed = quoted.clone();
        changed.price_per_tick += 1;
        mutations.push(changed);
        let mut changed = quoted.clone();
        changed.ticks -= 1;
        mutations.push(changed);
        // the frozen row is six fields. A head that agrees on id/TC/price/ticks and moves the
        // moment the book stops honouring it, or what the funded deal will BE, is a different offer
        // than the one rendered -- and before this it was paid for without a word.
        let mut changed = quoted.clone();
        changed.deadline += 1;
        mutations.push(changed);
        let mut changed = quoted.clone();
        changed.flags |= flags::AON;
        mutations.push(changed);

        for changed in mutations {
            let error = ensure_pre_submit_quote_unchanged(Some(&quoted), &changed)
                .expect_err("a changed matcher head must fail before escrow")
                .to_string();
            assert!(
                error.contains(
                    "buyer pre-submit matcher head differs from the rendered quote; no escrow was sent"
                ),
                "{error}"
            );
        }
        assert!(ensure_pre_submit_quote_unchanged(None, &quoted)
            .expect_err("a missing durable quote must fail before escrow")
            .to_string()
            .contains("no escrow was sent"));
    }

    /// non-term metadata returned by a later non-atomic book read does not make the same
    /// executable ask stale.
    #[test]
    fn pre_submit_quote_identity_accepts_benign_reread_metadata_changes() {
        let quoted = ask(
            489,
            "0:03d8b19ead1b4efce30066813b244de7d92e07ea87cc20f8e0ec9c4ebf552cfb",
            1,
            2,
        );
        let mut reread = quoted.clone();
        reread.timestamp = 456;

        assert_ne!(
            quoted, reread,
            "the old whole-OrderBookOrder comparison must observe the  benign reread difference"
        );
        ensure_pre_submit_quote_unchanged(Some(&quoted), &reread)
            .expect("the same order identity and executable terms remain submit-safe");
    }

    /// quote consumes the book in price/time order and includes the 2.5% book fee in totals.
    #[test]
    fn executable_quote_uses_price_time_depth_and_fee() {
        let q = executable_quote(
            &[ask(2, "0:expensive", 1200, 10), ask(1, "0:cheap", 1000, 2)],
            Some(5),
            None,
        )
        .unwrap();
        assert!(q.complete);
        assert_eq!(q.filled_ticks, 5);
        assert_eq!(q.fills[0].order_id, 1);
        assert_eq!(q.fills[0].ticks, 2);
        assert_eq!(q.fills[0].cost_with_fee, required_escrow_for_buy(2, 1000));
        assert_eq!(q.fills[1].order_id, 2);
        assert_eq!(q.fills[1].ticks, 3);
        assert_eq!(
            q.total_with_fee,
            required_escrow_for_buy(2, 1000) + required_escrow_for_buy(3, 1200)
        );
    }

    /// equivalent duplicate active asks for one TokenContract coalesce to one deterministic candidate.
    #[test]
    fn executable_quote_coalesces_equivalent_duplicate_active_tc_asks() {
        let mut later_dup = ask(2, "0:DUP", 900, 4);
        later_dup.owner_note = "0:seller1".to_string();
        let q = executable_quote(
            &[
                later_dup,
                ask(1, "0:dup", 900, 4),
                ask(3, "0:later", 1000, 4),
            ],
            Some(4),
            None,
        )
        .unwrap();
        assert!(q.complete);
        assert_eq!(q.filled_ticks, 4);
        assert_eq!(q.fills.len(), 1);
        assert_eq!(q.fills[0].order_id, 1);
        assert_eq!(q.fills[0].token_contract, "0:dup");
        assert_eq!(q.total_with_fee, required_escrow_for_buy(4, 900));
    }

    /// negative: duplicate active asks with conflicting terms/state remain ambiguous and fail closed.
    #[test]
    fn executable_quote_rejects_conflicting_duplicate_active_tc_asks() {
        let err = executable_quote(
            &[ask(2, "0:DUP", 900, 1), ask(1, "0:dup", 1000, 1)],
            Some(2),
            None,
        )
        .unwrap_err();
        assert!(err.contains("conflicting terms/state"), "{err}");
        assert!(err.contains("0:dup"), "{err}");
        assert!(
            err.contains("order_ids [1,2]") || err.contains("order_ids [2,1]"),
            "{err}"
        );
    }

    /// negative: a fill event is not enough. The matched TC must read funded=true before the buyer
    /// waits for handover.
    #[test]
    fn reported_match_with_unfunded_tc_fails_before_handover() {
        let err = check_matched_token_contract_state(
            "0:tc",
            DealChainState {
                funded: false,
                opened: false,
                probe_accepted: true,
                disputed: false,
                deposit: 1_000,
                finalized_owed: 0,
                tokens_final: 0,
                tokens_pending: 0,
                funded_time: None,
                probe_tick: 0,
                probe_time: 0,
                last_claim_time: 0,
                dispute_time: 0,
            },
            1000,
            MATCH_OPEN_TIMEOUT_SECS,
        )
        .unwrap_err();
        assert!(err.contains("not funded after the fill event"), "{err}");
        assert!(err.contains("refusing to wait for handover"), "{err}");
    }

    fn matched_never_opened_state() -> DealChainState {
        DealChainState {
            funded: true,
            opened: false,
            probe_accepted: false,
            disputed: false,
            deposit: 1_000,
            finalized_owed: 0,
            tokens_final: 0,
            tokens_pending: 0,
            funded_time: Some(100),
            probe_tick: 0,
            probe_time: 0,
            last_claim_time: 100,
            dispute_time: 0,
        }
    }

    /// funded-but-never-opened is recognized as cleanup-eligible only after MATCH_OPEN_TIMEOUT.
    #[test]
    fn funded_never_opened_cleanup_readiness_is_timeout_gated() {
        let early = check_matched_token_contract_state(
            "0:tc",
            matched_never_opened_state(),
            699,
            MATCH_OPEN_TIMEOUT_SECS,
        )
        .unwrap();
        assert_eq!(
            early,
            MatchedTokenContractStatus::FundedNeverOpened {
                funded_time: Some(100),
                cleanup_after_unix: Some(700),
                cleanup_ready: false,
                remaining_secs: Some(1),
            }
        );

        let ready = check_matched_token_contract_state(
            "0:tc",
            matched_never_opened_state(),
            700,
            MATCH_OPEN_TIMEOUT_SECS,
        )
        .unwrap();
        assert_eq!(
            ready,
            MatchedTokenContractStatus::FundedNeverOpened {
                funded_time: Some(100),
                cleanup_after_unix: Some(700),
                cleanup_ready: true,
                remaining_secs: Some(0),
            }
        );
    }

    #[test]
    fn matched_classifier_rejects_every_mutated_never_opened_shape() {
        type StateMutation = (&'static str, fn(&mut DealChainState));

        let valid = matched_never_opened_state();
        let malformed = "not the authoritative funded-never-opened shape";

        let mut disputed = valid;
        disputed.disputed = true;
        let error =
            check_matched_token_contract_state("0:tc", disputed, 700, MATCH_OPEN_TIMEOUT_SECS)
                .unwrap_err();
        assert!(error.contains("disputed immediately"), "{error}");

        let mutations: &[StateMutation] = &[
            ("accepted probe", |state| state.probe_accepted = true),
            ("drained deposit", |state| state.deposit = 0),
            ("probe money", |state| state.probe_tick = 1),
            ("seller money", |state| state.finalized_owed = 1),
            ("final tokens", |state| state.tokens_final = 1),
            ("pending tokens", |state| state.tokens_pending = 1),
            ("probe time", |state| state.probe_time = 1),
            ("last claim time", |state| state.last_claim_time = 101),
            ("missing funded time", |state| state.funded_time = None),
            ("zero funded time", |state| {
                state.funded_time = Some(0);
                state.last_claim_time = 0;
            }),
            ("mismatched funded time", |state| {
                state.funded_time = Some(101)
            }),
            ("stale dispute time", |state| state.dispute_time = 1),
        ];
        for (name, mutate) in mutations {
            let mut state = valid;
            mutate(&mut state);
            let error =
                check_matched_token_contract_state("0:tc", state, 700, MATCH_OPEN_TIMEOUT_SECS)
                    .unwrap_err();
            assert!(error.contains(malformed), "{name}: {error}");
        }
    }

    /// negative: insufficient depth returns an incomplete quote instead of inventing liquidity.
    #[test]
    fn executable_quote_reports_incomplete_depth() {
        let q = executable_quote(&[ask(1, "0:one", 1000, 2)], Some(3), None).unwrap();
        assert!(!q.complete);
        assert_eq!(q.filled_ticks, 2);
    }

    /// budget mode: the executable tick count is bounded by fee-inclusive unit cost.
    #[test]
    fn executable_quote_budget_mode_respects_fee_inclusive_budget() {
        let q = executable_quote(&[ask(1, "0:one", 1000, 10)], None, Some(3075)).unwrap();
        assert!(q.complete);
        assert_eq!(q.filled_ticks, 3);
        assert_eq!(q.total_with_fee, required_escrow_for_buy(3, 1000));
    }

    #[test]
    fn submit_safe_single_ask_quote_accepts_partial_head_fill() {
        let q =
            submit_safe_single_ask_quote(&[ask(1, "0:one", 1000, 1024)], Some(1), None).unwrap();
        assert!(q.complete);
        assert_eq!(q.filled_ticks, 1);
        assert_eq!(q.total_with_fee, required_escrow_for_buy(1, 1000));
        assert_eq!(q.fills.len(), 1);
        assert_eq!(q.fills[0].order_id, 1);
        assert_eq!(q.fills[0].ticks, 1);
    }

    #[test]
    fn submit_safe_single_ask_quote_accepts_exact_whole_head_ask() {
        let q = submit_safe_single_ask_quote(
            &[ask(1, "0:one", 900, 1024), ask(2, "0:two", 1000, 1)],
            Some(1024),
            None,
        )
        .unwrap();
        assert!(q.complete);
        assert_eq!(q.filled_ticks, 1024);
        assert_eq!(q.fills.len(), 1);
        assert_eq!(q.fills[0].order_id, 1);
        assert_eq!(q.fills[0].token_contract, "0:one");
    }

    #[test]
    fn submit_safe_single_ask_quote_uses_crossing_depth_without_exact_head_size() {
        let q = submit_safe_single_ask_quote(
            &[ask(1, "0:small", 900, 1), ask(2, "0:exact", 1000, 1024)],
            Some(1024),
            None,
        )
        .unwrap();
        assert!(q.complete);
        assert_eq!(q.filled_ticks, 1024);
        assert_eq!(q.fills.len(), 2);
        assert_eq!(q.fills[0].order_id, 1);
        assert_eq!(q.fills[0].ticks, 1);
        assert_eq!(q.fills[1].order_id, 2);
        assert_eq!(q.fills[1].ticks, 1023);
    }

    #[test]
    fn submit_safe_single_ask_quote_reports_fok_incomplete_depth() {
        let q = submit_safe_single_ask_quote(&[ask(1, "0:small", 900, 1)], Some(2), None).unwrap();
        assert!(!q.complete);
        assert_eq!(q.filled_ticks, 1);
        assert_eq!(q.fills.len(), 1);
    }

    #[test]
    fn submit_safe_single_ask_quote_budget_mode_uses_head_ask_only() {
        let q =
            submit_safe_single_ask_quote(&[ask(1, "0:one", 1000, 4)], None, Some(4099)).unwrap();
        assert!(q.complete);
        assert_eq!(q.filled_ticks, 3);
        assert_eq!(q.total_with_fee, required_escrow_for_buy(3, 1000));

        let q =
            submit_safe_single_ask_quote(&[ask(1, "0:one", 1000, 4)], None, Some(4100)).unwrap();
        assert!(q.complete);
        assert_eq!(q.filled_ticks, 4);
        assert_eq!(q.total_with_fee, required_escrow_for_buy(4, 1000));
    }

    /// Issue (track-1, fail-closed): absurdly large `ticks`/`maxPricePerTick` must NOT panic
    /// (debug) and must NOT wrap (release) -- `required` saturates to `u128::MAX`, and the guard rejects.
    /// On the old code (`p * FEE_BPS` without `saturating_mul`) this test would have panicked on overflow.
    #[test]
    fn deposit_headroom_fails_closed_on_overflow() {
        // Overflowing inputs saturate (the wrapper) instead of panicking/wrapping.
        assert_eq!(required_escrow_for_buy(u128::MAX, u128::MAX), u128::MAX);
        assert_eq!(required_escrow_for_buy(1, u128::MAX), u128::MAX); // checked pxFEE_BPS overflows -> saturates
                                                                      // Any real escrow < the saturated minimum -> reject (fail-closed, without a panic).
        assert!(check_buy_deposit_headroom(u128::MAX - 1, u128::MAX, u128::MAX).is_err());
        // review: a SATURATED `required` must reject even when `escrow == required == u128::MAX`
        // (the exact-equality upper bound alone would otherwise let this absurd config slip through).
        assert!(check_buy_deposit_headroom(u128::MAX, u128::MAX, u128::MAX).is_err());
        assert!(check_buy_deposit_headroom(1_000_000_000, u128::MAX, 1_000_000).is_err());
        assert!(check_buy_deposit_headroom(0, 1, u128::MAX).is_err());
        // re-review: the INTERMEDIATE `p x FEE_BPS` fee product can overflow and then be divided
        // (`/ 10000`) back BELOW u128::MAX -- a truncated value a final `required == u128::MAX` check alone
        // would miss, accepting `escrow == required` (the garbage). The guard now rejects ANY arithmetic
        // overflow via the checked helper. p = u128::MAX/100 -> px250 overflows in the fee product.
        let p_fee_overflow = u128::MAX / 100;
        assert!(check_buy_deposit_headroom(
            required_escrow_for_buy(1, p_fee_overflow),
            1,
            p_fee_overflow
        )
        .is_err());
        assert!(check_buy_deposit_headroom(0, 1, p_fee_overflow).is_err());
        // A large but NON-overflowing value -- exact computation without saturation (= the contract's).
        assert_eq!(
            required_escrow_for_buy(1, 1_000_000_000_000),
            1_025_000_000_000
        );
    }

    /// (R11): `note_snapshot` shows the note's offers, its deals and the **anonymous**
    /// counterparty (the note's pubkey, not an identity); another note sees nothing.
    #[tokio::test]
    async fn note_snapshot_shows_offers_deals_and_anon_counterparty() {
        let base_dir = tempfile::tempdir().expect("test temp dir");
        let base = base_dir.path();
        let chain = MockChainBackend::new(
            base.join("eps.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let seller = LocalNote::from_seed(&[1u8; 32]);
        let buyer = LocalNote::from_seed(&[2u8; 32]);
        let tc = "tc-snap".to_string();
        chain
            .post_offer(
                SellOffer {
                    price_per_tick: u64::try_from(crate::PRICE_STEP).unwrap(),
                    max_ticks: 8,
                    token_contract: tc.clone(),
                    flags: 0,
                },
                &seller,
            )
            .await
            .unwrap();
        chain.place_buy(&tc, &buyer).await.unwrap();

        let s = chain.note_snapshot(&seller.pubkey()).await.unwrap();
        assert!(
            s.offers.is_empty(),
            "the filled offer is no longer an active book ask"
        );
        assert_eq!(s.deals.len(), 1, "the seller sees the deal");
        assert_eq!(s.deals[0].role, DealRole::Seller);
        assert_eq!(
            s.deals[0].counterparty.as_deref(),
            Some(note_id_hex(&buyer.pubkey()).as_str()),
            "the seller's counterparty = the buyer's anonymous pubkey ()"
        );

        let b = chain.note_snapshot(&buyer.pubkey()).await.unwrap();
        assert!(b.offers.is_empty(), "the buyer has no offers of its own");
        assert_eq!(b.deals.len(), 1);
        assert_eq!(b.deals[0].role, DealRole::Buyer);
        assert_eq!(
            b.deals[0].counterparty.as_deref(),
            Some(note_id_hex(&seller.pubkey()).as_str()),
            "the buyer's counterparty = the seller's anonymous pubkey ()"
        );

        let stranger = LocalNote::from_seed(&[9u8; 32]);
        let n = chain.note_snapshot(&stranger.pubkey()).await.unwrap();
        assert!(
            n.offers.is_empty() && n.deals.is_empty(),
            "another note sees nothing"
        );
    }

    // ---- issue: per-model by-fact accounting breakdown (pure) ----

    fn snap(
        received: Shell,
        seller_locked: Shell,
        buyer_locked: Shell,
        burned: Shell,
    ) -> StreamSnapshot {
        StreamSnapshot {
            seller_locked: u128::from(seller_locked),
            buyer_locked: u128::from(buyer_locked),
            buyer_lead: u128::from(buyer_locked), // test helper: treat the lock as the at-risk lead (the two-tick tests)
            tokens_final: 0,
            seller_received: u128::from(received),
            buyer_refunded: 0,
            burned: u128::from(burned),
            closed: false,
        }
    }

    fn snap_with_ticks(
        received: Shell,
        finalized_ticks: u128,
        seller_locked: Shell,
        buyer_locked: Shell,
        burned: Shell,
    ) -> StreamSnapshot {
        let mut snapshot = snap(received, seller_locked, buyer_locked, burned);
        snapshot.tokens_final = finalized_ticks * crate::params::TICK_SIZE;
        snapshot
    }

    fn deal(
        role: DealRole,
        model: Option<&str>,
        cp: Option<&str>,
        price: Shell,
        snapshot: Option<StreamSnapshot>,
    ) -> DealView {
        DealView {
            token_contract: "0:tc".to_string(),
            role,
            counterparty: cp.map(|s| s.to_string()),
            price_per_tick: price,
            model: model.map(|s| s.to_string()),
            snapshot,
        }
    }

    /// The seller view groups deals by served model, then by anonymous counterparty, summing the by-fact
    /// figures; the per-model roll-up is the sum across its counterparties.
    #[test]
    fn breakdown_groups_by_model_then_counterparty_with_rollup() {
        let deals = vec![
            deal(
                DealRole::Seller,
                Some("qwen"),
                Some("aa"),
                100,
                Some(snap_with_ticks(500, 5, 200, 0, 50)),
            ),
            deal(
                DealRole::Seller,
                Some("qwen"),
                Some("bb"),
                100,
                Some(snap_with_ticks(300, 3, 100, 0, 30)),
            ),
            deal(
                DealRole::Seller,
                Some("llama"),
                Some("cc"),
                100,
                Some(snap_with_ticks(200, 2, 0, 0, 20)),
            ),
        ];
        let b = per_model_breakdown(&deals, DealRole::Seller);
        assert_eq!(b.len(), 2, "two model buckets");
        let qwen = &b[0];
        assert_eq!(qwen.model, "qwen");
        assert_eq!(qwen.tokens, 8, "5 + 3 finalized ticks");
        assert_eq!(qwen.money, 800);
        assert_eq!(qwen.locked, 300);
        assert_eq!(qwen.burned, 80);
        assert_eq!(qwen.counterparties.len(), 2, "aa + bb");
        assert_eq!(qwen.counterparties[0].counterparty.as_deref(), Some("aa"));
        assert_eq!(qwen.counterparties[0].tokens, 5);
        assert_eq!(qwen.counterparties[0].money, 500);
        assert_eq!(qwen.counterparties[1].counterparty.as_deref(), Some("bb"));
        assert_eq!(qwen.counterparties[1].tokens, 3);
        let llama = &b[1];
        assert_eq!(llama.model, "llama");
        assert_eq!(llama.tokens, 2);
        assert_eq!(llama.money, 200);
    }

    /// `locked` is role-specific: the seller view shows `seller_locked`, the buyer view shows `buyer_locked`,
    /// from the SAME deal snapshot. Money comes from `seller_received`; volume comes from `tokens_final`.
    #[test]
    fn breakdown_locked_is_role_specific() {
        let s = snap_with_ticks(400, 4, 200, 700, 10);
        let deals = vec![
            deal(DealRole::Seller, Some("m"), Some("x"), 100, Some(s.clone())),
            deal(DealRole::Buyer, Some("m"), Some("y"), 100, Some(s)),
        ];
        let seller = per_model_breakdown(&deals, DealRole::Seller);
        assert_eq!(seller.len(), 1);
        assert_eq!(seller[0].locked, 200, "seller sees seller_locked");
        assert_eq!(seller[0].money, 400);
        let buyer = per_model_breakdown(&deals, DealRole::Buyer);
        assert_eq!(buyer.len(), 1);
        assert_eq!(buyer[0].locked, 700, "buyer sees buyer_locked");
        assert_eq!(buyer[0].tokens, 4, "buyer's spent ticks = settled ticks");
    }

    /// A returned seller bond changes money but cannot invent delivered volume when `tokens_final` is zero.
    #[test]
    fn breakdown_returned_two_p_bond_with_zero_tokens_reports_zero_volume() {
        let deals = vec![deal(
            DealRole::Seller,
            Some("m"),
            Some("x"),
            100,
            Some(snap(200, 0, 0, 0)),
        )];
        let breakdown = per_model_breakdown(&deals, DealRole::Seller);
        assert_eq!(breakdown[0].money, 200, "the returned 2P remains money");
        assert_eq!(
            breakdown[0].tokens, 0,
            "zero authoritative tokens cannot become ticks"
        );
    }

    /// Withdrawing accrued money may reduce `finalized_owed`, but the immutable token counter preserves volume.
    #[test]
    fn breakdown_withdrawal_cannot_reduce_authoritative_token_volume() {
        let before = vec![deal(
            DealRole::Seller,
            Some("m"),
            Some("x"),
            100,
            Some(snap_with_ticks(200, 2, 0, 0, 0)),
        )];
        let after = vec![deal(
            DealRole::Seller,
            Some("m"),
            Some("x"),
            100,
            Some(snap_with_ticks(0, 2, 0, 0, 0)),
        )];
        let before = per_model_breakdown(&before, DealRole::Seller);
        let after = per_model_breakdown(&after, DealRole::Seller);
        assert_eq!(before[0].tokens, 2);
        assert_eq!(after[0].tokens, 2);
        assert_eq!(before[0].money, 200);
        assert_eq!(after[0].money, 0);
    }

    /// A deal whose model the source cannot name buckets under `(unknown)` (the mock book has no per-deal
    /// model); the bucket still aggregates correctly.
    #[test]
    fn breakdown_unknown_model_bucket() {
        let deals = vec![deal(
            DealRole::Seller,
            None,
            Some("x"),
            100,
            Some(snap_with_ticks(200, 2, 0, 0, 0)),
        )];
        let b = per_model_breakdown(&deals, DealRole::Seller);
        assert_eq!(b[0].model, UNKNOWN_MODEL);
        assert_eq!(b[0].tokens, 2);
    }

    /// The view is per role: a seller-role query never includes buyer-role deals (and vice versa).
    #[test]
    fn breakdown_skips_the_other_role() {
        let deals = vec![
            deal(
                DealRole::Seller,
                Some("s-model"),
                Some("x"),
                100,
                Some(snap(100, 0, 0, 0)),
            ),
            deal(
                DealRole::Buyer,
                Some("b-model"),
                Some("y"),
                100,
                Some(snap(100, 0, 0, 0)),
            ),
        ];
        let seller = per_model_breakdown(&deals, DealRole::Seller);
        assert_eq!(seller.len(), 1);
        assert_eq!(seller[0].model, "s-model");
    }

    /// Visibility of anomalies: a deal that locked SHELL but finalized nothing (`received=0`)
    /// still appears -- non-zero `locked`, zero `tokens`/`money` -- so a lock-without-delivery is not hidden.
    /// A deal with no stream snapshot at all also appears with all-zero figures.
    #[test]
    fn breakdown_shows_lock_without_delivery_and_no_snapshot() {
        let deals = vec![
            deal(
                DealRole::Seller,
                Some("m"),
                Some("x"),
                100,
                Some(snap(0, 250, 0, 0)),
            ),
            deal(DealRole::Seller, Some("m"), Some("z"), 100, None),
        ];
        let b = per_model_breakdown(&deals, DealRole::Seller);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].counterparties.len(), 2, "both deals visible");
        assert_eq!(b[0].locked, 250, "the lock-without-delivery is surfaced");
        assert_eq!(b[0].money, 0);
        assert_eq!(b[0].tokens, 0);
    }

    /// Multiple deals with the SAME model and SAME counterparty collapse into one counterparty tally that
    /// sums them -- the per-counterparty roll-up the accounting view needs.
    #[test]
    fn breakdown_sums_repeated_counterparty() {
        let deals = vec![
            deal(
                DealRole::Seller,
                Some("m"),
                Some("x"),
                100,
                Some(snap_with_ticks(200, 2, 50, 0, 10)),
            ),
            deal(
                DealRole::Seller,
                Some("m"),
                Some("x"),
                100,
                Some(snap_with_ticks(300, 3, 70, 0, 20)),
            ),
        ];
        let b = per_model_breakdown(&deals, DealRole::Seller);
        assert_eq!(b[0].counterparties.len(), 1, "same counterparty collapses");
        let c = &b[0].counterparties[0];
        assert_eq!(c.tokens, 5);
        assert_eq!(c.money, 500);
        assert_eq!(c.locked, 120);
        assert_eq!(c.burned, 30);
    }

    // ---- issue: by-fact anomaly surfacing (pure) ----

    /// An orphaned lock: SHELL frozen with NO matched counterparty -> `LockedNoMatch`.
    #[test]
    fn anomalies_flag_orphaned_lock_no_match() {
        let d = deal(
            DealRole::Seller,
            Some("m"),
            None,
            100,
            Some(snap(0, 0, 150, 0)),
        );
        assert_eq!(
            deal_anomalies(&d),
            vec![DealAnomaly::LockedNoMatch { locked: 150 }]
        );
    }

    /// A lock that survived a STOP: the deal is closed but SHELL is still locked ->
    /// `LockedAfterClose`.
    #[test]
    fn anomalies_flag_lock_surviving_close() {
        let mut s = snap(500, 0, 100, 0);
        s.closed = true;
        let d = deal(DealRole::Seller, Some("m"), Some("cp"), 100, Some(s));
        assert_eq!(
            deal_anomalies(&d),
            vec![DealAnomaly::LockedAfterClose { locked: 100 }]
        );
    }

    /// The two-tick invariant: the ceiling is `2 x _unit(price)` and `_unit` **includes the book
    /// fee** (`p + pxFEE_BPS/10000`). A legitimate two-tick lock -- which the buyer escrows WITH the fee -- is
    /// NOT an anomaly; the bug was a fee-less `2 x p` ceiling that false-flagged every real two-tick deal.
    #[test]
    fn anomalies_flag_buyer_lock_over_two_ticks_fee_inclusive() {
        // The repro: price 10000 -> by-fact 2-tick lock = 2 x (10000 + 10000x250/10000) = 2 x 10250 = 20500.
        // The old fee-less ceiling (20000) false-flagged this legitimate lock; the fee-inclusive ceiling (20500)
        // does not.
        let legit = deal(
            DealRole::Buyer,
            Some("m"),
            Some("cp"),
            10_000,
            Some(snap(0, 0, 20_500, 0)),
        );
        assert!(
            deal_anomalies(&legit).is_empty(),
            "a legitimate two-tick lock (book fee included) is not an anomaly"
        );
        // One SHELL above the fee-inclusive two-tick ceiling -> flagged (a real over-lock).
        let over = deal(
            DealRole::Buyer,
            Some("m"),
            Some("cp"),
            10_000,
            Some(snap(0, 0, 20_501, 0)),
        );
        assert_eq!(
            deal_anomalies(&over),
            vec![DealAnomaly::BuyerLockExceedsTwoTicks {
                buyer_lead: 20_501,
                ceiling: 20_500
            }]
        );
    }

    /// regression: the two-tick check bounds the at-risk LEAD (`prepaid + frozen`), NOT the total
    /// `buyer_locked` (which carries the unspent deposit for the remaining ticks). A legitimate 8-tick deal
    /// locks `8 x _unit(1000) = 8200` total but keeps its lead within 2 ticks -- it must NOT false-flag; only
    /// an oversized lead does.
    #[test]
    fn two_tick_bounds_lead_not_total_lock() {
        let snap_lead = |buyer_locked: Shell, buyer_lead: Shell| StreamSnapshot {
            seller_locked: 0,
            buyer_locked: u128::from(buyer_locked),
            buyer_lead: u128::from(buyer_lead),
            tokens_final: 0,
            seller_received: 0,
            buyer_refunded: 0,
            burned: 0,
            closed: false,
        };
        // 8-tick total lock 8200, lead within the 2-tick ceiling (2050) -> NOT flagged.
        let ok = deal(
            DealRole::Buyer,
            Some("m"),
            Some("cp"),
            1000,
            Some(snap_lead(8200, 2050)),
        );
        assert!(
            deal_anomalies(&ok).is_empty(),
            "an 8-tick total lock with a <=2-tick lead is not an anomaly ()"
        );
        // Same total, but a lead one SHELL over the 2-tick ceiling -> flagged on the LEAD.
        let bad = deal(
            DealRole::Buyer,
            Some("m"),
            Some("cp"),
            1000,
            Some(snap_lead(8200, 2051)),
        );
        assert_eq!(
            deal_anomalies(&bad),
            vec![DealAnomaly::BuyerLockExceedsTwoTicks {
                buyer_lead: 2051,
                ceiling: 2050
            }]
        );
    }

    /// A clean matched/open deal and a deal with no stream snapshot both flag nothing.
    #[test]
    fn anomalies_clean_or_no_snapshot_deal_has_none() {
        let clean = deal(
            DealRole::Seller,
            Some("m"),
            Some("cp"),
            100,
            Some(snap(500, 0, 100, 0)),
        );
        assert!(deal_anomalies(&clean).is_empty());
        let no_snap = deal(DealRole::Seller, Some("m"), Some("cp"), 100, None);
        assert!(deal_anomalies(&no_snap).is_empty());
    }

    /// A zero price skips the two-tick check (no division/ceiling) rather than panicking or false-flagging.
    #[test]
    fn anomalies_price_zero_skips_two_tick_check() {
        let d = deal(
            DealRole::Seller,
            Some("m"),
            Some("cp"),
            0,
            Some(snap(0, 0, 5000, 0)),
        );
        assert!(
            deal_anomalies(&d).is_empty(),
            "no two-tick ceiling when price is zero"
        );
    }
}

#[cfg(test)]
mod recover_tests {
    use super::check_recoverable;

    /// an OPEN, undisputed deal whose recorded buyer matches the recover note -> recoverable.
    #[test]
    fn recoverable_ok_on_open_owned() {
        let me = [7u8; 32];
        assert!(check_recoverable(true, false, Some("0:buyer"), "0:buyer", Some(&me), &me).is_ok());
    }

    /// negatives -- each precondition fails closed with an actionable message, BEFORE any on-chain
    /// STOP: not-OPEN, disputed, a foreign note (not the deal's buyer), and an unmatched deal (no buyer).
    #[test]
    fn recoverable_fails_closed_on_each_precondition() {
        let me = [7u8; 32];
        let other = [9u8; 32];
        assert!(
            check_recoverable(false, false, Some("0:buyer"), "0:buyer", Some(&me), &me)
                .unwrap_err()
                .contains("not OPEN")
        );
        assert!(
            check_recoverable(true, true, Some("0:buyer"), "0:buyer", Some(&me), &me)
                .unwrap_err()
                .contains("DISPUTED")
        );
        assert!(
            check_recoverable(true, false, Some("0:other"), "0:buyer", Some(&me), &me)
                .unwrap_err()
                .contains("not the deal's buyer note")
        );
        assert!(
            check_recoverable(true, false, Some("0:buyer"), "0:buyer", Some(&other), &me)
                .unwrap_err()
                .contains("not the deal's buyer key")
        );
        assert!(
            check_recoverable(true, false, None, "0:buyer", Some(&me), &me)
                .unwrap_err()
                .contains("no recorded buyer note")
        );
        assert!(
            check_recoverable(true, false, Some("0:buyer"), "0:buyer", None, &me)
                .unwrap_err()
                .contains("no recorded buyer")
        );
    }
}

#[cfg(test)]
mod dispute_reclaim_tests {
    use super::{
        check_disputable, check_reclaimable, check_release_disputable, check_seller_pubkey,
        check_withdrawable_shell, DealChainState,
    };
    use crate::params::MATCH_OPEN_TIMEOUT_SECS;

    /// -- `check_disputable`: an OPEN, undisputed deal owned by THIS buyer is disputable; each
    /// precondition fails closed BEFORE any on-chain `streamDispute`.
    #[test]
    fn disputable_gates() {
        let me = [7u8; 32];
        let other = [9u8; 32];
        assert!(check_disputable(true, false, Some("0:buyer"), "0:buyer", Some(&me), &me).is_ok());
        assert!(
            check_disputable(false, false, Some("0:buyer"), "0:buyer", Some(&me), &me)
                .unwrap_err()
                .contains("not OPEN")
        );
        assert!(
            check_disputable(true, true, Some("0:buyer"), "0:buyer", Some(&me), &me)
                .unwrap_err()
                .contains("ALREADY disputed")
        );
        assert!(
            check_disputable(true, false, Some("0:other"), "0:buyer", Some(&me), &me)
                .unwrap_err()
                .contains("not the deal's buyer note")
        );
        assert!(
            check_disputable(true, false, Some("0:buyer"), "0:buyer", Some(&other), &me)
                .unwrap_err()
                .contains("not the deal's buyer key")
        );
    }

    fn never_opened_state() -> DealChainState {
        DealChainState {
            funded: true,
            opened: false,
            probe_accepted: false,
            disputed: false,
            deposit: 2_000,
            finalized_owed: 0,
            tokens_final: 0,
            tokens_pending: 0,
            probe_tick: 0,
            funded_time: Some(500),
            probe_time: 0,
            last_claim_time: 500,
            dispute_time: 0,
        }
    }

    fn owned_reclaim(state: DealChainState, now: u64) -> Result<(), String> {
        let me = [7u8; 32];
        check_reclaimable(
            state,
            Some("0:buyer"),
            "0:buyer",
            Some(&me),
            &me,
            now,
            MATCH_OPEN_TIMEOUT_SECS,
        )
    }

    /// Reclaim is only the exact never-opened cleanup. Explicit buyer STOP is tested separately through
    /// `check_recoverable` and is never rewritten from this legacy command name.
    #[test]
    fn reclaimable_gates() {
        let me = [7u8; 32];
        let other = [9u8; 32];
        let mut opened = never_opened_state();
        opened.opened = true;
        opened.probe_time = 501;
        opened.last_claim_time = 501;
        let opened_reclaim = owned_reclaim(opened, 500);
        assert_eq!(
            usize::from(opened_reclaim.is_ok()),
            0,
            "OPEN reclaim POST count"
        );
        assert!(opened_reclaim
            .unwrap_err()
            .contains("explicit `dexdo close`"));

        let never_opened = never_opened_state();
        assert!(owned_reclaim(never_opened, 1_099)
            .unwrap_err()
            .contains("MATCH_OPEN_TIMEOUT"));
        owned_reclaim(never_opened, 1_100)
            .expect("the exact never-opened shape past MATCH_OPEN_TIMEOUT is admissible");

        let mut not_funded = never_opened;
        not_funded.funded = false;
        assert!(owned_reclaim(not_funded, 9_999)
            .unwrap_err()
            .contains("not funded"));

        let mut disputed = opened;
        disputed.disputed = true;
        assert!(owned_reclaim(disputed, 9_999)
            .unwrap_err()
            .contains("DISPUTED"));

        assert!(check_reclaimable(
            opened,
            Some("0:other"),
            "0:buyer",
            Some(&me),
            &me,
            9_999,
            MATCH_OPEN_TIMEOUT_SECS,
        )
        .unwrap_err()
        .contains("not the deal's buyer note"));
        assert!(check_reclaimable(
            opened,
            Some("0:buyer"),
            "0:buyer",
            Some(&other),
            &me,
            9_999,
            MATCH_OPEN_TIMEOUT_SECS,
        )
        .unwrap_err()
        .contains("not the deal's buyer key"));
        assert!(check_reclaimable(
            opened,
            None,
            "0:buyer",
            Some(&me),
            &me,
            9_999,
            MATCH_OPEN_TIMEOUT_SECS,
        )
        .unwrap_err()
        .contains("no recorded buyer note"));
    }

    #[test]
    fn cleanup_rejects_terminal_and_every_mutated_never_opened_shape() {
        let valid = never_opened_state();
        let mut cases = Vec::new();

        let mut state = valid;
        state.opened = true;
        cases.push(("opened deal", state, "dexdo close"));

        let mut state = valid;
        state.deposit = 0;
        cases.push(("terminal drained", state, "terminal/drained"));

        let mut state = valid;
        state.probe_accepted = true;
        cases.push(("accepted probe", state, "never-opened shape"));

        let mut state = valid;
        state.probe_tick = 1;
        cases.push(("probe money", state, "never-opened shape"));

        let mut state = valid;
        state.finalized_owed = 1;
        cases.push(("seller money", state, "never-opened shape"));

        let mut state = valid;
        state.tokens_final = 1;
        cases.push(("final tokens", state, "never-opened shape"));

        let mut state = valid;
        state.tokens_pending = 1;
        cases.push(("pending tokens", state, "never-opened shape"));

        let mut state = valid;
        state.probe_time = 501;
        cases.push(("probe time", state, "never-opened shape"));

        let mut state = valid;
        state.last_claim_time = 501;
        cases.push(("last claim time", state, "never-opened shape"));

        let mut state = valid;
        state.dispute_time = 501;
        cases.push(("stale dispute time", state, "never-opened shape"));

        let mut state = valid;
        state.funded_time = None;
        cases.push(("missing funded time", state, "fundedTime"));

        let mut state = valid;
        state.funded_time = Some(0);
        state.last_claim_time = 0;
        cases.push(("zero funded time", state, "fundedTime"));

        for (name, state, expected) in cases {
            let result = owned_reclaim(state, 1_100);
            assert_eq!(usize::from(result.is_ok()), 0, "{name} POST count");
            assert!(
                result.unwrap_err().contains(expected),
                "{name} must fail with {expected}"
            );
        }
    }

    /// seller-side dispute/payout commands fail closed before on-chain submission where state/key checks
    /// already prove the call would revert.
    #[test]
    fn seller_release_and_withdraw_gates() {
        assert!(check_release_disputable(true).is_ok());
        assert!(check_release_disputable(false)
            .unwrap_err()
            .contains("not DISPUTED"));

        assert!(check_seller_pubkey("release-dispute", Some("0x00000abc"), "0ABC").is_ok());
        assert!(check_seller_pubkey("release-dispute", Some("0xabc"), "def")
            .unwrap_err()
            .contains("seller key"));
        assert!(check_seller_pubkey("withdraw-shell", None, "abc")
            .unwrap_err()
            .contains("no seller pubkey"));
        assert!(check_seller_pubkey("destroy", Some("0xabc"), "def")
            .unwrap_err()
            .starts_with("destroy: --note-key is not the deal's seller key"));

        assert_eq!(check_withdrawable_shell(500, None).unwrap(), 500);
        assert_eq!(check_withdrawable_shell(500, Some(100)).unwrap(), 100);
        assert!(check_withdrawable_shell(0, None)
            .unwrap_err()
            .contains("no finalized"));
        assert!(check_withdrawable_shell(500, Some(501))
            .unwrap_err()
            .contains("exceeds"));
    }
}
