//! Buyer routing: a smart client auto-executes the user's frame
//! across many sellers. Contains **candidate selection** (`eligible_ranked`: frame filter + blacklist
//! avoidance + ranking) and the **routing loop** (`route_capped_buy`: discovery -> ranking ->
//! iteration with failover; on scam/no-show -- anti-scam reaction + blacklist + next seller).
//! The deal+verification itself (stream, D4) lives behind the [`DealRunner`] seam (real -- gateway; test -- script).

use super::api::content_check_policy;
use super::verify::{reference_endpoint_for, ModelCheck, Verdict};
use super::Buyer;
use crate::api_format::{inspect_body, parse_frame, ApiFormat, Frame, RequestBody, SseFrames};
use crate::seller::ModelsConfig;
use async_trait::async_trait;
pub use dexdo_core::params::DEFAULT_SPOT_CHECK_RATE;
use dexdo_core::{
    params::{
        CONTENT_PROBE_MAX_TOKENS, SELLER_SCORE_BAIL_DELTA, SELLER_SCORE_PASS_DELTA,
        SPOT_CHECK_POSITIVE_SCORE_DECAY, SPOT_CHECK_UNKNOWN_SCORE_MULTIPLIER,
    },
    ChainBackend, Note, OfferListing,
};
use dexdo_proto::{BillingUsage, StreamFrame};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio_stream::{Stream, StreamExt};

/// Buyer's reaction to a caught scammer. Set **EXPLICITLY** at
/// client setup (no silent default): the trade-off "get service fast" vs "recover the tick / don't
/// let the scammer go".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScammerReaction {
    /// `stop()` -> `BurnBoth` on the probe (scam revenue = 0), blacklist, instant failover. The buyer's
    /// own probe tick is burned.
    Stop,
    /// `dispute()` freezes this TC's contested buyer amount and seller bond until resolution.
    /// Neither actor's whole note is locked; the current routing attempt waits.
    Dispute,
}

/// User frame for `CappedBuy` (B2): what / at what price / how much. The client does not exceed it.
#[derive(Debug, Clone)]
pub struct CappedBuy {
    /// The market frame's declared model -- only it is served (B2).
    pub model: String,
    /// Price ceiling per tick: an offer more expensive than this is not eligible.
    pub price_cap: u128,
    /// How many ticks to buy (needs an offer with `max_ticks` >= this).
    pub ticks: u128,
    /// Reaction to a scammer (set explicitly at client setup).
    pub scammer_reaction: ScammerReaction,
    /// Base rate of the B7-full spot-check / B8: Bernoulli per request,
    /// default ~0.03 (range 1-5%). For a seller with a low/unknown score -- more often (`spotcheck_rate_for`);
    /// `0.0` -- disabled.
    pub spot_check_rate: f64,
}

impl CappedBuy {
    /// **Production** constructor for a buy frame: the frame carries the safe **default**
    /// B7-full/B8 sampling rate ([`DEFAULT_SPOT_CHECK_RATE`]), so a production buy ALWAYS samples -- the
    /// content-identity layers fire on a fraction of requests, never silently `0.0`. Deterministic
    /// ranking/contrast tests build the struct literally with `spot_check_rate: 0.0` (sampling off, for a
    /// reproducible outcome); any non-test buy path must construct the frame through `new`. NB: the routing
    /// loop ([`route_capped_buy`]) is not yet wired into the `dexdo` CLI binary; when it
    /// is, it constructs the frame here and so inherits the non-zero default.
    pub fn new(
        model: String,
        price_cap: u128,
        ticks: u128,
        scammer_reaction: ScammerReaction,
    ) -> Self {
        Self {
            model,
            price_cap,
            ticks,
            scammer_reaction,
            spot_check_rate: DEFAULT_SPOT_CHECK_RATE,
        }
    }
}

/// Effective spot-check rate for a seller: the "score" = the buyer's **local private
/// memory B16** keyed by the **anonymous note public** (THIS buyer's history, not global
/// reputation -- intact). **Safe default:** an unknown/new note (`score <= 0`) -> check it
/// **MORE OFTEN** (x4), NOT "trust by default". Only a note that this buyer
/// has itself checked clean many times gets a reduced rate (`score > 0` -> asymptotically rarer). A scammer rotating notes
/// is always "unknown" -> always under maximum scrutiny. Clamped to [0, 1].
pub fn spotcheck_rate_for(base_rate: f64, score: i64) -> f64 {
    let factor = if score <= 0 {
        SPOT_CHECK_UNKNOWN_SCORE_MULTIPLIER // unfamiliar/caught -- elevated rate (safe default)
    } else {
        1.0 / (1.0 + score as f64 * SPOT_CHECK_POSITIVE_SCORE_DECAY) // the more stable the LOCAL history, the rarer
    };
    (base_rate * factor).clamp(0.0, 1.0)
}

/// A candidate offer from discovery (B1): the seller (note-id for the blacklist), its per-deal `TokenContract`
/// and the offer's price/volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Seller identifier (note address) -- the blacklist key (B16).
    pub seller_id: String,
    /// Address of the offer's per-deal `TokenContract`.
    pub token_contract: String,
    pub price_per_tick: u128,
    pub max_ticks: u128,
}

/// Local **private** memory of caught sellers (B16): not global reputation -- we avoid those
/// this buyer itself caught on scam/quality.
#[derive(Debug, Clone, Default)]
pub struct Blacklist {
    sellers: HashSet<String>,
}

