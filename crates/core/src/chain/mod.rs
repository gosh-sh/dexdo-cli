//! On-chain abstraction and the mock implementation for.
//! brings up **only what e2e needs**: offer/match, `open_stream`
//! (freezing the probe tick + locking `SELLER_PROBE_COMMISSION` + writing the enc-endpoint to
//! the endpoints file), `advance_tick`, `read_handover`, `stop`(incl. `BurnBoth` on the probe),
//! `seller_timeout`/settle. No networked on-chain.

use crate::machine::Settlement;
use crate::note::{Note, NotePubkey};
use async_trait::async_trait;

mod accounting;
mod mock;
mod types;
pub use accounting::*;
pub use mock::*;
pub use types::*;

/// On-chain abstraction. In -- `MockChainBackend`; in -- the shellnet adapter.
/// Brings up the minimum for e2e: discovery/book/oracle and subscriptions are the horizon.
#[async_trait]
pub trait ChainBackend: Send + Sync {
    /// Book discovery: the list of current offers with their sellers. The buyer
    /// filters/ranks against its frame(`buyer::routing::eligible_ranked`). Mock -- all offers;
    /// real -- reading `InferenceOrderBook`.
    async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError>;
    /// The seller posts an offer from a note.
    async fn post_offer(&self, offer: SellOffer, note: &dyn Note) -> Result<(), ChainError>;
    /// ensure THIS backend's note carries the current contract code before publishing an offer. The
    /// seller daemon path(`run_seller -> post_offer`) does NOT go through `provision_market`'s note-current
    /// gate, so a note orphaned by a contract redeploy(stale code_hash) would hit a raw `TVM_ERROR` from
    /// `postSellOffer`. Default `Ok(())`(mock/buyer/deal backends are not gated); the real seller backend
    /// overrides with the on-chain code_hash check, failing closed with an actionable "re-mint" message.
    async fn assert_note_current(&self) -> Result<(), ChainError> {
        Ok(())
    }
    /// ensure the per-deal `TokenContract` being advertised is FRESH(deployed but unused) before resting
    /// an ask on it. A deterministic `(sellerPubkey, nonce)` TC is a single-use per-deal resource -- if a prior
    /// deal already `opened`/`funded`/`disputed` it(or left residual deposit/prepaid/frozen/finalized), the
    /// seller's pre-stream steps(`fundProbeCommission`/`open`) revert with a raw `TVM_ERROR` (`ERR_ALREADY_OPEN`
    /// 321 and kin). Default `Ok(())`(mock/buyer/deal backends are not gated); the real seller backend overrides
    /// with the on-chain `getState` check, failing closed with an actionable "use a fresh nonce / recover+destroy".
    async fn assert_token_contract_fresh(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<(), ChainError> {
        Ok(())
    }
    /// after `post_offer`, confirm the seller's ask actually RESTED in the `InferenceOrderBook` before
    /// waiting for a match. A note-level `postSellOffer` can submit OK while the IOB rejects/does not rest the
    /// ask (e.g. a non-canonical `(sellerPubkey, nonce)` TC) -- the gateway listens but never matches and the
    /// buyer times out(300s). Default `Ok(())`(mock has no IOB); the real seller backend scans the book's
    /// orders(`getStats` + `getOrder`) and requires one whose `tokenContract` is THIS deal's -- `getBestBidAsk.
    /// hasAsk` alone is a false-green in a shared book -- failing closed with the IOB stats if our ask is absent.
    async fn assert_offer_rested(&self, _token_contract: &TokenContract) -> Result<(), ChainError> {
        Ok(())
    }
    /// before posting a sell offer, ensure the same `TokenContract` is not already represented by a
    /// resting/active ask in the model book. Fill/cancel/cleanup removes the old order first; only then may the
    /// seller repost. Default `Ok(())` for backends without a model order book.
    async fn assert_no_active_sell_order(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<(), ChainError> {
        Ok(())
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
    /// ([`Self::wait_matched_token_contract`]). Default: unsupported; the real shellnet buyer backend overrides it.
    async fn place_buy_by_model(
        &self,
        _note: &dyn Note,
        _ticks: u128,
        _max_price_per_tick: u128,
        _escrow: u128,
    ) -> Result<(), ChainError> {
        Err(ChainError::Chain(
            "place_buy_by_model: model-only buy is only supported on the real shellnet buyer backend".into(),
        ))
    }
    /// Read-only guard for automated model-only buyer selection. Real shellnet `placeInferenceBuy(modelHash,...)`
    /// cannot name an order id or `TokenContract`, so an executable quote is submit-safe only when the raw
    /// on-chain price/time matcher would reach the same ask. Default `Ok(())` keeps mock backends simple; the
    /// real buyer backend fails closed on stale raw heads before emitting a machine `quote_selected` event.
    async fn assert_model_buy_matches_executable_quote(
        &self,
        _ticks: u128,
        _max_price_per_tick: u128,
    ) -> Result<(), ChainError> {
        Ok(())
    }
    /// Read-only guard for explicit `--token-contract` buyer selection. A displayed quote is submit-safe only
    /// if the model-wide `placeInferenceBuy(modelHash,...)` matcher would actually fund this TokenContract,
    /// and the selected TC is still unused. Default `Ok(())` keeps mock backends simple; real shellnet fails
    /// closed before the CLI emits `quote_selected` or sends escrow.
    async fn assert_explicit_buy_matches_executable_quote(
        &self,
        _token_contract: &TokenContract,
        _ticks: u128,
        _max_price_per_tick: u128,
    ) -> Result<(), ChainError> {
        Ok(())
    }
    /// The current real shellnet submit path can safely buy only a single whole ask at the raw price-time head.
    /// Mock/backends without that order-book limitation keep the generic partial-depth quote.
    fn requires_submit_safe_single_ask_quote(&self) -> bool {
        false
    }
    /// After a model-only buy, learn the matched per-deal `TokenContract` from THIS note's owner-facing
    /// `InferenceFilledConfirmed` ext-out -- each side reads only its own note,
    /// no shared-book index. `since_unix` drops a prior deal's fill on a reused note. Default: unsupported;
    /// the real shellnet buyer backend overrides it.
    async fn wait_matched_token_contract(
        &self,
        _since_unix: i64,
        _timeout: std::time::Duration,
    ) -> Result<TokenContract, ChainError> {
        Err(ChainError::Chain(
            "wait_matched_token_contract: only supported on the real shellnet buyer backend".into(),
        ))
    }
    /// After model-only resume recovers a matched `TokenContract` from this note's fill event, prove by
    /// chain facts that the deal still belongs to the current buyer/backend and is still resumable. The
    /// default fails closed; real shellnet buyer backends override it with `getState`/model/buyer checks.
    async fn assert_model_only_resume_target(
        &self,
        token_contract: &TokenContract,
    ) -> Result<(), ChainError> {
        Err(ChainError::Chain(format!(
            "model-only resume validation is not supported for {token_contract}"
        )))
    }
    /// Non-blocking seller resume probe. Returns `Some(match)` only when the per-deal TC is already matched and
    /// still openable by the seller(funded, no prior handover/stream state). This is intentionally separate
    /// from [`Self::read_match`], which may wait for a future match after the seller has posted an offer.
    async fn read_openable_match_now(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Option<Match>, ChainError> {
        Ok(None)
    }
    /// Gateway-owned seller watch poll. The default uses an equivalent authoritative state source (the
    /// per-deal TC) and returns immediately; real shellnet seller backends override this to advance the
    /// note-event cursor as well. The caller owns sleeping/backoff and persists `cursor`.
    async fn poll_openable_match(
        &self,
        token_contract: &TokenContract,
        _cursor: &mut MatchWatchCursor,
    ) -> Result<Option<Match>, ChainError> {
        self.read_openable_match_now(token_contract).await
    }
    /// The seller waits for/reads the match on its own `token_contract`.
    async fn read_match(&self, token_contract: &TokenContract) -> Result<Match, ChainError>;
    /// The seller opens a stream: freezes the probe tick, locks
    /// `SELLER_PROBE_COMMISSION`, writes `encrypt_to(buyer_pubkey, endpoint)` to the endpoints file.
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
    /// The seller advances a tick on cadence.
    async fn advance_tick(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
    ) -> Result<(), ChainError>;
    /// Acceptance of the probe tick: `Probe` -> `Streaming`.
    async fn accept_probe(&self, token_contract: &TokenContract) -> Result<(), ChainError>;
    /// Buyer STOP; on the probe -> `BurnBoth`.
    async fn stop(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
    ) -> Result<Settlement, ChainError>;
    /// The buyer opens a dispute on the stream: the seller's note
    /// is locked (`streamDispute(tc)`->`TC.dispute()`) -- new offers/withdrawals are rejected with
    /// `ERR_STREAM_LOCKED`, until arbitration resolves the dispute. Default implementation: STOP (lower
    /// bound -- scam revenue=0); backends with disputes(mock/shellnet) override it to actually lock the scammer's note.
    async fn dispute(
        &self,
        token_contract: &TokenContract,
        note: &dyn Note,
    ) -> Result<Settlement, ChainError> {
        self.stop(token_contract, note).await
    }
    /// The seller **concedes the dispute**: `releaseDispute()` -> unlocks the notes and
    /// **returns the frozen tick to the buyer**(on the probe -- without burn). Default: not supported (backends
    /// with disputes -- mock/shellnet -- override it). Symmetric to `dispute`.
    async fn release_dispute(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        Err(ChainError::Chain(format!(
            "release_dispute not supported for {token_contract}"
        )))
    }
    /// The seller is gone: no-show/inactivity timeout -> refund to the buyer, without burn.
    async fn seller_timeout(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError>;
    /// The seller never opened a funded match: after MATCH_OPEN_TIMEOUT the buyer can clean up the unopened
    /// deal(`streamCleanup` -> `TC.cleanupUnopened`) and recover escrow. Default unsupported; real buyer
    /// backends override it.
    async fn cleanup_unopened(
        &self,
        token_contract: &TokenContract,
    ) -> Result<Settlement, ChainError> {
        Err(ChainError::Chain(format!(
            "cleanup_unopened not supported for {token_contract}"
        )))
    }
    /// Read by-fact per-deal lifecycle flags and timeout anchors from the chain. Default `None` keeps mock and
    /// unsupported backends on their local/session fallback; real shellnet buyer/deal backends override this so
    /// the long-running buyer monitor can derive cleanup/reclaim decisions from `TokenContract.getState`.
    async fn deal_state(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<Option<DealChainState>, ChainError> {
        Ok(None)
    }
    /// Snapshot of locks and burned SHELL for the contract -- for e2e checks.
    async fn snapshot(&self, token_contract: &TokenContract) -> Option<StreamSnapshot>;

    /// Observability **from the note**: a snapshot of the note's state -- its own orders,
    /// deals(role + anonymous counterparty + by-fact), exposure. **Read only** (the monitor moves
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

    /// Per-deal stream-phase cadence (`getConfig().settleWindow`,, dynamic since 4.0.5) for the
    /// seller-driven advance loop. Default zero(mock/fast paths + the buyer/deal backends); the real
    /// **seller** backend overrides it to read the deal's on-chain `getConfig`.
    async fn deal_settle_window(
        &self,
        _token_contract: &TokenContract,
    ) -> Result<std::time::Duration, ChainError> {
        Ok(std::time::Duration::ZERO)
    }
}

/// Note identifier for the blacklist/lock: hex of the ed-pubkey. The same kind of id
/// for the seller(`offer_sellers`) and the buyer(`Match.buyer_pubkey`), so both notes can be locked.
pub(crate) fn note_id_hex(pk: &NotePubkey) -> String {
    pk.ed.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::note::LocalNote;
    use crate::params::{DobParams, ProtocolConsts, Shell};

    /// regression: after dropping the buyer-side shared-book scan, canonical-TC safety must remain at the
    /// order-book entry point. The buyer cannot derive per-ask `(sellerPubkey, nonce)` from `getOrder`, so the
    /// on-chain `placeSellOffer` require is the source of truth.
    #[test]
    fn orderbook_source_enforces_canonical_sell_offer_tc() {
        let source = include_str!("../../../../contracts/airegistry/InferenceOrderBook.sol");
        assert!(
            source.contains(
                "require(tokenContract == _tokenContractAddr(sellerPubkey, nonce), ERR_BAD_TOKEN_CONTRACT);"
            ),
            "InferenceOrderBook.placeSellOffer must reject non-canonical token contracts before an ask can rest"
        );
    }

    /// regression: the contract source must reserve one active/resting sell order per TokenContract and
    /// clear that reservation when the old order leaves the book.
    #[test]
    fn orderbook_source_rejects_duplicate_active_sell_tc() {
        let source = include_str!("../../../../contracts/airegistry/InferenceOrderBook.sol");
        assert!(source.contains("mapping(address => bool) _sellTcInUse;"));
        assert!(source.contains("if (!o.isBuy) { _sellTcInUse[o.tokenContract] = true; }"));
        assert!(source.contains("if (!o.isBuy) { delete _sellTcInUse[o.tokenContract]; }"));
        assert!(source.contains("if (!e.isBuy && _sellTcInUse.exists(e.tokenContract))"));
    }

    /// regression: the buyer must not resurrect the old "every shared-book ask must equal my TC"
    /// canonicality scan(it false-closed on unrelated valid sellers), but the real shellnet path now has a
    /// narrower target preflight because `placeInferenceBuy(modelHash,...)` cannot name a TokenContract.
    #[test]
    fn buyer_path_has_targeted_preflight_without_old_canonical_guard() {
        let source = include_str!("../shellnet/backends.rs");
        assert!(!source.contains("assert_matchable_asks_canonical"));
        assert!(!source.contains("check_asks_canonical"));
        assert!(source.contains("assert_expected_buy_target"));
        assert!(source.contains("placeInferenceBuy cannot target a TokenContract"));
    }

    /// Issue(track-1, negative): the book's deposit check charges 2.5 % ON TOP of the limit price, so
    /// `escrow = maxPricePerTick x ticks`(without headroom) NEVER passes. The client must reject
    /// such a configuration in advance, otherwise the SHELL will orphan in the book.
    #[test]
    fn deposit_headroom_rejects_insufficient_escrow() {
        // Original numbers from: ticks=2, maxPrice=50M, escrow=100M. Requires 2x50Mx1.025 = 102.5M.
        assert_eq!(required_escrow_for_buy(2, 50_000_000), 102_500_000);
        let err = check_buy_deposit_headroom(100_000_000, 2, 50_000_000).unwrap_err();
        assert!(err.contains("ERR_INSUFFICIENT_DEPOSIT"), "{err}");
        // Any `escrow == maxPrice x ticks`(the old bug `maxPrice = escrow/ticks`) -- always falls short.
        assert!(check_buy_deposit_headroom(2_000_000, 2, 1_000_000).is_err());
        // Exactly 1 SHELL below the minimum -- rejected(the boundary is strict, check `>=`).
        let req = required_escrow_for_buy(2, 1_000_000);
        assert!(check_buy_deposit_headroom(req - 1, 2, 1_000_000).is_err());
    }

    /// Issue +(positive/boundary): the escrow must equal EXACTLY `required`(fee-inclusive) --
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

    /// Issue(over-funding rejected): the surplus `escrow - required` is debited but NOT refunded when
    /// the buy rests and is filled as a maker(live-proven on 4.0.10) -- the client now fails-closed on it.
    #[test]
    fn deposit_headroom_rejects_over_funding() {
        let req = required_escrow_for_buy(2, 1_000_000); // 2_050_000
        assert!(check_buy_deposit_headroom(req + 1, 2, 1_000_000).is_err());
        // The exact case in: escrow=100M, 8 ticks x maxPrice 1M(required 8.2M) -> over-funded surplus.
        assert_eq!(required_escrow_for_buy(8, 1_000_000), 8_200_000);
        let err = check_buy_deposit_headroom(100_000_000, 8, 1_000_000).unwrap_err();
        assert!(err.contains(""), "{err}");
        // The old over-funded control(110M, ticks=2, maxPrice=50M; required 102.5M) is now rejected.
        assert!(check_buy_deposit_headroom(110_000_000, 2, 50_000_000).is_err());
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
                disputed: false,
                probe_accepted: false,
                funded_time: None,
                last_advance: 0,
            },
            1000,
            MATCH_OPEN_TIMEOUT_SECS,
        )
        .unwrap_err();
        assert!(err.contains("not funded after the fill event"), "{err}");
        assert!(err.contains("refusing to wait for handover"), "{err}");
    }

    /// funded-but-never-opened is recognized as cleanup-eligible only after MATCH_OPEN_TIMEOUT.
    #[test]
    fn funded_never_opened_cleanup_readiness_is_timeout_gated() {
        let early = check_matched_token_contract_state(
            "0:tc",
            DealChainState {
                funded: true,
                opened: false,
                disputed: false,
                probe_accepted: false,
                funded_time: Some(100),
                last_advance: 0,
            },
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
            DealChainState {
                funded: true,
                opened: false,
                disputed: false,
                probe_accepted: false,
                funded_time: Some(100),
                last_advance: 0,
            },
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
    fn submit_safe_single_ask_quote_rejects_partial_head_fill() {
        let q =
            submit_safe_single_ask_quote(&[ask(1, "0:one", 1000, 1024)], Some(1), None).unwrap();
        assert!(!q.complete);
        assert_eq!(q.filled_ticks, 0);
        assert!(q.fills.is_empty());
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
    fn submit_safe_single_ask_quote_does_not_skip_mismatched_head() {
        let q = submit_safe_single_ask_quote(
            &[ask(1, "0:small", 900, 1), ask(2, "0:exact", 1000, 1024)],
            Some(1024),
            None,
        )
        .unwrap();
        assert!(!q.complete);
        assert_eq!(q.filled_ticks, 0);
        assert!(q.fills.is_empty());
    }

    #[test]
    fn submit_safe_single_ask_quote_budget_mode_uses_one_whole_head_ask() {
        let q =
            submit_safe_single_ask_quote(&[ask(1, "0:one", 1000, 4)], None, Some(4099)).unwrap();
        assert!(!q.complete);
        assert_eq!(q.filled_ticks, 0);

        let q =
            submit_safe_single_ask_quote(&[ask(1, "0:one", 1000, 4)], None, Some(4100)).unwrap();
        assert!(q.complete);
        assert_eq!(q.filled_ticks, 4);
        assert_eq!(q.total_with_fee, required_escrow_for_buy(4, 1000));
    }

    /// Issue(track-1, fail-closed): absurdly large `ticks`/`maxPricePerTick` must NOT panic
    /// (debug) and must NOT wrap(release) -- `required` saturates to `u128::MAX`, and the guard rejects.
    /// On the old code(`p * FEE_BPS` without `saturating_mul`) this test would have panicked on overflow.
    #[test]
    fn deposit_headroom_fails_closed_on_overflow() {
        // Overflowing inputs saturate(the wrapper) instead of panicking/wrapping.
        assert_eq!(required_escrow_for_buy(u128::MAX, u128::MAX), u128::MAX);
        assert_eq!(required_escrow_for_buy(1, u128::MAX), u128::MAX); // checked pxFEE_BPS overflows -> saturates
                                                                      // Any real escrow < the saturated minimum -> reject(fail-closed, without a panic).
        assert!(check_buy_deposit_headroom(u128::MAX - 1, u128::MAX, u128::MAX).is_err());
        // review: a SATURATED `required` must reject even when `escrow == required == u128::MAX`
        // (the exact-equality upper bound alone would otherwise let this absurd config slip through).
        assert!(check_buy_deposit_headroom(u128::MAX, u128::MAX, u128::MAX).is_err());
        assert!(check_buy_deposit_headroom(1_000_000_000, u128::MAX, 1_000_000).is_err());
        assert!(check_buy_deposit_headroom(0, 1, u128::MAX).is_err());
        // (`/ 10000`) back BELOW u128::MAX -- a truncated value a final `required == u128::MAX` check alone
        // would miss, accepting `escrow == required`(the garbage). The guard now rejects ANY arithmetic
        // overflow via the checked helper. p = u128::MAX/100 -> px250 overflows in the fee product.
        let p_fee_overflow = u128::MAX / 100;
        assert!(check_buy_deposit_headroom(
            required_escrow_for_buy(1, p_fee_overflow),
            1,
            p_fee_overflow
        )
        .is_err());
        assert!(check_buy_deposit_headroom(0, 1, p_fee_overflow).is_err());
        // A large but NON-overflowing value -- exact computation without saturation(= the contract's).
        assert_eq!(
            required_escrow_for_buy(1, 1_000_000_000_000),
            1_025_000_000_000
        );
    }

    /// regression: a shared book can hold a foreign seller's ask and the intended seller's ask at the
    /// same time; buying the intended TC must not fail closed merely because another valid seller has a
    /// different canonical TC.
    #[tokio::test]
    async fn shared_book_foreign_seller_ask_does_not_block_intended_buy() {
        let base = std::env::temp_dir().join(format!(
            "dexdo-shared-book-{}-{}",
            std::process::id(),
            "foreign-ask"
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let chain = MockChainBackend::new(
            base.join("eps.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let foreign_seller = LocalNote::from_seed(&[3u8; 32]);
        let intended_seller = LocalNote::from_seed(&[4u8; 32]);
        let buyer = LocalNote::from_seed(&[5u8; 32]);
        let foreign_tc = "tc-foreign".to_string();
        let intended_tc = "tc-intended".to_string();

        chain
            .post_offer(
                SellOffer {
                    price_per_tick: 1200,
                    max_ticks: 8,
                    token_contract: foreign_tc,
                },
                &foreign_seller,
            )
            .await
            .unwrap();
        chain
            .post_offer(
                SellOffer {
                    price_per_tick: 1000,
                    max_ticks: 8,
                    token_contract: intended_tc.clone(),
                },
                &intended_seller,
            )
            .await
            .unwrap();

        chain.place_buy(&intended_tc, &buyer).await.unwrap();
        let m = chain.read_match(&intended_tc).await.unwrap();
        assert_eq!(m.token_contract, intended_tc);
        assert_eq!(m.price_per_tick, 1000);
        assert_eq!(m.buyer_pubkey, buyer.pubkey());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// duplicate active sell posts for the same TC fail, but once a fill consumes the old active ask,
    /// a fresh post is allowed by the book-level guard.
    #[tokio::test]
    async fn mock_duplicate_sell_post_fails_until_fill_removes_old_order() {
        let base = std::env::temp_dir().join(format!(
            "dexdo-dup-post-{}-{}",
            std::process::id(),
            "active-tc"
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let chain = MockChainBackend::new(
            base.join("eps.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let seller = LocalNote::from_seed(&[11u8; 32]);
        let buyer = LocalNote::from_seed(&[12u8; 32]);
        let tc = "tc-dup".to_string();
        let offer = SellOffer {
            price_per_tick: 1000,
            max_ticks: 8,
            token_contract: tc.clone(),
        };

        chain.post_offer(offer.clone(), &seller).await.unwrap();
        let err = chain.post_offer(offer.clone(), &seller).await.unwrap_err();
        assert!(err.to_string().contains("duplicate active sell order"));
        assert_eq!(chain.discover_offers().await.unwrap().len(), 1);

        chain.place_buy(&tc, &buyer).await.unwrap();
        assert!(
            chain.discover_offers().await.unwrap().is_empty(),
            "fill removes the active ask from the book"
        );
        chain.post_offer(offer, &seller).await.unwrap();
        assert_eq!(chain.discover_offers().await.unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// (R11): `note_snapshot` shows the note's offers, its deals and the **anonymous**
    /// counterparty(the note's pubkey, not an identity); another note sees nothing.
    #[tokio::test]
    async fn note_snapshot_shows_offers_deals_and_anon_counterparty() {
        let base = std::env::temp_dir().join(format!("dexdo-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
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
                    price_per_tick: 1000,
                    max_ticks: 8,
                    token_contract: tc.clone(),
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

        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- issue: per-model by-fact accounting breakdown(pure) ----

    fn snap(
        received: Shell,
        seller_locked: Shell,
        buyer_locked: Shell,
        burned: Shell,
    ) -> StreamSnapshot {
        StreamSnapshot {
            seller_locked,
            buyer_locked,
            buyer_lead: buyer_locked, // test helper: treat the lock as the at-risk lead(the two-tick tests)
            seller_received: received,
            buyer_refunded: 0,
            burned,
            closed: false,
        }
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
    /// figures; the per-model roll-up is the sum across its counterparties(tokens = `received / price`).
    #[test]
    fn breakdown_groups_by_model_then_counterparty_with_rollup() {
        let deals = vec![
            deal(
                DealRole::Seller,
                Some("qwen"),
                Some("aa"),
                100,
                Some(snap(500, 200, 0, 50)),
            ),
            deal(
                DealRole::Seller,
                Some("qwen"),
                Some("bb"),
                100,
                Some(snap(300, 100, 0, 30)),
            ),
            deal(
                DealRole::Seller,
                Some("llama"),
                Some("cc"),
                100,
                Some(snap(200, 0, 0, 20)),
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
    /// from the SAME deal snapshot. `money`/`tokens` are the settled `seller_received` for both roles.
    #[test]
    fn breakdown_locked_is_role_specific() {
        let s = snap(400, 200, 700, 10);
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

    /// Finalized ticks = `received / price`. A zero price(a malformed/uninitialised deal) yields zero ticks
    /// rather than dividing by zero.
    #[test]
    fn breakdown_ticks_from_price_and_zero_price_guard() {
        let ok = vec![deal(
            DealRole::Seller,
            Some("m"),
            Some("x"),
            100,
            Some(snap(1000, 0, 0, 0)),
        )];
        assert_eq!(per_model_breakdown(&ok, DealRole::Seller)[0].tokens, 10);
        let zero = vec![deal(
            DealRole::Seller,
            Some("m"),
            Some("x"),
            0,
            Some(snap(1000, 0, 0, 0)),
        )];
        assert_eq!(
            per_model_breakdown(&zero, DealRole::Seller)[0].tokens,
            0,
            "no div-by-zero"
        );
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
            Some(snap(200, 0, 0, 0)),
        )];
        let b = per_model_breakdown(&deals, DealRole::Seller);
        assert_eq!(b[0].model, UNKNOWN_MODEL);
        assert_eq!(b[0].tokens, 2);
    }

    /// The view is per role: a seller-role query never includes buyer-role deals(and vice versa).
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

    /// Visibility of anomalies: a deal that locked SHELL but finalized nothing(`received=0`)
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
                Some(snap(200, 50, 0, 10)),
            ),
            deal(
                DealRole::Seller,
                Some("m"),
                Some("x"),
                100,
                Some(snap(300, 70, 0, 20)),
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

    // ---- issue: by-fact anomaly surfacing(pure) ----

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
    /// fee**(`p + pxFEE_BPS/10000`). A legitimate two-tick lock -- which the buyer escrows WITH the fee -- is
    /// NOT an anomaly; the bug was a fee-less `2 x p` ceiling that false-flagged every real two-tick deal.
    #[test]
    fn anomalies_flag_buyer_lock_over_two_ticks_fee_inclusive() {
        // The repro: price 10000 -> by-fact 2-tick lock = 2 x(10000 + 10000x250/10000) = 2 x 10250 = 20500.
        // The old fee-less ceiling(20000) false-flagged this legitimate lock; the fee-inclusive ceiling(20500)
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
        // One SHELL above the fee-inclusive two-tick ceiling -> flagged(a real over-lock).
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

    /// regression: the two-tick check bounds the at-risk LEAD(`prepaid + frozen`), NOT the total
    /// `buyer_locked`(which carries the unspent deposit for the remaining ticks). A legitimate 8-tick deal
    /// locks `8 x _unit(1000) = 8200` total but keeps its lead within 2 ticks -- it must NOT false-flag; only
    /// an oversized lead does.
    #[test]
    fn two_tick_bounds_lead_not_total_lock() {
        let snap_lead = |buyer_locked: Shell, buyer_lead: Shell| StreamSnapshot {
            seller_locked: 0,
            buyer_locked,
            buyer_lead,
            seller_received: 0,
            buyer_refunded: 0,
            burned: 0,
            closed: false,
        };
        // 8-tick total lock 8200, lead within the 2-tick ceiling(2050) -> NOT flagged.
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

    /// A zero price skips the two-tick check(no division/ceiling) rather than panicking or false-flagging.
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
    /// STOP: not-OPEN, disputed, a foreign note(not the deal's buyer), and an unmatched deal(no buyer).
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
        check_withdrawable_shell, MATCH_OPEN_TIMEOUT_SECS,
    };

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

    /// -- `check_reclaimable` is a fail-loud timer gate:
    /// opened+past-timeout -> ok; opened+too-early -> reject; funded-never-opened before
    /// MATCH_OPEN_TIMEOUT -> reject; funded-never-opened after MATCH_OPEN_TIMEOUT -> ok; not-funded /
    /// disputed / foreign-note / wrong-key / unmatched -> reject.
    #[test]
    fn reclaimable_gates() {
        let me = [7u8; 32];
        let other = [9u8; 32];
        // opened + past STREAM_TIMEOUT(now 1000 >= lastAdvance 100 + streamTimeout 600), owned -> ok
        assert!(check_reclaimable(
            true,
            true,
            false,
            Some("0:buyer"),
            "0:buyer",
            Some(&me),
            &me,
            1000,
            100,
            Some(600),
            None,
            MATCH_OPEN_TIMEOUT_SECS
        )
        .is_ok());
        // opened, before STREAM_TIMEOUT(now 500 < 700) -> reject
        assert!(check_reclaimable(
            true,
            true,
            false,
            Some("0:buyer"),
            "0:buyer",
            Some(&me),
            &me,
            500,
            100,
            Some(600),
            None,
            MATCH_OPEN_TIMEOUT_SECS
        )
        .unwrap_err()
        .contains("too early"));
        // funded but never opened before MATCH_OPEN_TIMEOUT -> reject.
        assert!(check_reclaimable(
            true,
            false,
            false,
            Some("0:buyer"),
            "0:buyer",
            Some(&me),
            &me,
            1099,
            0,
            None,
            Some(500),
            MATCH_OPEN_TIMEOUT_SECS
        )
        .unwrap_err()
        .contains("MATCH_OPEN_TIMEOUT"));
        // funded but never opened after MATCH_OPEN_TIMEOUT -> ok(streamCleanup path).
        assert!(check_reclaimable(
            true,
            false,
            false,
            Some("0:buyer"),
            "0:buyer",
            Some(&me),
            &me,
            1100,
            0,
            None,
            Some(500),
            MATCH_OPEN_TIMEOUT_SECS
        )
        .is_ok());
        // not funded -> reject
        assert!(check_reclaimable(
            false,
            true,
            false,
            Some("0:buyer"),
            "0:buyer",
            Some(&me),
            &me,
            9999,
            0,
            Some(600),
            None,
            MATCH_OPEN_TIMEOUT_SECS
        )
        .unwrap_err()
        .contains("not funded"));
        // disputed -> reject
        assert!(check_reclaimable(
            true,
            true,
            true,
            Some("0:buyer"),
            "0:buyer",
            Some(&me),
            &me,
            9999,
            0,
            Some(600),
            None,
            MATCH_OPEN_TIMEOUT_SECS
        )
        .unwrap_err()
        .contains("DISPUTED"));
        // foreign note -> reject
        assert!(check_reclaimable(
            true,
            true,
            false,
            Some("0:other"),
            "0:buyer",
            Some(&me),
            &me,
            9999,
            0,
            Some(600),
            None,
            MATCH_OPEN_TIMEOUT_SECS
        )
        .unwrap_err()
        .contains("not the deal's buyer note"));
        // wrong key -> reject
        assert!(check_reclaimable(
            true,
            true,
            false,
            Some("0:buyer"),
            "0:buyer",
            Some(&other),
            &me,
            9999,
            0,
            Some(600),
            None,
            MATCH_OPEN_TIMEOUT_SECS
        )
        .unwrap_err()
        .contains("not the deal's buyer key"));
        // unmatched -> reject
        assert!(check_reclaimable(
            true,
            true,
            false,
            None,
            "0:buyer",
            Some(&me),
            &me,
            9999,
            0,
            Some(600),
            None,
            MATCH_OPEN_TIMEOUT_SECS
        )
        .unwrap_err()
        .contains("no recorded buyer note"));
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