impl Blacklist {
    pub fn new() -> Self {
        Self::default()
    }
    /// Add a seller (by note-id) to the blacklist after being caught (`Bail`/no-show).
    pub fn mark(&mut self, seller_id: &str) {
        self.sellers.insert(seller_id.to_string());
    }
    pub fn contains(&self, seller_id: &str) -> bool {
        self.sellers.contains(seller_id)
    }
    pub fn len(&self) -> usize {
        self.sellers.len()
    }
    pub fn is_empty(&self) -> bool {
        self.sellers.is_empty()
    }
}

/// Per-seller **verification score**: accumulates D4 verdicts (Pass `+1` / Bail `-1`)
/// in local private memory and influences **ranking** -- at equal price the seller with the better
/// history is taken first. Complements the hard blacklist (binary "avoid") with a soft quality signal:
/// a caught scammer is blacklisted anyway, but among the REMAINING ones the one who passed more often is preferred.
#[derive(Debug, Clone, Default)]
pub struct SellerScores {
    scores: HashMap<String, i64>,
}

impl SellerScores {
    pub fn new() -> Self {
        Self::default()
    }
    /// Successful verification (D4 `Pass` / delivery) -- the seller is more reliable.
    pub fn record_pass(&mut self, seller_id: &str) {
        *self.scores.entry(seller_id.to_string()).or_insert(0) += SELLER_SCORE_PASS_DELTA;
    }
    /// Bail/no-show (D4 `Bail` / no-show) -- the seller is worse for the frame.
    pub fn record_bail(&mut self, seller_id: &str) {
        *self.scores.entry(seller_id.to_string()).or_insert(0) += SELLER_SCORE_BAIL_DELTA;
    }
    /// Current score of a seller (`0` -- unfamiliar).
    pub fn score_of(&self, seller_id: &str) -> i64 {
        self.scores.get(seller_id).copied().unwrap_or(0)
    }
}

/// Select candidates for the frame (B1-B2): **price <= cap**, `max_ticks` >= required, **not blacklisted** --
/// in preference order: cheaper first; at equal price -- **higher verification score** (B4); on ties
/// stably by `seller_id`. Returns an ordered list for iteration with failover. Empty ->
/// no eligible sellers (frame/blacklist).
pub fn eligible_ranked(
    frame: &CappedBuy,
    offers: &[Candidate],
    blacklist: &Blacklist,
    scores: &SellerScores,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = offers
        .iter()
        .filter(|c| c.price_per_tick <= frame.price_cap)
        .filter(|c| c.max_ticks >= frame.ticks)
        .filter(|c| !blacklist.contains(&c.seller_id))
        .cloned()
        .collect();
    out.sort_by(|a, b| {
        a.price_per_tick
            .cmp(&b.price_per_tick)
            // at equal price -- the seller with the BETTER score first (higher score -> smaller in the sort).
            .then_with(|| {
                scores
                    .score_of(&b.seller_id)
                    .cmp(&scores.score_of(&a.seller_id))
            })
            .then_with(|| a.seller_id.cmp(&b.seller_id))
    });
    out
}

/// Discovery listings (`OfferListing`, chain width u64) -> frame candidates (u128) for ranking.
fn listings_to_candidates(offers: &[OfferListing]) -> Vec<Candidate> {
    offers
        .iter()
        .map(|o| Candidate {
            seller_id: o.seller_id.clone(),
            token_contract: o.token_contract.clone(),
            price_per_tick: o.price_per_tick as u128,
            max_ticks: o.max_ticks as u128,
        })
        .collect()
}

/// Outcome of a single deal with a candidate (after stream+verification D4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DealOutcome {
    /// The stream passed verification (D4 Pass) -- the user received the answer.
    Delivered,
    /// Verification returned `Bail` (substitution/scam); the string is the reason for the report.
    Scam(String),
    /// The seller did not open the stream / vanished (no handover, inactivity timeout).
    NoShow,
}

/// Deal+verification with one candidate -- the seam between routing orchestration and the gateway stream.
/// Real: read the handover from the chain, open an authorized TLS gRPC stream, check the model the answer
/// names (D4). Returns the outcome; the on-chain reaction (`stop`/`dispute`) and blacklist are applied by
/// the **routing loop**, not the runner (the runner does not know the frame policy).
#[async_trait]
pub trait DealRunner: Send + Sync {
    /// Execute the deal with a candidate. `spot_check` -- the request is **sampled** for a full audit
    /// on a `Delivered` outcome the real runner additionally runs a shadow
    /// B7-full (`reference_spotcheck`) + B8 (`behavioral_probe`); a mismatch -> `Scam`.
    async fn run(&self, candidate: &Candidate, spot_check: bool) -> DealOutcome;
}

/// A single iteration attempt (for the failover report/audit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attempt {
    pub seller_id: String,
    pub token_contract: String,
    pub outcome: DealOutcome,
    /// The applied anti-scam reaction (`Some` only for `Scam`).
    pub reaction: Option<ScammerReaction>,
}

/// Result of CappedBuy routing: who (if anyone) delivered + the log of attempts.
#[derive(Debug, Clone)]
pub struct RouteOutcome {
    /// The `seller_id` whose stream delivered; `None` -- no seller in the frame succeeded.
    pub served_by: Option<String>,
    /// Iteration in order: who was tried and with what outcome.
    pub attempts: Vec<Attempt>,
}

impl RouteOutcome {
    pub fn served(&self) -> bool {
        self.served_by.is_some()
    }
}

/// **CappedBuy routing loop**: book discovery -> ranking for the frame ->
/// iteration over candidates with failover. For each: `place_buy`
/// -> deal+verification (`runner`):
/// - `Delivered` -> delivered, return (we do not close an honest seller's stream);
/// - `Scam` -> anti-scam per the **explicit** frame policy (`Stop` -> `stop()`/BurnBoth, scam revenue=0;
/// `Dispute` -> `dispute()`, per-TC funds freeze) + `blacklist.mark`;
/// - `NoShow` -> `seller_timeout()` + `blacklist.mark` + next.

/// Exposure is bounded by the frame and the machine invariant (<= 2 ticks per stream): failover tries sellers
/// sequentially, not multiplying risk. The reaction and `seller_timeout` are best-effort: their error does not break
/// failover (the `attempts` record remains), the buyer still tries the next one.
pub async fn route_capped_buy(
    chain: &dyn ChainBackend,
    note: &dyn Note,
    frame: &CappedBuy,
    blacklist: &mut Blacklist,
    scores: &mut SellerScores,
    runner: &dyn DealRunner,
) -> RouteOutcome {
    let mut attempts = Vec::new();
    let offers = match chain.discover_offers().await {
        Ok(o) => o,
        Err(_) => {
            return RouteOutcome {
                served_by: None,
                attempts,
            }
        }
    };
    // Ranking accounts for the accumulated verification score (B4): the better history is taken first.
    let candidates = eligible_ranked(frame, &listings_to_candidates(&offers), blacklist, scores);

    for c in candidates {
        // The buyer sends a buy order. On failure -- skip the seller (as a no-show).
        if chain.place_buy(&c.token_contract, note).await.is_err() {
            blacklist.mark(&c.seller_id);
            scores.record_bail(&c.seller_id);
            attempts.push(Attempt {
                seller_id: c.seller_id.clone(),
                token_contract: c.token_contract.clone(),
                outcome: DealOutcome::NoShow,
                reaction: None,
            });
            continue;
        }

        // (lead's decision): sample the request for a full spot-check/B8 via Bernoulli with
        // a rate adjusted by the per-seller score (low/unknown -> more often). On a sample the real runner
        // runs a shadow audit; Bail -> Scam (like the inline layers).
        let rate = spotcheck_rate_for(frame.spot_check_rate, scores.score_of(&c.seller_id));
        let spot_check = rate > 0.0 && rand::random::<f64>() < rate;

        match runner.run(&c, spot_check).await {
            DealOutcome::Delivered => {
                scores.record_pass(&c.seller_id);
                attempts.push(Attempt {
                    seller_id: c.seller_id.clone(),
                    token_contract: c.token_contract.clone(),
                    outcome: DealOutcome::Delivered,
                    reaction: None,
                });
                return RouteOutcome {
                    served_by: Some(c.seller_id),
                    attempts,
                };
            }
            DealOutcome::Scam(reason) => {
                // anti-scam: reaction per the explicit frame policy (no silent default).
                let reaction = frame.scammer_reaction;
                let _ = match reaction {
                    ScammerReaction::Stop => chain.stop(&c.token_contract, note).await,
                    ScammerReaction::Dispute => chain.dispute(&c.token_contract, note).await,
                };
                blacklist.mark(&c.seller_id);
                scores.record_bail(&c.seller_id);
                attempts.push(Attempt {
                    seller_id: c.seller_id.clone(),
                    token_contract: c.token_contract.clone(),
                    outcome: DealOutcome::Scam(reason),
                    reaction: Some(reaction),
                });
                // Policy: a dispute freezes this TC's contested funds and stops the current routing
                // attempt while it awaits resolution. It does not lock either whole note.
                if reaction == ScammerReaction::Dispute {
                    break;
                }
            }
            DealOutcome::NoShow => {
                // The seller vanished. There is no inactivity timeout to wait out: STOP is available at any
                // moment and returns the escrow minus only the consumption already promoted -- which on a
                // no-show is nothing. Ignoring the result is deliberate; the routing decision (blacklist,
                // next candidate) does not depend on the recovery landing.
                let _ = chain.stop(&c.token_contract, note).await;
                blacklist.mark(&c.seller_id);
                scores.record_bail(&c.seller_id);
                attempts.push(Attempt {
                    seller_id: c.seller_id.clone(),
                    token_contract: c.token_contract.clone(),
                    outcome: DealOutcome::NoShow,
                    reaction: None,
                });
            }
        }
    }

    RouteOutcome {
        served_by: None,
        attempts,
    }
}

/// Production [`DealRunner`]: executes the deal via the buyer's gateway stream. Resolves the handover
/// from the chain (absent -> `NoShow`), opens an authorized TLS gRPC stream carrying the
/// user's request, and checks the model the answer names BEFORE accepting it: `Bail` -> `Scam` (B10 bail),
/// a clean finish with accepted output -> `Delivered`, an empty/severed stream -> `NoShow`.
/// The on-chain reaction (`stop`/`dispute`) and blacklist on top of this are applied by [`route_capped_buy`].
pub struct GatewayDealRunner<'a> {
    buyer: &'a Buyer,
    chain: &'a dyn ChainBackend,
    /// The user's Chat Completions request body (B19); the model is forced by the frame, so it is the same
    /// for all.
    request: Vec<u8>,
    /// The expected frame model (B7): the model the answer names is checked against it.
    expected_model: String,
    /// Total input+output budget available to this route execution.
    billing_grant_tokens: u64,
    /// Loaded model config -- B8/B7 verification data is data-driven from it, and the pre-`Delivered`
    /// content-identity gate (Path A fail-closed) mirrors the consumer-API `content_check_policy`.
    models: Arc<ModelsConfig>,
    /// `--mock-model`: mock output is fake by design -> the content gate is skipped.
    mock_model: bool,
    /// `--allow-unverified-model`: opt into paying a model with no content-identity check (name-only).
    allow_unverified: bool,
}

impl<'a> GatewayDealRunner<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        buyer: &'a Buyer,
        chain: &'a dyn ChainBackend,
        request: Vec<u8>,
        expected_model: String,
        billing_grant_tokens: u64,
        models: Arc<ModelsConfig>,
        mock_model: bool,
        allow_unverified: bool,
    ) -> Self {
        Self {
            buyer,
            chain,
            request,
            expected_model,
            billing_grant_tokens,
            models,
            mock_model,
            allow_unverified,
        }
    }
}

/// Path A fail-closed: mirror the consumer-API `content_check_policy`. A frame model with NO B8
/// fingerprint AND NO B7 reference key (in env) has no content layer that can catch a substituted model, so we
/// refuse to complete the deal (seller not paid) unless `--allow-unverified-model` was passed. This closes the
/// gap where the gateway path used to degrade both content layers to `Pass` and `Deliver` an unverifiable
/// model. Returns `Some(reason)` when delivery must be blocked, `None` when it may proceed. Pure/offline
/// (`std::env` read for the reference key only) so it is unit-testable without a live gateway.
pub(crate) fn gateway_content_refusal(
    expected_model: &str,
    models: &ModelsConfig,
    mock_model: bool,
    allow_unverified: bool,
) -> Option<String> {
    let has_ref_key = reference_endpoint_for(expected_model, models)
        .map(|e| {
            std::env::var(&e.api_key_env)
                .map(|k| !k.is_empty())
                .unwrap_or(false)
        })
        .unwrap_or(false);
    content_check_policy(
        expected_model,
        None,
        mock_model,
        allow_unverified,
        has_ref_key,
        models,
    )
    .err()
    .map(|e| e.to_string())
}

/// Read one answer: a successful head, output, and one validated terminal usage within the grant.
/// Returns the usage and the number of output-carrying frames.
async fn receive_gateway_answer<S>(
    stream: &mut S,
    format: ApiFormat,
    streamed: bool,
    check: &ModelCheck,
    billing_grant_tokens: u64,
) -> Result<(BillingUsage, u64), DealOutcome>
where
    S: Stream<Item = Result<StreamFrame, tonic::Status>> + Unpin,
{
    let mut success = None;
    let mut frames = SseFrames::default();
    let mut whole = Vec::new();
    let mut received_output = 0u64;
    let mut terminal_usage: Option<BillingUsage> = None;
    while let Some(item) = stream.next().await {
        let frame = match item {
            Ok(frame) => frame,
            Err(_) if received_output == 0 => return Err(DealOutcome::NoShow),
            Err(error) => return Err(DealOutcome::Scam(format!("gateway stream error: {error}"))),
        };
        if terminal_usage.is_some() {
            return Err(DealOutcome::Scam(
                "response content followed terminal billing usage".to_string(),
            ));
        }
        if let Some(head) = frame.head {
            if success.is_some() {
                return Err(DealOutcome::Scam(
                    "the seller stated a second response head".to_string(),
                ));
            }
            success = Some((200..300).contains(&head.status));
            continue;
        }
        if let Some(usage) = frame.usage {
            if let Err(error) = usage.validate() {
                return Err(DealOutcome::Scam(error.to_string()));
            }
            if usage.billable_tokens() > billing_grant_tokens {
                return Err(DealOutcome::Scam(format!(
                    "terminal billable usage {} exceeds billing grant {}",
                    usage.billable_tokens(),
                    billing_grant_tokens
                )));
            }
            if streamed && (received_output == 0 || usage.output_tokens == 0) {
                return Err(DealOutcome::Scam(
                    "terminal billing usage has no preceding delivered output".to_string(),
                ));
            }
            terminal_usage = Some(usage);
            continue;
        }
        match success {
            None => {
                return Err(DealOutcome::Scam(
                    "response content arrived before its response head".to_string(),
                ))
            }
            Some(false) => continue,
            Some(true) => {}
        }
        if !streamed {
            whole.extend_from_slice(&frame.data);
            continue;
        }
        let complete = frames
            .push(&frame.data)
            .map_err(|error| DealOutcome::Scam(error.to_string()))?;
        for complete in complete {
            let Frame::Event { facts, .. } =
                parse_frame(format, &complete.text).map_err(DealOutcome::Scam)?
            else {
                continue;
            };
            if let Some(Verdict::Bail(reason)) = facts.model.as_deref().map(|m| check.verify(m)) {
                return Err(DealOutcome::Scam(reason));
            }
            received_output += u64::from(facts.has_output());
        }
    }
    if success != Some(true) {
        return Err(DealOutcome::NoShow);
    }
    if !streamed {
        let value = serde_json::from_slice::<serde_json::Value>(&whole).map_err(|error| {
            DealOutcome::Scam(format!("provider response is not JSON: {error}"))
        })?;
        let facts = inspect_body(format, &value);
        if let Some(Verdict::Bail(reason)) = facts.model.as_deref().map(|m| check.verify(m)) {
            return Err(DealOutcome::Scam(reason));
        }
        received_output = u64::from(facts.has_output());
    }
    let Some(terminal_usage) = terminal_usage else {
        return if received_output == 0 {
            Err(DealOutcome::NoShow)
        } else {
            Err(DealOutcome::Scam(
                "response stream ended without terminal billing usage".to_string(),
            ))
        };
    };
    if received_output == 0 || terminal_usage.output_tokens == 0 {
        return Err(DealOutcome::Scam(
            "terminal billing usage has no preceding delivered output".to_string(),
        ));
    }
    Ok((terminal_usage, received_output))
}

#[async_trait]
impl DealRunner for GatewayDealRunner<'_> {
    async fn run(&self, candidate: &Candidate, spot_check: bool) -> DealOutcome {
        // The handover has not yet been written by the seller -> it did not open the stream (no-show).
        let handover = match self
            .buyer
            .resolve_endpoint(self.chain, &candidate.token_contract)
            .await
        {
            Ok(h) => h,
            Err(_) => return DealOutcome::NoShow,
        };
        let streamed = RequestBody::parse(&self.request).is_ok_and(|body| body.stream());
        // Authorized stream (B18); a connection/authorization failure is also a no-show.
        let mut stream = match self
            .buyer
            .open_request_stream(
                &handover,
                &candidate.token_contract,
                ApiFormat::ChatCompletions.path(),
                self.request.clone(),
                Vec::new(),
                self.billing_grant_tokens,
            )
            .await
        {
            Ok(s) => s,
            Err(_) => return DealOutcome::NoShow,
        };
        // D4: verification BEFORE accepting; Bail -> Scam (bail, exposure <= 2 ticks).
        let check = ModelCheck::new(&self.expected_model, Some(&self.models));
        let (terminal_usage, received_output) = match receive_gateway_answer(
            &mut stream,
            ApiFormat::ChatCompletions,
            streamed,
            &check,
            self.billing_grant_tokens,
        )
        .await
        {
            Ok(receipt) => receipt,
            Err(outcome) => return outcome,
        };
        if received_output == 0 {
            return DealOutcome::NoShow;
        }
        let mut remaining_grant = match self
            .billing_grant_tokens
            .checked_sub(terminal_usage.billable_tokens())
        {
            Some(remaining) => remaining,
            None => {
                return DealOutcome::Scam("terminal billable usage exceeded route grant".into())
            }
        };
        // (lead's decision): on a sampled request -- a shadow run of B7-full
        // (`reference_spotcheck`: greedy vs official endpoint) + B8 (`behavioral_probe`); any
        // mismatch -> `Scam` (the same reaction as the inline layers). No reference/key/model ->
        // degradation (Pass) inside the methods. Extra tick budget is spent only on the sample (1-5%).
        if spot_check {
            let b7_grant = remaining_grant;
            let b7_output_limit = CONTENT_PROBE_MAX_TOKENS.min(b7_grant);
            let b7_result = {
                let mut charge = |usage: BillingUsage| -> Result<(), String> {
                    remaining_grant = remaining_grant
                        .checked_sub(usage.billable_tokens())
                        .ok_or_else(|| {
                            "identity verification exceeded route billing grant".to_string()
                        })?;
                    Ok(())
                };
                self.buyer
                    .reference_spotcheck(
                        &handover,
                        &candidate.token_contract,
                        &self.expected_model,
                        &self.expected_model,
                        b7_output_limit,
                        b7_grant,
                        &self.models,
                        Some(&mut charge),
                        None,
                    )
                    .await
            };
            match b7_result {
                Ok(Verdict::Bail(reason)) => {
                    return DealOutcome::Scam(format!("spot-check B7: {reason}"));
                }
                Err(error) => {
                    return DealOutcome::Scam(format!("spot-check B7 failed: {error}"));
                }
                Ok(Verdict::Pass) => {}
            }
            let b8_grant = remaining_grant;
            let b8_output_limit = CONTENT_PROBE_MAX_TOKENS.min(b8_grant);
            let b8_result = {
                let mut charge = |usage: BillingUsage| -> Result<(), String> {
                    remaining_grant = remaining_grant
                        .checked_sub(usage.billable_tokens())
                        .ok_or_else(|| {
                            "identity verification exceeded route billing grant".to_string()
                        })?;
                    Ok(())
                };
                self.buyer
                    .behavioral_probe(
                        &handover,
                        &candidate.token_contract,
                        &self.expected_model,
                        &self.expected_model,
                        b8_output_limit,
                        b8_grant,
                        &self.models,
                        Some(&mut charge),
                        None,
                    )
                    .await
            };
            match b8_result {
                Ok(Verdict::Bail(reason)) => {
                    return DealOutcome::Scam(format!("spot-check B8: {reason}"));
                }
                Err(error) => {
                    return DealOutcome::Scam(format!("spot-check B8 failed: {error}"));
                }
                Ok(Verdict::Pass) => {}
            }
        }
        // Path A fail-closed: mirror Path B `content_check_policy`. If this frame model has NO content
        // check available (no B8 fingerprint AND no B7 reference key) and the operator did not opt into
        // name-only (`--allow-unverified-model`), REFUSE -- do not pay a model whose identity no layer can
        // verify. The model check above cannot catch a same-name cheaper-model substitution.
        if let Some(reason) = gateway_content_refusal(
            &self.expected_model,
            &self.models,
            self.mock_model,
            self.allow_unverified,
        ) {
            return DealOutcome::Scam(format!("content-identity refused (fail-closed): {reason}"));
        }
        DealOutcome::Delivered
    }
}

#[cfg(test)]
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;

    fn head() -> Result<StreamFrame, tonic::Status> {
        Ok(StreamFrame {
            head: Some(dexdo_proto::ResponseHead {
                status: 200,
                headers: Vec::new(),
            }),
            ..StreamFrame::default()
        })
    }

    fn content(text: &str) -> Result<StreamFrame, tonic::Status> {
        Ok(StreamFrame {
            data: format!(
                "data: {}\n\n",
                serde_json::json!({"model":"m","choices":[{"delta":{"content":text}}]})
            )
            .into_bytes(),
            ..StreamFrame::default()
        })
    }

    fn usage(input: u64, output: u64) -> Result<StreamFrame, tonic::Status> {
        Ok(StreamFrame {
            usage: Some(BillingUsage::new(input, output, None).unwrap()),
            ..StreamFrame::default()
        })
    }

    async fn receive(
        frames: Vec<Result<StreamFrame, tonic::Status>>,
        grant: u64,
    ) -> Result<(BillingUsage, u64), DealOutcome> {
        let mut stream = tokio_stream::iter(frames);
        receive_gateway_answer(
            &mut stream,
            ApiFormat::ChatCompletions,
            true,
            &ModelCheck::new("m", None),
            grant,
        )
        .await
    }

    #[tokio::test]
    async fn gateway_empty_or_severed_stream_before_output_remains_a_noshow() {
        assert_eq!(
            receive(Vec::new(), 16).await.unwrap_err(),
            DealOutcome::NoShow
        );
        assert_eq!(
            receive(vec![head(), Err(tonic::Status::unavailable("severed"))], 16)
                .await
                .unwrap_err(),
            DealOutcome::NoShow,
            "an empty or severed stream preserves the established no-show/refund path"
        );
    }

    #[tokio::test]
    async fn gateway_missing_terminal_usage_after_output_is_a_scam() {
        let outcome = receive(vec![head(), content("accepted")], 16)
            .await
            .unwrap_err();
        assert!(
            matches!(outcome, DealOutcome::Scam(reason) if reason.contains("without terminal billing usage")),
            "accepted output without authoritative terminal usage must fail closed"
        );
    }

    #[tokio::test]
    async fn gateway_real_text_then_transport_error_is_a_scam() {
        let outcome = receive(
            vec![
                head(),
                content("accepted"),
                Err(tonic::Status::unavailable("severed")),
            ],
            16,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(outcome, DealOutcome::Scam(reason) if reason.contains("gateway stream error"))
        );
    }

    #[tokio::test]
    async fn gateway_accepts_reasoning_inclusive_billing_above_the_visible_text() {
        let mut frames = vec![head()];
        frames.extend((0..3).map(|_| content("x")));
        frames.push(usage(1, 65));
        let (usage, outputs) = receive(frames, 66).await.unwrap();
        assert_eq!(usage.output_tokens, 65);
        assert_eq!(outputs, 3);
    }

    #[tokio::test]
    async fn gateway_bills_within_the_grant_and_bails_on_another_model() {
        let over = receive(vec![head(), content("x"), usage(10, 10)], 16)
            .await
            .unwrap_err();
        assert!(
            matches!(over, DealOutcome::Scam(reason) if reason.contains("exceeds billing grant"))
        );
        let foreign = Ok(StreamFrame {
            data: b"data: {\"model\":\"cheap/other\",\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n".to_vec(),
            ..StreamFrame::default()
        });
        let bail = receive(vec![head(), foreign, usage(1, 1)], 16)
            .await
            .unwrap_err();
        assert!(matches!(bail, DealOutcome::Scam(reason) if reason.contains("cheap/other")));
    }

    fn qwen_models_for_routing() -> ModelsConfig {
        ModelsConfig::from_json(
            r#"{ "models": { "qwen": {
                "frame_model": "qwen--qwen3--32b",
                "base_url": "https://api.groq.com/openai/v1",
                "served_model": "qwen/qwen3-32b",
                "api_key_env": "GROQ_API_KEY",
                "tokenizer_family": "qwen",
                "price_per_tick": 1000,
                "identity_aliases": ["Qwen/Qwen3-32B"],
                "vocab_size": 152064,
                "fingerprints": [ { "probe_prompt": "What is 17*23? Think step by step.", "expected_contains": "<think>", "accepts_reasoning_side_channel": true } ]
            } } }"#,
        )
        .unwrap()
    }

    #[test]
    fn path_a_refuses_model_with_no_verification_data() {
        // A real (non-mock) frame model with NO fingerprint (not in config) and NO reference key -> the gate
        // returns a refusal reason, so `run()` returns Scam (seller-not-paid), NOT Delivered.
        let cfg = qwen_models_for_routing();
        let refusal = gateway_content_refusal("meta-llama/llama-3.1-8b", &cfg, false, false);
        assert!(
            refusal.is_some(),
            "unverified model must be refused (fail-closed), got {refusal:?}"
        );
        assert!(refusal.unwrap().contains("no exact content-identity check"));
    }

    #[test]
    fn path_a_allows_unverified_model_with_opt_in() {
        // The explicit --allow-unverified-model opt-out lets the same name-only model through (name-only).
        let cfg = qwen_models_for_routing();
        assert!(
            gateway_content_refusal("meta-llama/llama-3.1-8b", &cfg, false, true).is_none(),
            "--allow-unverified-model opts into name-only delivery"
        );
    }

    #[test]
    fn path_a_allows_model_with_fingerprint() {
        // qwen HAS a B8 fingerprint in config -> the content gate can run -> delivery may proceed (no refusal).
        let cfg = qwen_models_for_routing();
        assert!(gateway_content_refusal("qwen--qwen3--32b", &cfg, false, false).is_none());
        // The served + registry spellings resolve to the same fingerprint -> also allowed.
        assert!(gateway_content_refusal("qwen/qwen3-32b", &cfg, false, false).is_none());
    }

    #[test]
    fn path_a_mock_model_is_exempt() {
        // --mock-model: fake tokens by design -> no content gate, delivery allowed even with no fingerprint.
        let cfg = qwen_models_for_routing();
        assert!(gateway_content_refusal("dexdo-mock", &cfg, true, false).is_none());
    }

    fn frame(cap: u128, ticks: u128) -> CappedBuy {
        CappedBuy {
            model: "qwen/qwen3-32b".to_string(),
            price_cap: cap,
            ticks,
            scammer_reaction: ScammerReaction::Stop,
            spot_check_rate: 0.0,
        }
    }

    fn cand(id: &str, price: u128, max_ticks: u128) -> Candidate {
        Candidate {
            seller_id: id.to_string(),
            token_contract: format!("tc-{id}"),
            price_per_tick: price,
            max_ticks,
        }
    }

    #[test]
    fn picks_cheapest_eligible_first() {
        let offers = vec![cand("a", 30, 10), cand("b", 10, 10), cand("c", 20, 10)];
        let ranked = eligible_ranked(
            &frame(100, 5),
            &offers,
            &Blacklist::new(),
            &SellerScores::new(),
        );
        assert_eq!(
            ranked
                .iter()
                .map(|c| c.seller_id.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c", "a"],
            "cheaper first"
        );
    }

    #[test]
    fn over_cap_excluded() {
        // Negative: an offer more expensive than the ceiling is not taken (frame B2 is not violated).
        let offers = vec![cand("cheap", 50, 10), cand("expensive", 150, 10)];
        let ranked = eligible_ranked(
            &frame(100, 5),
            &offers,
            &Blacklist::new(),
            &SellerScores::new(),
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].seller_id, "cheap");
    }

    #[test]
    fn blacklisted_seller_excluded() {
        // Negative: a caught seller (B16) is avoided even at a better price.
        let offers = vec![cand("scammer", 5, 10), cand("honest", 20, 10)];
        let mut bl = Blacklist::new();
        bl.mark("scammer");
        let ranked = eligible_ranked(&frame(100, 5), &offers, &bl, &SellerScores::new());
        assert_eq!(ranked.len(), 1);
        assert_eq!(
            ranked[0].seller_id, "honest",
            "scammer avoided, take the more expensive honest one"
        );
    }

    #[test]
    fn insufficient_capacity_excluded() {
        // Negative: an offer cannot carry the required tick volume -- not eligible.
        let offers = vec![cand("small", 10, 3), cand("big", 20, 10)];
        let ranked = eligible_ranked(
            &frame(100, 5),
            &offers,
            &Blacklist::new(),
            &SellerScores::new(),
        );
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].seller_id, "big");
    }

    #[test]
    fn empty_when_none_eligible() {
        let offers = vec![cand("a", 200, 10), cand("b", 300, 10)];
        assert!(eligible_ranked(
            &frame(100, 5),
            &offers,
            &Blacklist::new(),
            &SellerScores::new()
        )
        .is_empty());
    }

    #[test]
    fn scores_accumulate_pass_and_bail() {
        let mut s = SellerScores::new();
        assert_eq!(s.score_of("a"), 0, "unfamiliar seller -- 0");
        s.record_pass("a");
        s.record_pass("a");
        s.record_bail("a");
        assert_eq!(
            s.score_of("a"),
            SELLER_SCORE_PASS_DELTA * 2 + SELLER_SCORE_BAIL_DELTA,
            "score uses the canonical pass and bail deltas"
        );
        s.record_bail("b");
        assert_eq!(s.score_of("b"), SELLER_SCORE_BAIL_DELTA);
    }

    #[test]
    fn higher_score_ranked_first_at_equal_price() {
        // B4: at EQUAL price the seller with the better verification score is taken first.
        let offers = vec![cand("flaky", 10, 10), cand("solid", 10, 10)];
        let mut scores = SellerScores::new();
        scores.record_pass("solid");
        scores.record_bail("flaky");
        let ranked = eligible_ranked(&frame(100, 5), &offers, &Blacklist::new(), &scores);
        assert_eq!(
            ranked
                .iter()
                .map(|c| c.seller_id.as_str())
                .collect::<Vec<_>>(),
            vec!["solid", "flaky"],
            "better score first at equal price"
        );
    }

    #[test]
    fn price_dominates_score() {
        // Price takes priority over score: cheap-unfamiliar before expensive-with-good-history (frame B2).
        let offers = vec![cand("cheap_new", 10, 10), cand("pricey_good", 20, 10)];
        let mut scores = SellerScores::new();
        scores.record_pass("pricey_good");
        scores.record_pass("pricey_good");
        let ranked = eligible_ranked(&frame(100, 5), &offers, &Blacklist::new(), &scores);
        assert_eq!(
            ranked[0].seller_id, "cheap_new",
            "price takes priority over score"
        );
    }

    #[test]
    fn spotcheck_rate_scales_with_score() {
        // Lead's decision: unfamiliar/low score -> more often (x4); stable history -> rarer; clamp [0,1].
        let base = DEFAULT_SPOT_CHECK_RATE;
        let unknown_rate = base * SPOT_CHECK_UNKNOWN_SCORE_MULTIPLIER;
        // Unfamiliar (score 0) and caught (score < 0) -- elevated rate.
        assert!(
            (spotcheck_rate_for(base, 0) - unknown_rate).abs() < f64::EPSILON,
            "score 0 uses the canonical unknown-score multiplier"
        );
        assert!(
            (spotcheck_rate_for(base, -2) - unknown_rate).abs() < f64::EPSILON,
            "score<0 uses the canonical unknown-score multiplier"
        );
        // Stable history -- rarer than base.
        assert!(
            spotcheck_rate_for(base, 4) < base,
            "good score -> rarer than base"
        );
        assert!(
            spotcheck_rate_for(base, 10) < spotcheck_rate_for(base, 4),
            "even better score -> even rarer"
        );
        // Disabled / clamp.
        assert_eq!(spotcheck_rate_for(0.0, 0), 0.0, "0 base -> disabled");
        assert_eq!(
            spotcheck_rate_for(1.0, 0),
            1.0,
            "clamp to 1.0 (base 1.0, score 0 -> x4 -> 1.0)"
        );
    }

    #[test]
    fn unknown_note_always_max_scrutiny() {
        // Directive @43057bd: safe default -- an unknown note (score 0) is checked NO LESS OFTEN than
        // any note with local history. A scammer rotating notes is always "unknown" -> always
        // under maximum scrutiny; relaxation only for a note that has been clean many times.
        let base = DEFAULT_SPOT_CHECK_RATE;
        let unknown = spotcheck_rate_for(base, 0);
        for score in 1..=50 {
            assert!(
                unknown >= spotcheck_rate_for(base, score),
                "unknown note no rarer than a note with score {score}"
            );
        }
        assert!(
            unknown > base,
            "unknown note -- elevated rate, not trust by default"
        );
    }

    #[test]
    fn new_frame_carries_default_spot_check_rate() {
        // Finding 3: a frame built through the PRODUCTION constructor samples by default -- the
        // B7-full/B8 content-identity layers are never silently disabled (`0.0`). Test fixtures opt out
        // literally (`spot_check_rate: 0.0`) for deterministic ranking; a production buy goes through `new`.
        let f = CappedBuy::new("qwen/qwen3-32b".to_string(), 100, 1, ScammerReaction::Stop);
        assert_eq!(
            f.spot_check_rate, DEFAULT_SPOT_CHECK_RATE,
            "production frame carries the default sampling rate"
        );
        assert!(
            f.spot_check_rate > 0.0,
            "production frame samples (not 0.0)"
        );
    }

    use proptest::prelude::*;

    proptest! {
        /// Invariant: whatever comes as input, the selected candidates are ALWAYS in the frame
        /// (price <= cap, volume >= required) and NOT in the blacklist, and ordered by non-decreasing price --
        /// the frame (B2) and blacklist (B16) are not violated on any set of offers.
        #[test]
        fn ranked_respects_frame_and_blacklist(
            cap in 1u128..1000,
            ticks in 1u128..50,
            raw in proptest::collection::vec((any::<u8>(), 0u128..2000, 0u128..100), 0..30),
            black in proptest::collection::vec(any::<u8>(), 0..10),
        ) {
            let offers: Vec<Candidate> = raw.iter().enumerate().map(|(i, &(sid, price, mt))| Candidate {
                seller_id: format!("s{sid}"),
                token_contract: format!("tc{i}"),
                price_per_tick: price,
                max_ticks: mt,
            }).collect();
            let mut bl = Blacklist::new();
            for sid in &black { bl.mark(&format!("s{sid}")); }
            let f = CappedBuy {
                model: "m".to_string(),
                price_cap: cap,
                ticks,
                scammer_reaction: ScammerReaction::Stop,
                spot_check_rate: 0.0,
            };
            let ranked = eligible_ranked(&f, &offers, &bl, &SellerScores::new());
            for c in &ranked {
                prop_assert!(c.price_per_tick <= cap, "price within ceiling");
                prop_assert!(c.max_ticks >= ticks, "volume is enough");
                prop_assert!(!bl.contains(&c.seller_id), "not from the blacklist");
            }
            for w in ranked.windows(2) {
                prop_assert!(w[0].price_per_tick <= w[1].price_per_tick, "ranked by price");
            }
        }
    }
}
