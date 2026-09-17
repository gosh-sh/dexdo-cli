//! Seller command handler (Track C13, move-only).

use crate::cli::args::SellerArgs;
use crate::cli::commands::{
    chain_doctor_preflight, enforce_model_registry_policy, expected_order_book_for_note,
    load_enabled_model_registry_policy, order_book_active_from_contracts,
    resolve_model_registry_target, BookTarget,
};
use crate::cli::commands::{
    preload_model_registry_policy, save_runtime_deal_handle_for_network, RuntimeDealHandleInput,
};
use crate::cli::deals;
use crate::cli::policy;
use crate::cli::seller_policy::{
    apply_seller_dispute_policy, apply_seller_terminal_policy, classify_by_fact_advance_failure,
    classify_terminal_settlement, is_err_not_open, AdvanceFailureDisposition,
};
use crate::cli::support::*;
use anyhow::{bail, Result};
use dexdo::registry::{BuyerMissingBookPolicy, RegistryRole};
use dexdo_core::params::{
    SellerLivenessParams, DEFAULT_MATCH_POLL_INTERVAL, SELLER_TERMINAL_RECEIPT_POLL_INTERVAL,
    SELLER_TERMINAL_RECEIPT_TIMEOUT,
};
use dexdo_core::{DobParams, MatchWatchCursor, SellOfferOutcome};
use futures::{stream::FuturesUnordered, FutureExt as _, StreamExt as _};
use serde_json::json;
use std::future::Future;
use std::io::Write as _;
use std::pin::Pin;
use std::sync::Arc;
use tokio::task::JoinSet;

use crate::operator_shutdown_signal;

fn display_token_contract(value: impl std::fmt::Display) -> String {
    dexdo_core::address::display_self_dapp(&value.to_string())
}

struct ClaimDeliveryRuntimeEvent {
    delivery: dexdo::seller::gateway::DealDelivery,
    kind: dexdo::runtime_events::SellerClaimEventKind,
    token_contract: dexdo_core::TokenContract,
    measurement: dexdo::seller::ClaimDeliveryMeasurement,
}

struct OrdinaryCapacityObserver {
    gateway: std::sync::Arc<dexdo::seller::gateway::GatewayState>,
    delivery: dexdo::seller::gateway::DealDelivery,
    events: tokio::sync::mpsc::UnboundedSender<ClaimDeliveryRuntimeEvent>,
}

impl dexdo::seller::ClaimStateObserver for OrdinaryCapacityObserver {
    fn observe(
        &self,
        token_contract: &dexdo_core::TokenContract,
        state: dexdo_core::DealChainState,
    ) -> std::result::Result<(), dexdo_core::ChainError> {
        self.gateway
            .reconcile_ordinary_capacity(token_contract, state)
            .map(|_| ())
            .map_err(|error| {
                dexdo_core::ChainError::Chain(format!(
                    "TokenContract {}: persist ordinary delivery capacity from claim state: {error}",
                    display_token_contract(token_contract)
                ))
            })
    }

    fn observe_terminal(
        &self,
        token_contract: &dexdo_core::TokenContract,
    ) -> std::result::Result<(), dexdo_core::ChainError> {
        self.gateway
            .mark_deal_terminal(token_contract)
            .map_err(|error| {
                dexdo_core::ChainError::Chain(format!(
                    "TokenContract {}: remove terminal ordinary delivery capacity: {error}",
                    display_token_contract(token_contract)
                ))
            })
    }

    fn observe_chain_unavailable(
        &self,
        token_contract: &dexdo_core::TokenContract,
        _error: &dexdo_core::ChainError,
    ) {
        self.gateway.report_chain_unavailable(token_contract);
    }

    fn observe_probe_decision(
        &self,
        token_contract: &dexdo_core::TokenContract,
        measurement: dexdo::seller::ClaimDeliveryMeasurement,
    ) {
        queue_claim_delivery_measurement(
            &self.events,
            &self.delivery,
            token_contract,
            dexdo::runtime_events::SellerClaimEventKind::ProbeDecision,
            measurement,
        );
    }

    fn observe_claim_submitted(
        &self,
        token_contract: &dexdo_core::TokenContract,
        measurement: dexdo::seller::ClaimDeliveryMeasurement,
    ) {
        queue_claim_delivery_measurement(
            &self.events,
            &self.delivery,
            token_contract,
            dexdo::runtime_events::SellerClaimEventKind::ClaimSubmitted,
            measurement,
        );
    }
}

struct ClaimMeasurementObserver {
    gateway: std::sync::Arc<dexdo::seller::gateway::GatewayState>,
    delivery: dexdo::seller::gateway::DealDelivery,
    events: tokio::sync::mpsc::UnboundedSender<ClaimDeliveryRuntimeEvent>,
}

impl dexdo::seller::ClaimStateObserver for ClaimMeasurementObserver {
    fn observe(
        &self,
        _token_contract: &dexdo_core::TokenContract,
        _state: dexdo_core::DealChainState,
    ) -> std::result::Result<(), dexdo_core::ChainError> {
        Ok(())
    }

    fn observe_chain_unavailable(
        &self,
        token_contract: &dexdo_core::TokenContract,
        _error: &dexdo_core::ChainError,
    ) {
        self.gateway.report_chain_unavailable(token_contract);
    }

    fn observe_probe_decision(
        &self,
        token_contract: &dexdo_core::TokenContract,
        measurement: dexdo::seller::ClaimDeliveryMeasurement,
    ) {
        queue_claim_delivery_measurement(
            &self.events,
            &self.delivery,
            token_contract,
            dexdo::runtime_events::SellerClaimEventKind::ProbeDecision,
            measurement,
        );
    }

    fn observe_claim_submitted(
        &self,
        token_contract: &dexdo_core::TokenContract,
        measurement: dexdo::seller::ClaimDeliveryMeasurement,
    ) {
        queue_claim_delivery_measurement(
            &self.events,
            &self.delivery,
            token_contract,
            dexdo::runtime_events::SellerClaimEventKind::ClaimSubmitted,
            measurement,
        );
    }
}

struct SubscriptionCapacityObserver {
    gateway: std::sync::Arc<dexdo::seller::gateway::GatewayState>,
}

fn funded_tick_budget(token_contract: &str, funded_tokens: u128, tick_size: u64) -> Result<u128> {
    let token_contract = display_token_contract(token_contract);
    let tick_size = u128::from(tick_size);
    if tick_size == 0 || funded_tokens == 0 || !funded_tokens.is_multiple_of(tick_size) {
        bail!(
            "--token-contract {token_contract}: strict getSubscription().fundedTokens \
             {funded_tokens} is not a positive multiple of canonical tick size {tick_size}"
        );
    }
    Ok(funded_tokens / tick_size)
}

/// What the pool must do with a deal whose stream has just opened.

/// Generation 4.0.33 made every terminal path of the TokenContract end in `_payOwedAndDie()` ->
/// `_die()` -> `selfdestruct` (`contracts/airegistry/TokenContract.sol`: `stop()`:1402 and, via
/// `_closeClean`:1326,:1408; `sellerStop()`:1433; `finalize()`:1090; `settleWeek()`:1231;
/// the dispute resolutions:1743 -- all funnelling into:415/:426). A settled deal therefore leaves
/// NO account behind: the buyer's `stop()` is the LAST message the contract ever processes.

/// The SDK reads that fact as `Ok(None)`. `read_getter` returns `None` only when the account
/// snapshot is missing or not Active; transport failures and ABI decode failures both come back as
/// `Err`. So `None` from a TokenContract getter is not "unreadable" -- it is the settled-and-gone
/// fact, and reading it a moment after `seller_match_opened` means the buyer stopped inside the
/// window between the match and the settlement driver's first read.
#[derive(Debug)]
enum OpenedDealPlan {
    /// The strict coherent snapshot read back; start the settlement driver from it.
    Drive(Box<dexdo_core::DealChainSnapshot>),
    /// The TokenContract is already gone. Retire this deal and keep serving the rest of the pool:
    /// `_payOwedAndDie()` paid `_finalizedOwed` to `_sellerNote` before the destruct, so there is
    /// nothing left for a settlement driver to claim and nothing lost by not starting one.
    RetireSettled,
}

/// Read the strict coherent deal snapshot for a deal whose stream just opened.

/// Unreadable stays fatal -- the settlement driver must never be started from a guessed deal shape.
/// Only the account-is-gone answer is reclassified, and only into a retirement.
async fn plan_opened_deal(
    chain: &dyn dexdo_core::ChainBackend,
    token_contract: &dexdo_core::TokenContract,
) -> Result<OpenedDealPlan> {
    let token_contract_display = display_token_contract(token_contract);
    let snapshot = chain.deal_snapshot(token_contract).await.map_err(|error| {
        anyhow::anyhow!(
            "--token-contract {token_contract_display}: strict coherent deal snapshot is unreadable, \
                 refusing to choose the settlement driver from guessed deal shape: {error}"
        )
    })?;
    Ok(match snapshot {
        Some(snapshot) => OpenedDealPlan::Drive(Box::new(snapshot)),
        None => OpenedDealPlan::RetireSettled,
    })
}

#[async_trait::async_trait]
impl dexdo::seller::SubscriptionKeeperObserver for SubscriptionCapacityObserver {
    async fn observe(
        &self,
        token_contract: &dexdo_core::TokenContract,
        snapshot: Option<&dexdo_core::DealChainSnapshot>,
    ) -> std::result::Result<(), dexdo_core::ChainError> {
        let result = match snapshot {
            Some(snapshot) => self
                .gateway
                .reconcile_subscription_capacity(
                    token_contract,
                    snapshot.state,
                    snapshot.subscription,
                )
                .map(|_| ()),
            None => self.gateway.mark_subscription_terminal(token_contract),
        };
        result.map_err(|error| {
            dexdo_core::ChainError::Chain(format!(
                "TokenContract {}: persist subscription delivery capacity from keeper snapshot: {error}",
                display_token_contract(token_contract)
            ))
        })
    }
}

fn seller_offer_outcome_line(outcome: &SellOfferOutcome) -> String {
    match outcome {
        SellOfferOutcome::Rested { order_id } => {
            format!("seller_offer_outcome RESTED order_id={order_id}")
        }
        SellOfferOutcome::Matched => "seller_offer_outcome MATCHED".to_string(),
    }
}

/// The startup announcement printed before `seller_ready` for one prepared pool deal: a fresh post
/// reports its authoritative outcome (`seller_offer_outcome`), an adopted raw resting SELL reports
/// the resume (`seller_offer_resume`), and a startup with no resting or matched fact stays silent.
fn seller_offer_startup_line(startup: &dexdo::seller::SellerOfferStartup) -> Option<String> {
    match startup {
        dexdo::seller::SellerOfferStartup::ResumedResting { order_id } => {
            Some(format!("seller_offer_resume RESTING order_id={order_id}"))
        }
        dexdo::seller::SellerOfferStartup::Posted { outcome } => {
            outcome.as_ref().map(seller_offer_outcome_line)
        }
        dexdo::seller::SellerOfferStartup::ResumedFunded => None,
    }
}

fn seller_ready_line(
    token_contract: &str,
    gateway_advertise: &str,
    gateway_listen: &str,
    startup: &dexdo::seller::SellerOfferStartup,
    identity: Option<&dexdo::seller::liveness::RestingOfferIdentity>,
) -> Option<String> {
    let identity = identity?;
    let readiness = match startup {
        dexdo::seller::SellerOfferStartup::ResumedResting { order_id }
            if *order_id == identity.order_id =>
        {
            "resumed_resting_offer"
        }
        dexdo::seller::SellerOfferStartup::Posted {
            outcome: Some(SellOfferOutcome::Rested { order_id }),
        } if *order_id == identity.order_id => "exact_tc_offer_accepted",
        _ => return None,
    };
    Some(format!(
        "seller_ready token_contract={} gateway={} gateway_listen={} order_id={} readiness={}",
        dexdo_core::address::display_self_dapp(token_contract),
        gateway_advertise,
        gateway_listen,
        identity.order_id,
        readiness
    ))
}

// The JSON lifecycle events keep their published `dexdo.*.event.v1` address representation until the
// machine schemas are versioned together; only the human-readable seller lines carry the canonical form.
fn seller_shutdown_event(token_contract: &str) -> serde_json::Value {
    json!({
        "event": "stopping",
        "role": "seller",
        "token_contract": token_contract,
        "reason": "signal"
    })
}

fn emit_seller_shutdown_event(token_contract: &str) {
    println!("{}", seller_shutdown_event(token_contract));
    let _ = std::io::stdout().flush();
}

const SELLER_EVENT_SCHEMA: &str = dexdo::runtime_events::SELLER_EVENT_SCHEMA;

fn seller_runtime_event(
    seq: u64,
    event: &'static str,
    token_contract: &str,
    fields: serde_json::Value,
) -> serde_json::Value {
    let mut value = json!({
        "schema": SELLER_EVENT_SCHEMA,
        "seq": seq,
        "ts_unix": deals::now_unix().unwrap_or(0),
        "event": event,
        "role": "seller",
        "token_contract": token_contract,
    });
    if let (Some(target), Some(fields)) = (value.as_object_mut(), fields.as_object()) {
        target.extend(fields.clone());
    }
    value
}

fn emit_seller_runtime_event(event: &serde_json::Value) {
    println!("{event}");
    let _ = std::io::stdout().flush();
}

fn queue_claim_delivery_measurement(
    events: &tokio::sync::mpsc::UnboundedSender<ClaimDeliveryRuntimeEvent>,
    delivery: &dexdo::seller::gateway::DealDelivery,
    token_contract: &str,
    kind: dexdo::runtime_events::SellerClaimEventKind,
    measurement: dexdo::seller::ClaimDeliveryMeasurement,
) {
    let _ = events.send(ClaimDeliveryRuntimeEvent {
        delivery: delivery.clone(),
        kind,
        token_contract: token_contract.to_string(),
        measurement,
    });
}

fn emit_claim_delivery_measurement(event: ClaimDeliveryRuntimeEvent) {
    emit_seller_runtime_event(&dexdo::runtime_events::seller_claim_event(
        event.delivery.next_event_seq(),
        deals::now_unix().unwrap_or(0),
        &event.token_contract,
        event.kind,
        event.measurement,
    ));
}

fn upstream_failure_event(
    seq: u64,
    token_contract: &str,
    error_class: &str,
    retryable: bool,
    grpc_status: &str,
    http_status: Option<u16>,
) -> serde_json::Value {
    seller_runtime_event(
        seq,
        "upstream_failed",
        token_contract,
        json!({
            "error_class": error_class,
            "retryable": retryable,
            "grpc_status": grpc_status,
            "http_status": http_status,
        }),
    )
}

fn emit_upstream_failure(failure: dexdo::seller::gateway::UpstreamFailure) -> serde_json::Value {
    let event = upstream_failure_event(
        failure.next_event_seq(),
        &failure.token_contract,
        failure.error_class,
        failure.retryable,
        &failure.grpc_status,
        failure.http_status,
    );
    emit_seller_runtime_event(&event);
    event
}

fn emit_buyer_stop_terminal_trail(
    token_contract: &str,
    delivery: &dexdo::seller::gateway::DealDelivery,
    (to_seller, refund_to_buyer): (u128, u128),
) -> Vec<serde_json::Value> {
    if !delivery.claim_terminal_trail() {
        return Vec::new();
    }
    let observed = seller_runtime_event(
        delivery.next_event_seq(),
        "buyer_stop_observed",
        token_contract,
        json!({
            "source": "token_contract_event",
            "initiator": "buyer",
            "chain_event": "StreamStopped",
        }),
    );
    let settled = seller_runtime_event(
        delivery.next_event_seq(),
        "settled",
        token_contract,
        json!({
            "source": "token_contract_event",
            "state": "stopped",
            "terminal": true,
            "chain_event": "StreamStopped",
            "outcome": "AmicableSplit",
            "to_seller": to_seller.to_string(),
            "refund_to_buyer": refund_to_buyer.to_string(),
        }),
    );
    let exiting = seller_runtime_event(
        delivery.next_event_seq(),
        "exiting",
        token_contract,
        json!({
            "scope": "deal_worker",
            "reason": "settled",
            "exit_code": 0,
        }),
    );
    let events = vec![observed, settled, exiting];
    for event in &events {
        emit_seller_runtime_event(event);
    }
    events
}

async fn read_buyer_stop_settlement(
    chain: &dyn dexdo_core::ChainBackend,
    token_contract: &dexdo_core::TokenContract,
) -> Option<(u128, u128)> {
    match tokio::time::timeout(SELLER_TERMINAL_RECEIPT_TIMEOUT, async {
        loop {
            if let Ok(Some(settlement)) = chain.buyer_stop_settlement(token_contract).await {
                return settlement;
            }
            tokio::time::sleep(SELLER_TERMINAL_RECEIPT_POLL_INTERVAL).await;
        }
    })
    .await
    {
        Ok(settlement) => Some(settlement),
        Err(_) => {
            tracing::warn!(
                token_contract = %display_token_contract(token_contract),
                "buyer STOP settlement event unavailable after bounded receipt wait; no terminal event emitted"
            );
            None
        }
    }
}

fn seller_liveness_event(
    token_contract: &str,
    owner_note: Option<&str>,
    order_id: Option<u128>,
    event: &str,
    component: Option<&str>,
    outcome: &str,
    known_result: Option<&str>,
) -> serde_json::Value {
    json!({
        "event": event,
        "role": "seller",
        "timestamp": deals::now_unix().unwrap_or(0),
        "token_contract": dexdo_core::address::display_self_dapp(token_contract),
        "owner_note": owner_note.map(dexdo_core::address::display),
        "order_id": order_id.map(|value| value.to_string()),
        "component": component,
        "outcome": outcome,
        "known_result": known_result,
    })
}

fn emit_seller_liveness_event(
    token_contract: &str,
    owner_note: Option<&str>,
    order_id: Option<u128>,
    event: &str,
    component: Option<&str>,
    outcome: &str,
    known_result: Option<&str>,
) {
    println!(
        "{}",
        seller_liveness_event(
            token_contract,
            owner_note,
            order_id,
            event,
            component,
            outcome,
            known_result,
        )
    );
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
async fn react_to_seller_shutdown_signal<S>(
    mut shutdown: Pin<&mut S>,
    already_requested: bool,
    token_contract: &str,
) where
    S: Future<Output = ()> + ?Sized,
{
    if !already_requested {
        shutdown.as_mut().await;
    }
    emit_seller_shutdown_event(token_contract);
}

enum SellerGatewayStartup {
    Ready(dexdo::seller::RunningSeller),
    Stopped {
        reason: dexdo::seller::liveness::RestingStopReason,
        disposition: dexdo::seller::liveness::CancellationDisposition,
    },
}

async fn start_seller_gateway_with_liveness<G, S>(
    gateway: G,
    chain: &dyn dexdo_core::ChainBackend,
    cfg: &dexdo::seller::SellerConfig,
    existing_identity: Option<&dexdo::seller::liveness::RestingOfferIdentity>,
    shutdown: S,
) -> Result<SellerGatewayStartup>
where
    G: Future<Output = Result<dexdo::seller::RunningSeller>>,
    S: Future<Output = ()>,
{
    tokio::pin!(gateway);
    tokio::pin!(shutdown);
    let reason = tokio::select! {
        biased;
        _ = &mut shutdown => {
            dexdo::seller::liveness::RestingStopReason::Shutdown
        }
        result = &mut gateway => {
            match result {
                Ok(seller) => return Ok(SellerGatewayStartup::Ready(seller)),
                Err(error) => {
                    if existing_identity.is_none() {
                        return Err(error);
                    }
                    dexdo::seller::liveness::RestingStopReason::Health(
                        dexdo::seller::liveness::HealthFailure::new(
                            dexdo::seller::liveness::HealthComponent::GatewayTask,
                            false,
                            format!("gateway startup failed: {error}"),
                        )
                    )
                }
            }
        }
    };
    let disposition = match existing_identity {
        Some(identity) => dexdo::seller::liveness::cancel_and_confirm(chain, cfg, identity).await,
        None => dexdo::seller::liveness::CancellationDisposition::AlreadyAbsent,
    };
    Ok(SellerGatewayStartup::Stopped {
        reason,
        disposition,
    })
}

fn seller_watch_cursor_path(
    deals_dir: Option<&std::path::Path>,
    token_contract: &str,
) -> Result<std::path::PathBuf> {
    Ok(deals::resolve_deals_dir(deals_dir)?
        .join("seller-watch")
        .join(format!(
            "{}.cursor.json",
            deals::make_token_contract_id(token_contract)
        )))
}

fn seller_pool_dir(
    deals_dir: Option<&std::path::Path>,
    seller_note: &str,
) -> Result<std::path::PathBuf> {
    let seller_note = dexdo_core::normalize_wallet_address(seller_note)
        .map_err(|error| anyhow::anyhow!("invalid seller note for pool state: {error}"))?;
    Ok(deals::resolve_deals_dir(deals_dir)?
        .join("seller-pool")
        .join(deals::make_token_contract_id(&seller_note)))
}

/// The gateway's long-lived TLS identity, through the client's secret store.

/// The private key of the seller's gateway is a key like any other, and this is the first thing in
/// the client to go through `secret_store`: on a machine with a system store the bundle is held
/// there, and on a headless server -- which is where a seller actually runs -- it is an owner-only
/// file at exactly the path it has always been. An identity written by an earlier client keeps being
/// found either way, so a restart still presents the certificate a buyer already pinned.

/// Two things changed with the move, and both are the house rule arriving here rather than a new
/// decision. The permission check now runs BEFORE the read instead of after it -- the previous shape
/// had the whole bundle in memory before it decided whether it was allowed to look. And the check is
/// the one the rest of this repository uses, which admits any owner-only mode rather than `0600`
/// alone: `0400` on a key is stricter than what was demanded, and refusing it was refusing the
/// operator who was more careful than required.
fn load_or_create_gateway_tls(
    pool_dir: &std::path::Path,
) -> Result<dexdo::seller::tls::GatewayTls> {
    let store = crate::cli::secret_store::SecretStore::open()?;
    let name = crate::cli::secret_store::SecretName::at(pool_dir.join("gateway.pem"));
    if let Some(bundle) = store.read(&name)? {
        return dexdo::seller::tls::GatewayTls::from_pem_bundle(bundle.to_string()).map_err(
            |error| anyhow::anyhow!("load seller gateway TLS {}: {error}", name.file().display()),
        );
    }
    let tls = dexdo::seller::tls::GatewayTls::generate()?;
    store.write(&name, &tls.pem_bundle())?;
    Ok(tls)
}

struct SellerPoolLock {
    file: std::fs::File,
}

impl Drop for SellerPoolLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

fn acquire_seller_pool_lock(pool_dir: &std::path::Path) -> Result<SellerPoolLock> {
    std::fs::create_dir_all(pool_dir).map_err(|error| {
        anyhow::anyhow!(
            "create seller pool directory {}: {error}",
            pool_dir.display()
        )
    })?;
    let path = pool_dir.join("seller.lock");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(|error| anyhow::anyhow!("open seller pool lock {}: {error}", path.display()))?;
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => Ok(SellerPoolLock { file }),
        Err(error)
            if error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
                || error.kind() == std::io::ErrorKind::WouldBlock =>
        {
            bail!(
                "seller pool for this note is already running; lock {} is held",
                path.display()
            )
        }
        Err(error) => Err(anyhow::anyhow!(
            "lock seller pool {}: {error}",
            path.display()
        )),
    }
}

#[cfg(test)]
#[path = "seller_1416_stale_ask_race_tests.rs"]
mod seller_1416_stale_ask_race_tests;

#[derive(Clone)]
struct SellerPoolDeal {
    chain: Arc<dyn dexdo_core::ChainBackend>,
    cfg: dexdo::seller::SellerConfig,
    watch: dexdo::seller::SellerMatchWatchConfig,
    upstream: dexdo::seller::UpstreamConfig,
    nonce: u64,
    market: Option<dexdo_core::MarketManifest>,
}

/// one resting offer the pool is still answerable for.
/// re-review: the whole deal, not a triple of its parts.

/// The sweep has to be able to SERVE a match it discovers, and serving needs the watch config the
/// triple threw away. Carrying the deal is what turns the sweep's finding from a complaint into an
/// action.
type RestingEntry = (
    SellerPoolDeal,
    dexdo::seller::liveness::RestingOfferIdentity,
);

/// what the pool owes once a supervised offer has stopped.

/// This replaced a `bool`, and the reason is the review that caught the first attempt. A boolean
/// forced one answer onto two different questions. `CancellationDisposition::proven_unmatchable`
/// answers the question about the BOOK -- can this ask still be crossed -- and for every outcome but
/// one that is also the answer to the pool's question, which is whether it may stop answering for
/// the deal. `AlreadyMatched` is where they part: the ask can never match again, and precisely
/// because it already did, a buyer is now waiting. Reading "unmatchable" as "nothing owed" there
/// drops a real match on the floor.

/// So the two questions get two answers, and this one is an enum rather than a bool so a caller
/// cannot forget the match arm -- the compiler will not let it. One place, but one QUESTION.
#[derive(Debug, Clone)]
enum AfterStop {
    /// The ask is off the book and nobody is waiting: drop the deal.
    Retire,
    /// The ask may still be executable. Keep answering, and keep it where the sweep looks.
    Retain,
    /// The ask MATCHED. It cannot match again, and a buyer is owed service for it.

    /// re-review: the match TRAVELS with the decision. The first attempt returned a bare
    /// marker, so the one place that could act on it -- the shutdown sweep -- had a name for the
    /// obligation and no way to discharge it, and settled for logging that nobody answered. A
    /// decision that says "serve" must carry the thing to serve.
    ServeMatch(dexdo_core::Match),
}

impl AfterStop {
    /// The decision itself, without its payload -- `dexdo_core::Match` carries no `Eq`, and adding one
    /// to a chain type so an assertion could be written would be tailoring the domain to the test.
    #[cfg(test)]
    fn kind(&self) -> &'static str {
        match self {
            Self::Retire => "retire",
            Self::Retain => "retain",
            Self::ServeMatch(_) => "serve_match",
        }
    }
}

/// The single decision point. Both the running retire path and the shutdown sweep ask it, so they
/// cannot form separate opinions -- which is the defect this whole change exists to remove.
fn decide_after_stop(disposition: &dexdo::seller::liveness::CancellationDisposition) -> AfterStop {
    match disposition {
        dexdo::seller::liveness::CancellationDisposition::AlreadyMatched(matched) => {
            AfterStop::ServeMatch(matched.clone())
        }
        proven if proven.proven_unmatchable() => AfterStop::Retire,
        _ => AfterStop::Retain,
    }
}

/// review finding 1: must the pool put this deal back under a watcher?

/// Keeping the bookkeeping and the gateway route was not enough. After an unproven stop the sole
/// observer had ENDED, so a buyer who crossed the still-live ask was unserved no matter what the
/// sweep later concluded -- the ask stopped being an executable row in the book and became an
/// executed, abandoned deal. The observer has to go back too.

/// A shutdown is the one exception, and it is not an optimisation: re-arming while draining would
/// keep the pool alive forever. There the entry stays in `resting` and the sweep raises on it.
fn should_rearm_watcher(
    decision: &AfterStop,
    reason: &dexdo::seller::liveness::RestingStopReason,
) -> bool {
    match decision {
        AfterStop::Retire => false,
        // A match obliges service, and shutdown does not excuse it: the buyer already paid in.
        AfterStop::ServeMatch(_) => true,
        AfterStop::Retain => {
            !matches!(reason, dexdo::seller::liveness::RestingStopReason::Shutdown)
        }
    }
}

/// review finding 2: put the entry back on the identity the WATCH reported.

/// A relist advances the supervised identity to a successor. The pool captured its copy before the
/// watch began, so re-seating from that copy names a generation the book has already consumed: the
/// sweep would then prove the absence of a predecessor nobody cares about and leave the successor
/// resting, unserved, with a green refusal about somebody else's order. The reported identity wins;
/// the captured entry is only a fallback for the case where the watch reported none.
fn reseat_resting_after_stop(
    token_contract: &str,
    current_identity: Option<dexdo::seller::liveness::RestingOfferIdentity>,
    resting_entry: Option<RestingEntry>,
    resting: &mut std::collections::HashMap<String, RestingEntry>,
) {
    match (current_identity, resting_entry) {
        (Some(identity), Some((deal, _stale))) => {
            resting.insert(token_contract.to_string(), (deal, identity));
        }
        (None, Some(entry)) => {
            resting.insert(token_contract.to_string(), entry);
        }
        (Some(_), None) | (None, None) => {}
    }
}

/// point 2: what a bounded drain says when the bound is reached.

/// A silent exit on a timer is worse than the hang it replaces, because the hang is visible. This
/// names the deals whose watcher never reported back -- the ones removed from the sweep, because
/// forbids proving absence against a generation that may already be consumed.

/// Separated from the loop so the naming can be asserted without driving a live pool.
fn undrained_watchers_refusal(outstanding: &[String]) -> anyhow::Error {
    anyhow::anyhow!(
        "seller pool shutdown: {} watcher(s) did not finish within MATCH_OPEN_TIMEOUT ({}s) after \
         the stop signal; their resting offers were left unswept because the identity they were \
         advancing was never reported back: {}",
        outstanding.len(),
        dexdo_core::params::MATCH_OPEN_TIMEOUT.as_secs(),
        if outstanding.is_empty() {
            "<none recorded>".to_string()
        } else {
            outstanding.join(", ")
        }
    )
}

/// point 2: run the shutdown drain under a BOUND, and name what the bound cut off.

/// The stream and the bound are ARGUMENTS rather than the pool loop's own locals, and that is the
/// whole reason this function is separate. A test that builds its OWN `tokio::time::timeout` around
/// `pending()` asserts a property of tokio: it stays green with the bound taken out of the shutdown
/// path altogether, which is the one thing it exists to catch. Driving this function puts the
/// assertion on our code instead.

/// decides what the caller does with the names that come back. A watcher that never reported
/// back was advancing an identity nobody else has seen, so the sweep must not prove absence against
/// it. The caller drops those entries from the sweep, and the refusal names every one of them
/// rather than letting them go quiet.
async fn drain_watchers_within<S, K, A>(
    watched: &mut S,
    bound: std::time::Duration,
    expected: &std::collections::BTreeSet<String>,
    key: K,
    mut apply: A,
) -> Option<(Vec<String>, anyhow::Error)>
where
    S: futures::Stream + Unpin,
    K: Fn(&S::Item) -> String,
    A: std::ops::AsyncFnMut(S::Item),
{
    let mut drained: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let drain = async {
        while let Some(item) = watched.next().await {
            // Recorded BEFORE the item is applied, as the unextracted loop did: a watcher that
            // reported back has reported back, whatever its handling goes on to do.
            drained.insert(key(&item));
            apply(item).await;
        }
    };
    if tokio::time::timeout(bound, drain).await.is_ok() {
        return None;
    }
    let outstanding: Vec<String> = expected.difference(&drained).cloned().collect();
    let refusal = undrained_watchers_refusal(&outstanding);
    Some((outstanding, refusal))
}

#[cfg(test)]
#[path = "seller_drain_bound_1773.rs"]
mod seller_drain_bound_1773;

/// re-review, finding 2: what one DRAINED watcher does to the pool's books.

/// Named and separate because it is the only honest way to test the shutdown path: the loop's own
/// `select!` cannot be driven from a test, and a decision asserted outside the path it governs is an
/// assertion about a decision, not about the path. This is the path.

/// The identity used is the one the WATCH reported. That is the whole point of draining rather than
/// dropping: the supervisor advances its identity on a relist, and a dropped watcher takes the
/// advanced one with it, leaving the sweep pointed at a generation the book has already consumed.
async fn apply_drained_outcome(
    seller: &dexdo::seller::RunningSeller,
    deal: SellerPoolDeal,
    current_identity: Option<dexdo::seller::liveness::RestingOfferIdentity>,
    outcome: Result<dexdo::seller::liveness::RestingSellerOutcome>,
    resting: &mut std::collections::HashMap<String, RestingEntry>,
    first_error: &mut Option<anyhow::Error>,
) {
    let token_contract = deal.cfg.token_contract.clone();
    let previous = resting.remove(&token_contract);
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            // Nothing was established, so nothing may be concluded: keep the entry for the sweep.
            reseat_resting_after_stop(&token_contract, current_identity, previous, resting);
            first_error.get_or_insert(error);
            return;
        }
    };
    match outcome {
        // Already served inside `watch_pool_deal` before it returned; the ask is off the book.
        dexdo::seller::liveness::RestingSellerOutcome::Matched(_) => {}
        dexdo::seller::liveness::RestingSellerOutcome::Stopped {
            reason,
            disposition,
        } => match decide_after_stop(&disposition) {
            AfterStop::Retire => {
                seller.state.unregister_stream(&token_contract);
            }
            AfterStop::ServeMatch(matched) => {
                if let Err(error) = dexdo::seller::serve_watched_match(
                    seller,
                    deal.chain.as_ref(),
                    &deal.cfg,
                    &deal.watch,
                    matched,
                )
                .await
                {
                    first_error.get_or_insert(error);
                }
            }
            AfterStop::Retain => {
                tracing::warn!(
                    token_contract = %display_token_contract(&token_contract),
                    ?reason,
                    %disposition,
                    "seller pool drained a stopped resting deal whose ask is not proven off the book"
                );
                reseat_resting_after_stop(&token_contract, current_identity, previous, resting);
            }
        },
    }
}

/// the shutdown sweep -- the ONLY code that turns an unproven cancellation into a process
/// error, which is why a deal whose ask is not proven off the book has to still be in `resting` when
/// this runs. Extracted from the pool loop so that consequence is reachable from a test: asserting
/// that a key survived in a map proves nothing about whether anything ever acted on it.

/// Returns the FIRST failure, matching the pool's `first_error` discipline, and sweeps every entry
/// regardless so a later one is still cancelled rather than abandoned behind an earlier failure.
async fn sweep_unconfirmed_resting_offers(
    seller: &dexdo::seller::RunningSeller,
    entries: Vec<RestingEntry>,
) -> Option<anyhow::Error> {
    let mut first: Option<anyhow::Error> = None;
    for (deal, identity) in entries {
        let disposition =
            dexdo::seller::liveness::cancel_and_confirm(deal.chain.as_ref(), &deal.cfg, &identity)
                .await;
        match decide_after_stop(&disposition) {
            AfterStop::Retire => {}
            AfterStop::Retain => {
                first.get_or_insert_with(|| {
                    anyhow::anyhow!(
                        "seller pool could not confirm cancellation for {}: {disposition}",
                        display_token_contract(&deal.cfg.token_contract)
                    )
                });
            }
            // re-review, finding 1: a buyer crossed this ask and there is no watcher left, so
            // this is the LAST place that can answer him. The first attempt reported the fact and
            // stopped, which is the same loss the retire path used to cause, moved later and given a
            // better log line. Serve it here.
            AfterStop::ServeMatch(matched) => {
                match dexdo::seller::serve_watched_match(
                    seller,
                    deal.chain.as_ref(),
                    &deal.cfg,
                    &deal.watch,
                    matched,
                )
                .await
                {
                    Ok(served) => {
                        tracing::warn!(
                            token_contract = %display_token_contract(&deal.cfg.token_contract),
                            buyer = %dexdo_core::address::display_self_dapp(&served.token_contract),
                            "seller pool served a match found at shutdown rather than abandoning its buyer"
                        );
                    }
                    Err(error) => {
                        first.get_or_insert_with(|| {
                            anyhow::anyhow!(
                                "seller pool found a MATCH for {} at shutdown and could not serve \
                                 it: {error}",
                                display_token_contract(&deal.cfg.token_contract)
                            )
                        });
                    }
                }
            }
        }
    }
    first
}

type SellerAdvanceResult = (
    String,
    Arc<dyn dexdo_core::ChainBackend>,
    dexdo::seller::gateway::DealDelivery,
    bool,
    std::result::Result<u128, dexdo_core::ChainError>,
);

type SellerTerminalReceiptResult = (
    String,
    dexdo::seller::gateway::DealDelivery,
    Option<(u128, u128)>,
);

fn spawn_buyer_stop_receipt_wait(
    tasks: &mut JoinSet<SellerTerminalReceiptResult>,
    token_contract: String,
    chain: Arc<dyn dexdo_core::ChainBackend>,
    delivery: dexdo::seller::gateway::DealDelivery,
) {
    tasks.spawn(async move {
        let settlement = read_buyer_stop_settlement(chain.as_ref(), &token_contract).await;
        (token_contract, delivery, settlement)
    });
}

fn record_terminal_receipt_result(
    joined: std::result::Result<SellerTerminalReceiptResult, tokio::task::JoinError>,
    first_error: &mut Option<anyhow::Error>,
) -> Vec<serde_json::Value> {
    match joined {
        Ok((token_contract, delivery, Some(settlement))) => {
            emit_buyer_stop_terminal_trail(&token_contract, &delivery, settlement)
        }
        Ok((_, _, None)) => Vec::new(),
        Err(error) => {
            first_error.get_or_insert_with(|| {
                anyhow::anyhow!("seller terminal receipt task panicked: {error}")
            });
            Vec::new()
        }
    }
}

async fn record_advance_result(
    seller: &dexdo::seller::RunningSeller,
    joined: std::result::Result<SellerAdvanceResult, tokio::task::JoinError>,
    terminal_receipts: &mut JoinSet<SellerTerminalReceiptResult>,
    seller_policy: &policy::SellerRuntimePolicy,
    first_error: &mut Option<anyhow::Error>,
) {
    match joined {
        Ok((
            token_contract,
            chain,
            delivery,
            terminal_self_destruct_expected,
            Ok(claimed_tokens),
        )) => {
            // The driver returns the cumulative it CLAIMED, in tokens -- not what the contract promoted
            // and paid for. Naming it `finalized` here told the operator the opposite.
            tracing::info!(
                token_contract = %display_token_contract(&token_contract),
                claimed_tokens,
                "seller pool deal reached terminal by-fact state"
            );
            let mut seller_stop_attempted = false;
            match chain.deal_state(&token_contract).await {
                Ok(Some(state))
                    if delivery
                        .billing_exhausted
                        .load(std::sync::atomic::Ordering::Acquire)
                        && state.opened
                        && !state.disputed =>
                {
                    seller_stop_attempted = true;
                    match chain.seller_stop(&token_contract).await {
                        Ok(settlement) => {
                            tracing::info!(
                                token_contract = %display_token_contract(&token_contract),
                                claimed_tokens,
                                ?settlement,
                                "seller closed an open deal after its paid billing reservation was exhausted"
                            );
                            if let Err(error) = seller.state.mark_deal_terminal(&token_contract) {
                                first_error.get_or_insert_with(|| {
                                    anyhow::anyhow!(
                                        "--token-contract {}: sellerStop succeeded after billing exhaustion, \
                                         but terminal capacity cleanup failed: {error}",
                                        display_token_contract(&token_contract)
                                    )
                                });
                            }
                        }
                        Err(error) => {
                            first_error.get_or_insert_with(|| {
                                anyhow::anyhow!(
                                    "--token-contract {}: paid billing reservation was exhausted, \
                                     but sellerStop failed: {error}",
                                    display_token_contract(&token_contract)
                                )
                            });
                        }
                    }
                }
                Ok(Some(state)) => {
                    if let Err(error) = apply_seller_terminal_policy(
                        &token_contract,
                        seller_policy,
                        claimed_tokens,
                        state,
                    ) {
                        first_error.get_or_insert(error);
                    }
                }
                Ok(None) if terminal_self_destruct_expected => {
                    tracing::info!(
                        token_contract = %display_token_contract(&token_contract),
                        claimed_tokens,
                        "seller pool accepted the terminal TokenContract disappearance after a \
                        successful advance"
                    );
                }
                Ok(None) => {
                    first_error.get_or_insert_with(|| {
                        anyhow::anyhow!(
                            "--token-contract {}: authoritative terminal deal state is unavailable; \
                             refusing to guess buyer_no_show from claimed_tokens={claimed_tokens}",
                            display_token_contract(&token_contract)
                        )
                    });
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| {
                        anyhow::anyhow!(
                            "--token-contract {}: authoritative terminal deal state is unreadable; \
                             refusing to guess buyer_no_show from claimed_tokens={claimed_tokens}: {error}",
                            display_token_contract(&token_contract)
                        )
                    });
                }
            }
            seller.state.unregister_stream(&token_contract);
            if !seller_stop_attempted {
                spawn_buyer_stop_receipt_wait(terminal_receipts, token_contract, chain, delivery);
            }
        }
        Ok((token_contract, chain, delivery, _, Err(error))) => {
            tracing::error!(
                token_contract = %display_token_contract(&token_contract),
                error = %error,
                "seller pool isolated a failed deal"
            );
            let resolved = if is_err_not_open(&error) {
                match classify_by_fact_advance_failure(chain.as_ref(), &token_contract, &error)
                    .await
                {
                    Ok(AdvanceFailureDisposition::BenignTerminal { reason }) => {
                        tracing::info!(
                            token_contract = %display_token_contract(&token_contract),
                            %reason,
                            "seller pool retired terminal ERR_NOT_OPEN deal"
                        );
                        true
                    }
                    Ok(AdvanceFailureDisposition::Fault { reason }) => {
                        first_error.get_or_insert_with(|| {
                            anyhow::anyhow!(
                                "--token-contract {}: by-fact advance failed: {error}; \
                                 ERR_NOT_OPEN terminal check: {reason}",
                                display_token_contract(&token_contract)
                            )
                        });
                        false
                    }
                    Err(classify_error) => {
                        first_error.get_or_insert_with(|| {
                            anyhow::anyhow!(
                                "--token-contract {}: by-fact advance failed: {error}; \
                                 ERR_NOT_OPEN terminal classification failed: {classify_error}",
                                display_token_contract(&token_contract)
                            )
                        });
                        false
                    }
                }
            } else {
                // every terminal destroys the TokenContract. A late advance then fails on
                // getters that answer nothing even though immutable receipts already prove that
                // the deal finished. Classify that full lifecycle before treating a normal
                // terminal as a fault that kills the whole seller.
                let terminal =
                    match classify_terminal_settlement(chain.as_ref(), &token_contract).await {
                        Ok(reason) => reason,
                        Err(receipt_error) => {
                            tracing::warn!(
                                token_contract = %display_token_contract(&token_contract),
                                error = %receipt_error,
                                "seller pool could not prove a terminal from settlement receipts; \
                                 keeping the existing advance-failure classification"
                            );
                            None
                        }
                    };
                if let Some(reason) = terminal {
                    tracing::info!(
                        token_contract = %display_token_contract(&token_contract),
                        %reason,
                        "seller pool retired a deal with an immutable terminal settlement"
                    );
                    true
                } else {
                    match apply_seller_dispute_policy(
                        chain.as_ref(),
                        &token_contract,
                        seller_policy,
                        "advance-error",
                    )
                    .await
                    {
                        Ok(resolved) => resolved,
                        Err(policy_error) => {
                            first_error.get_or_insert(policy_error);
                            false
                        }
                    }
                }
            };
            if !resolved && first_error.is_none() {
                first_error.replace(anyhow::anyhow!(
                    "--token-contract {}: by-fact advance failed: {error}",
                    display_token_contract(&token_contract)
                ));
            }
            seller.state.unregister_stream(&token_contract);
            if resolved {
                spawn_buyer_stop_receipt_wait(terminal_receipts, token_contract, chain, delivery);
            }
        }
        Err(error) => {
            first_error
                .get_or_insert_with(|| anyhow::anyhow!("seller advance task panicked: {error}"));
        }
    }
}

struct SellerPoolContext<'a> {
    deals_dir: Option<&'a std::path::Path>,
    contracts: &'a std::path::Path,
    note_addr: &'a str,
    frame_model: &'a str,
    gateway_advertise: &'a str,
    /// how a failed `advertised_gateway` self-probe is treated.
    advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy,
    /// Present only for a human-invoked seller command which supplied both
    /// explicit recovery flags.  Timer/controller startup constructs `None`.
    recover_publication: Option<&'a str>,
}

fn save_pool_deal_handle(context: &SellerPoolContext<'_>, deal: &SellerPoolDeal) -> Result<()> {
    let Some(market) = deal.market.as_ref() else {
        return Ok(());
    };
    {
        save_runtime_deal_handle_for_network(
            RuntimeDealHandleInput {
                role: deals::DealHandleRole::Seller,
                deals_dir: context.deals_dir,
                token_contract: &deal.cfg.token_contract,
                note_addr: context.note_addr,
                frame_model: &market.frame_model,
                market: Some(market),
                market_path: None,
                contracts: context.contracts,
                endpoint: Some(deals::DealEndpointInfo {
                    kind: "gateway".to_string(),
                    value: context.gateway_advertise.to_string(),
                }),
                created_order_ids: Vec::new(),
            },
            deal.chain.network(),
            true,
        )?;
    }
    Ok(())
}

/// The gateway address is a property of the RUN, not of the deal.

/// The handle records the address this service last served the deal from. A seller that re-binds -
/// `--gateway-listen 127.0.0.1:0`, a restart after the old port was taken, a moved host - used to be
/// locked out of its own still-Active deals by an equality check on that record, with no way forward
/// for the operator at all: the deal cannot be dropped, the port cannot be recovered, and the service
/// refuses to start. It adopts this run's address instead, and rewrites the record so the next start
/// sees the truth.

/// Adopting is also what keeps the BUYER reachable rather than what strands it. Nothing else in the
/// client ever reads this field; the buyer learns the gateway from the on-chain handover ciphertext,
/// which `open_stream` writes from this same `gateway_advertise` on every open (`seller/mod.rs`,
/// `Handover { endpoint: format!("https://{}", cfg.gateway_advertise) }`). Pinning the record could
/// never have re-pointed a buyer at the dead address; it only stopped the deal being served from the
/// live one.

/// What the pin incidentally gave - two seller services must not fight over one deal - is held
/// properly by `acquire_seller_pool_lock`: an exclusive flock on `<seller pool dir>/seller.lock`,
/// keyed by exactly the (deals dir, note) pair over which handles are shared, taken before any chain
/// write. That is strictly stronger than the record it replaces here, because it also covers the case
/// the equality check never did - two services advertising the SAME address walked straight through
/// it - and it is already regressed by
/// `seller_pool_lock_contention_fails_before_any_chain_write`.

/// Returns the address that was displaced, so the caller can say what it re-bound; `None` when there
/// is nothing to change and the handle must not be rewritten.
fn adopt_run_gateway(handle: &mut deals::DealHandle, gateway_advertise: &str) -> Option<String> {
    let endpoint = handle.endpoint.as_mut()?;
    if endpoint.kind != "gateway" || endpoint.value == gateway_advertise {
        return None;
    }
    Some(std::mem::replace(
        &mut endpoint.value,
        gateway_advertise.to_string(),
    ))
}

struct SellerMarketHandle {
    path: std::path::PathBuf,
    handle: deals::DealHandle,
    market: dexdo_core::MarketManifest,
}

/// Read every seller handle for one note without adopting it into this run.

/// Startup needs this read-only view for two separate decisions: all known model books must have
/// their expired asks swept, while only the selected model (plus an already-funded obligation) may
/// enter the service pool. Rebinding here used to mutate every foreign-model handle before either
/// decision was made.
fn seller_market_handle_records(
    deals_dir: Option<&std::path::Path>,
    note_addr: &str,
) -> Result<std::collections::HashMap<String, SellerMarketHandle>> {
    let mut records: std::collections::HashMap<String, SellerMarketHandle> =
        std::collections::HashMap::new();
    let dir = deals::resolve_deals_dir(deals_dir)?;
    for (path, handle) in deals::list_deal_handles(&dir)? {
        if handle.role != deals::DealHandleRole::Seller
            || deals::normalize_addr(&handle.note_addr) != deals::normalize_addr(note_addr)
        {
            continue;
        }
        {
            let market = handle.market.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "seller deal handle {} for {} has no market manifest; nonce/config cannot be reconstructed",
                    path.display(),
                    display_token_contract(&handle.token_contract)
                )
            })?;
            assert_market_seller_note(&market.seller_note, note_addr)?;
        }
        let market = handle
            .market
            .clone()
            .expect("the market manifest was just read out of this handle");
        let key = deals::normalize_addr(&market.token_contract);
        if let Some(existing) = records.get(&key) {
            if existing.market != market {
                bail!(
                    "seller deal handles disagree about market {}",
                    display_token_contract(&market.token_contract)
                );
            }
        } else {
            records.insert(
                key,
                SellerMarketHandle {
                    path,
                    handle,
                    market,
                },
            );
        }
    }
    Ok(records)
}

#[cfg(test)]
fn seller_market_handles(
    deals_dir: Option<&std::path::Path>,
    note_addr: &str,
    gateway_advertise: &str,
) -> Result<std::collections::HashMap<String, dexdo_core::MarketManifest>> {
    let dir = deals::resolve_deals_dir(deals_dir)?;
    seller_market_handle_records(deals_dir, note_addr)?
        .into_iter()
        .map(|(key, mut record)| {
            if let Some(previous) = adopt_run_gateway(&mut record.handle, gateway_advertise) {
                tracing::warn!(
                    deal_handle = %record.path.display(),
                    token_contract = %record.handle.token_contract,
                    previous_gateway = %previous,
                    gateway = %gateway_advertise,
                    "seller deal handle re-bound to this service's gateway address"
                );
                deals::save_deal_handle(&dir, &record.handle)?;
            }
            Ok((key, record.market))
        })
        .collect()
}

fn seller_market_manifests(
    deals_dir: Option<&std::path::Path>,
    note_addr: &str,
) -> Result<std::collections::HashMap<String, dexdo_core::MarketManifest>> {
    Ok(seller_market_handle_records(deals_dir, note_addr)?
        .into_iter()
        .map(|(key, record)| (key, record.market))
        .collect())
}

async fn sweep_expired_seller_offer(
    chain: &dyn dexdo_core::ChainBackend,
    token_contract: &str,
    note_addr: &str,
    frame_model: &str,
) -> Result<()> {
    let token_contract_display = display_token_contract(token_contract);
    let expected_owner = dexdo_core::normalize_wallet_address(note_addr)
        .map_err(|error| anyhow::anyhow!("invalid seller note for expiry sweep: {error}"))?;
    let rows = chain
        .raw_resting_sell_orders_for_tc(&token_contract.to_string())
        .await?;
    for row in rows {
        let owner = dexdo_core::normalize_wallet_address(&row.owner_note).map_err(|error| {
            anyhow::anyhow!(
                "TokenContract {token_contract_display} resting SELL {} has an invalid owner: {error}",
                row.order_id
            )
        })?;
        if row.is_buy || owner != expected_owner {
            continue;
        }
        if dexdo_core::order_deadline_is_live(row.is_buy, row.deadline, deals::now_unix()?) {
            continue;
        }

        let timing = SellerLivenessParams::canonical();
        let deadline = tokio::time::Instant::now() + timing.offer_reap_timeout;
        let submit = match tokio::time::timeout_at(
            deadline,
            chain.expire_resting_sell_order(&token_contract.to_string(), row.order_id),
        )
        .await
        {
            Ok(Ok(())) => "submitted".to_string(),
            Ok(Err(error)) => format!("failed: {error}"),
            Err(_) => "timeout".to_string(),
        };

        loop {
            let observed = tokio::time::timeout_at(deadline, async {
                let rows = chain
                    .raw_resting_sell_orders_for_tc(&token_contract.to_string())
                    .await?;
                let latch = chain
                    .token_contract_offer_latch(&token_contract.to_string())
                    .await?;
                Ok::<_, dexdo_core::ChainError>((rows, latch))
            })
            .await;
            match observed {
                Ok(Ok((rows, Some(latch))))
                    if rows
                        .iter()
                        .all(|candidate| candidate.order_id != row.order_id)
                        && !latch.offer_posted =>
                {
                    tracing::info!(
                        token_contract = %token_contract_display,
                        frame_model,
                        order_id = row.order_id,
                        deadline = row.deadline,
                        expiry_submit = %submit,
                        "seller startup swept expired offer"
                    );
                    break;
                }
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    if tokio::time::Instant::now() >= deadline {
                        bail!(
                            "seller startup could not confirm expired offer removal for \
                             TokenContract {token_contract_display}, order {}: submit={submit}; {error}",
                            row.order_id
                        );
                    }
                }
                Err(_) => {
                    bail!(
                        "seller startup timed out confirming expired offer removal for \
                         TokenContract {token_contract_display}, order {}: submit={submit}",
                        row.order_id
                    );
                }
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "seller startup could not confirm expired offer removal for TokenContract \
                     {token_contract_display}, order {}: submit={submit}",
                    row.order_id
                );
            }
            let wake = std::cmp::min(
                tokio::time::Instant::now() + timing.offer_reap_poll,
                deadline,
            );
            tokio::time::sleep_until(wake).await;
        }
    }
    Ok(())
}

async fn sweep_configured_seller_model_books(
    args: &SellerArgs,
    note_addr: &str,
    startup_frame_model: &str,
    initial_token_contract: &str,
) -> Result<()> {
    // The manifest path comes from the environment now. The flag it used to
    // come from is gone, and with it the case where an operator typed something
    // unprintable -- what is left is a path this process was handed, which still has
    // to be text before it can be passed on as one.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let manifest = manifest_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds a path that is not printable text: {}",
            dexdo_core::params::MANIFEST_PATH_VAR,
            manifest_path.display()
        )
    })?;
    let chain = dexdo_core::RealChainBackend::connect(manifest)?;
    let note = dexdo_core::Address::parse(note_addr)
        .map_err(|error| anyhow::anyhow!("invalid seller note for expiry sweep: {error}"))?;
    let initial_token_contract_display = display_token_contract(initial_token_contract);
    let token_contract = dexdo_core::Address::parse(initial_token_contract).map_err(|error| {
        anyhow::anyhow!(
            "invalid startup TokenContract {initial_token_contract_display} for model validation: {error}"
        )
    })?;
    let on_chain_model = chain
        .token_contract_model_name(&token_contract)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "startup TokenContract {initial_token_contract_display} exposes no on-chain model name"
            )
        })?;
    let on_chain_model_hash = chain
        .token_contract_model_hash(&token_contract)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "startup TokenContract {initial_token_contract_display} exposes no on-chain model hash"
            )
        })?;
    let startup_model_hash = dexdo_core::model_hash_for(startup_frame_model);
    if on_chain_model != startup_frame_model
        || !on_chain_model_hash.eq_ignore_ascii_case(&startup_model_hash)
    {
        bail!(
            "startup TokenContract {initial_token_contract_display} is for model {on_chain_model} \
             ({on_chain_model_hash}), not requested model {startup_frame_model} \
             ({startup_model_hash})"
        );
    }
    let expected_owner = dexdo_core::normalize_wallet_address(note_addr)
        .map_err(|error| anyhow::anyhow!("invalid seller note for expiry sweep: {error}"))?;
    let models = dexdo::seller::ModelsConfig::load(&args.models)?;
    let mut frame_models = std::collections::HashSet::new();
    let timing = SellerLivenessParams::canonical();

    for model in models.models.values() {
        if !frame_models.insert(model.frame_model.clone()) {
            continue;
        }
        let model_hash = dexdo_core::model_hash_for(&model.frame_model);
        let snapshot = chain
            .inference_orderbook_snapshot_for_note(
                &note,
                &model.frame_model,
                &model_hash,
                dexdo_core::TICK_SIZE,
            )
            .await?;
        let order_book_display = dexdo_core::address::display(&snapshot.order_book);
        let order_book = dexdo_core::Address::parse(&snapshot.order_book).map_err(|error| {
            anyhow::anyhow!(
                "invalid {} order book {} during seller startup sweep: {error}",
                model.frame_model,
                order_book_display
            )
        })?;
        for row in snapshot.orders {
            let owner = dexdo_core::normalize_wallet_address(&row.owner_note).map_err(|error| {
                anyhow::anyhow!(
                    "{} resting SELL {} has an invalid owner: {error}",
                    order_book_display,
                    row.order_id
                )
            })?;
            if row.is_buy || owner != expected_owner {
                continue;
            }
            // A resting SELL of another model belongs to this note, but not to this startup. A
            // live one is left alone: a sibling `dexdo seller` on the same note may be serving it,
            // and only that instance may retract it -- the book would refuse anyway, since
            // `expireOrder` changes an already-expired order and nothing else. An expired one is
            // reaped, because an expired order is dead for every instance alike.

            // Both outcomes used to share the message "skipped non-active deal for another model",
            // copied from the local-handle scan, which does skip. Here nothing was skipped: the
            // order went on to the expiry check and could be reaped one line later. Reading
            // "skipped" while the order is gone costs an incident review its first hour.
            let foreign_model = model.frame_model != startup_frame_model;
            if dexdo_core::order_deadline_is_live(row.is_buy, row.deadline, deals::now_unix()?) {
                if foreign_model {
                    tracing::debug!(
                        token_contract = row.token_contract.as_deref().unwrap_or("none"),
                        deal_model = %model.frame_model,
                        startup_model = startup_frame_model,
                        "seller startup left a live resting SELL of another model untouched"
                    );
                }
                continue;
            }
            if foreign_model {
                tracing::warn!(
                    token_contract = %dexdo_core::address::display_self_dapp_opt(
                        row.token_contract.as_deref(),
                        "none"
                    ),
                    deal_model = %model.frame_model,
                    startup_model = startup_frame_model,
                    "seller startup reaps an expired resting SELL of another model"
                );
            }

            let token_contract = row
                .token_contract
                .as_deref()
                .map(dexdo_core::Address::parse)
                .transpose()
                .map_err(|error| {
                    anyhow::anyhow!(
                        "{} resting SELL {} has an invalid TokenContract: {error}",
                        order_book_display,
                        row.order_id
                    )
                })?;
            let deadline = tokio::time::Instant::now() + timing.offer_reap_timeout;
            let submit = match tokio::time::timeout_at(
                deadline,
                chain.expire_inference_order(&order_book, row.order_id),
            )
            .await
            {
                Ok(Ok(_)) => "submitted".to_string(),
                Ok(Err(error)) => format!("failed: {error}"),
                Err(_) => "timeout".to_string(),
            };

            loop {
                let observed = tokio::time::timeout_at(deadline, async {
                    let still_present = chain
                        .inference_orderbook_parsed_order(&order_book, row.order_id)
                        .await?
                        .is_some();
                    let latch = match token_contract.as_ref() {
                        Some(token_contract) => chain.token_contract_offer(token_contract).await?,
                        None => None,
                    };
                    Ok::<_, anyhow::Error>((still_present, latch))
                })
                .await;
                match observed {
                    Ok(Ok((false, latch)))
                        if latch.as_ref().is_none_or(|latch| !latch.offer_posted) =>
                    {
                        tracing::info!(
                            order_book = %order_book_display,
                            frame_model = %model.frame_model,
                            token_contract = %dexdo_core::address::display_self_dapp_opt(
                                row.token_contract.as_deref(),
                                "none"
                            ),
                            order_id = row.order_id,
                            deadline = row.deadline,
                            expiry_submit = %submit,
                            "seller startup swept expired offer"
                        );
                        break;
                    }
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        if tokio::time::Instant::now() >= deadline {
                            bail!(
                                "seller startup could not confirm expired offer removal from {} for order {}: \
                                 submit={submit}; {error}",
                                order_book_display,
                                row.order_id
                            );
                        }
                    }
                    Err(_) => {
                        bail!(
                            "seller startup timed out confirming expired offer removal from {} for order {}: \
                             submit={submit}",
                            order_book_display,
                            row.order_id
                        );
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    bail!(
                        "seller startup could not confirm expired offer removal from {} for order {}: \
                         submit={submit}",
                        order_book_display,
                        row.order_id
                    );
                }
                let wake = std::cmp::min(
                    tokio::time::Instant::now() + timing.offer_reap_poll,
                    deadline,
                );
                tokio::time::sleep_until(wake).await;
            }
        }
    }
    Ok(())
}

async fn sweep_seller_startup_offers<F>(
    context: &SellerPoolContext<'_>,
    initial_chain: &Arc<dyn dexdo_core::ChainBackend>,
    initial_token_contract: &str,
    mut chain_for_market: F,
) -> Result<std::collections::HashSet<String>>
where
    F: FnMut(&dexdo_core::MarketManifest) -> Result<Arc<dyn dexdo_core::ChainBackend>>,
{
    let records = seller_market_handle_records(context.deals_dir, context.note_addr)?;
    let initial_key = deals::normalize_addr(initial_token_contract);
    let initial_model = records
        .get(&initial_key)
        .map(|record| record.market.frame_model.as_str())
        .unwrap_or(context.frame_model);
    sweep_expired_seller_offer(
        initial_chain.as_ref(),
        initial_token_contract,
        context.note_addr,
        initial_model,
    )
    .await?;

    let mut active = std::collections::HashSet::new();
    if initial_chain
        .deal_state(&initial_token_contract.to_string())
        .await?
        .is_some_and(|state| state.funded && !state.is_stopped())
    {
        active.insert(initial_key.clone());
    }

    for (key, record) in records {
        if key == initial_key {
            continue;
        }
        let chain = chain_for_market(&record.market)?;
        sweep_expired_seller_offer(
            chain.as_ref(),
            &record.market.token_contract,
            context.note_addr,
            &record.market.frame_model,
        )
        .await?;
        if chain
            .deal_state(&record.market.token_contract)
            .await?
            .is_some_and(|state| state.funded && !state.is_stopped())
        {
            active.insert(key);
        }
    }
    Ok(active)
}

#[cfg(test)]
async fn load_seller_pool_deals<F>(
    context: &SellerPoolContext<'_>,
    initial: SellerPoolDeal,
    mock_token_count: u64,
    backend_for_market: F,
) -> Result<Vec<SellerPoolDeal>>
where
    F: FnMut(
        &dexdo_core::MarketManifest,
    ) -> Result<(
        Arc<dyn dexdo_core::ChainBackend>,
        dexdo::seller::UpstreamConfig,
    )>,
{
    load_seller_pool_deals_with_scope(context, initial, mock_token_count, None, backend_for_market)
        .await
}

async fn load_seller_pool_deals_with_scope<F>(
    context: &SellerPoolContext<'_>,
    mut initial: SellerPoolDeal,
    mock_token_count: u64,
    active_deals: Option<&std::collections::HashSet<String>>,
    mut backend_for_market: F,
) -> Result<Vec<SellerPoolDeal>>
where
    F: FnMut(
        &dexdo_core::MarketManifest,
    ) -> Result<(
        Arc<dyn dexdo_core::ChainBackend>,
        dexdo::seller::UpstreamConfig,
    )>,
{
    let deals_dir = deals::resolve_deals_dir(context.deals_dir)?;
    let mut records = seller_market_handle_records(context.deals_dir, context.note_addr)?;
    let explicit_initial_market = initial.market.take();

    let initial_key = deals::normalize_addr(&initial.cfg.token_contract);
    let mut initial_record = records.remove(&initial_key);
    let initial_market = explicit_initial_market
        .or_else(|| initial_record.as_ref().map(|record| record.market.clone()));
    if active_deals.is_some()
        && initial_market
            .as_ref()
            .is_some_and(|market| market.frame_model != context.frame_model)
    {
        let market = initial_market
            .as_ref()
            .expect("the model mismatch was just observed");
        bail!(
            "selected TokenContract {} belongs to model {}, not startup model {}",
            display_token_contract(&market.token_contract),
            market.frame_model,
            context.frame_model
        );
    }
    if let Some(initial_market) = initial_market {
        if (initial_market.price_per_tick, initial_market.max_ticks)
            != (
                u128::from(initial.cfg.price_per_tick),
                u128::from(initial.cfg.max_ticks),
            )
        {
            bail!(
                "initial market manifest terms ({},{}) do not match TokenContract.getDeal ({},{})",
                dexdo_core::shell_amount(initial_market.price_per_tick),
                initial_market.max_ticks,
                dexdo_core::shell_amount(initial.cfg.price_per_tick),
                initial.cfg.max_ticks
            );
        }
        initial.nonce = initial_market.nonce;
        initial.market = Some(initial_market);
    }
    if let Some(record) = initial_record.as_mut() {
        if let Some(previous) = adopt_run_gateway(&mut record.handle, context.gateway_advertise) {
            tracing::warn!(
                deal_handle = %record.path.display(),
                token_contract = %display_token_contract(&record.handle.token_contract),
                previous_gateway = %previous,
                gateway = %context.gateway_advertise,
                "seller deal handle re-bound to this service's gateway address"
            );
            deals::save_deal_handle(&deals_dir, &record.handle)?;
        }
    }
    let subscription = initial.cfg.subscription;
    let mut pool = vec![initial];

    for (key, mut record) in records {
        let market = record.market.clone();
        if let Some(active_deals) = active_deals {
            let foreign_model = market.frame_model != context.frame_model;
            if foreign_model && !active_deals.contains(&key) {
                tracing::warn!(
                    token_contract = %display_token_contract(&market.token_contract),
                    deal_model = %market.frame_model,
                    startup_model = %context.frame_model,
                    "seller startup skipped non-active deal for another model"
                );
                continue;
            }
            if foreign_model {
                tracing::warn!(
                    token_contract = %display_token_contract(&market.token_contract),
                    deal_model = %market.frame_model,
                    startup_model = %context.frame_model,
                    "seller startup retained an active buyer obligation from another model"
                );
            }
        }
        if let Some(previous) = adopt_run_gateway(&mut record.handle, context.gateway_advertise) {
            tracing::warn!(
                deal_handle = %record.path.display(),
                token_contract = %display_token_contract(&record.handle.token_contract),
                previous_gateway = %previous,
                gateway = %context.gateway_advertise,
                "seller deal handle re-bound to this service's gateway address"
            );
            deals::save_deal_handle(&deals_dir, &record.handle)?;
        }
        let price_per_tick = u64::try_from(market.price_per_tick).map_err(|_| {
            anyhow::anyhow!(
                "seller market {} price {} exceeds u64",
                display_token_contract(&market.token_contract),
                market.price_per_tick
            )
        })?;
        let max_ticks = u64::try_from(market.max_ticks).map_err(|_| {
            anyhow::anyhow!(
                "seller market {} max_ticks {} exceeds u64",
                display_token_contract(&market.token_contract),
                market.max_ticks
            )
        })?;
        let (chain, upstream) = backend_for_market(&market)?;
        let terms = match chain.sell_offer_terms(&market.token_contract).await {
            Ok(Some(terms)) => terms,
            unavailable => {
                let fill = dexdo::seller::read_seller_fill_lineage(
                    &seller_watch_cursor_path(context.deals_dir, &market.token_contract)?,
                    &market.token_contract,
                )?;
                if fill
                    .as_ref()
                    .and_then(|fill| fill.replacement_token_contract.as_ref())
                    .is_some()
                {
                    continue;
                }
                match unavailable {
                    Ok(None) if market.network == "mock" => (price_per_tick, max_ticks),
                    Ok(None) if fill.is_some() => {
                        let fill = fill.expect("the persisted seller fill was just observed");
                        if fill.residual_ticks == 0 {
                            println!(
                                "seller_residual_not_queued token_contract={} order_id={} offered_ticks={} matched_ticks={} residual_ticks=0 reason=fully_matched",
                                display_token_contract(&market.token_contract),
                                fill.order_id,
                                fill.offered_ticks,
                                fill.matched_ticks,
                            );
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        if u128::from(fill.residual_ticks) < dexdo_core::MIN_STREAM_BUY_TICKS {
                            println!(
                                "seller_residual_not_posted token_contract={} offered_ticks={} matched_ticks={} residual_ticks={} reason=below_contract_minimum",
                                display_token_contract(&market.token_contract),
                                fill.offered_ticks,
                                fill.matched_ticks,
                                fill.residual_ticks,
                            );
                            let _ = std::io::stdout().flush();
                            continue;
                        }
                        // The terminal parent is retained only long enough for `run_seller_pool` to
                        // put it on PR1055's existing residual queue. The handle terms are checked
                        // against the validated lineage again at the provision boundary.
                        (price_per_tick, max_ticks)
                    }
                    // the handle outlived its TokenContract. A TC that answers no `getDeal`
                    // has self-destructed, and that is the ORDINARY end of a completed deal, not a
                    // corruption: the deal is finished, the contract is gone, and the handle is
                    // residue left in this deals directory by an earlier run. There is nothing to
                    // serve and nothing at risk, so the deal is dropped from the pool exactly as a
                    // handle with a recorded replacement is dropped above. Refusing to start over
                    // it made ordinary residue an outage for every OTHER deal in the directory,
                    // including the one this run was invoked for - the pool loader runs before any
                    // of them is served. Only this run's own deal is a hard requirement, and it is
                    // never reached here: it was removed from `markets` before this loop.
                    Ok(None) => {
                        tracing::warn!(
                            token_contract = %display_token_contract(&market.token_contract),
                            "seller deal handle skipped: its TokenContract no longer answers getDeal, so the deal has ended and the contract is gone"
                        );
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                    Ok(Some(_)) => unreachable!(),
                }
            }
        };
        if terms != (price_per_tick, max_ticks) {
            bail!(
                "seller market {} terms ({},{max_ticks}) do not match TokenContract.getDeal ({},{})",
                display_token_contract(&market.token_contract),
                dexdo_core::shell_amount(u128::from(price_per_tick)),
                dexdo_core::shell_amount(u128::from(terms.0)),
                terms.1
            );
        }
        let cfg = dexdo::seller::SellerConfig {
            token_contract: market.token_contract.clone(),
            price_per_tick,
            max_ticks,
            subscription,
            gateway_advertise: context.gateway_advertise.to_string(),
            mock_token_count,
        };
        pool.push(SellerPoolDeal {
            watch: dexdo::seller::SellerMatchWatchConfig {
                cursor_path: seller_watch_cursor_path(context.deals_dir, &market.token_contract)?,
                poll_interval: DEFAULT_MATCH_POLL_INTERVAL,
            },
            chain,
            cfg,
            upstream,
            nonce: market.nonce,
            market: Some(market),
        });
    }
    Ok(pool)
}

/// what the startup admission decided, both halves of it.

/// The refusals used to be a `tracing::error!` and nothing else, so the run carried on and spent
/// the very capacity it had just told the operator it did not have. On mainnet (2026-08-16,
/// contracts 4.0.35) the seller declined the `TokenContract` `provision` had deployed and funded
/// one command earlier and then deployed and funded a replacement for a settled deal instead: two
/// 16-SHELL deposits out of one note for one deal. A refusal is a fact the spending path has to
/// know, so it is carried rather than logged and dropped.
struct SellerStartupAdmission {
    /// The deals this startup retained and will supervise.
    admitted: Vec<SellerPoolDeal>,
    /// Rendered `TokenContract`s refused for lack of `seller.max_open_deals` capacity.
    refused: Vec<String>,
}

async fn admit_seller_startup_deals(
    deals: Vec<SellerPoolDeal>,
    context: &SellerPoolContext<'_>,
    seller_policy: &policy::SellerRuntimePolicy,
) -> Result<SellerStartupAdmission> {
    let max_open_deals = usize::try_from(seller_policy.max_open_deals).unwrap_or(usize::MAX);
    let mut incumbents = Vec::new();
    let mut new_deals = Vec::new();
    for deal in deals {
        let fill_observed = dexdo::seller::read_seller_fill_lineage(
            &deal.watch.cursor_path,
            &deal.cfg.token_contract,
        )?
        .is_some();
        let inspection = dexdo::seller::inspect_seller_offer(
            deal.chain.as_ref(),
            &deal.cfg,
            Some(context.note_addr),
        )
        .await?;
        if fill_observed || !matches!(inspection, dexdo::seller::SellerOfferInspection::Vacant) {
            incumbents.push(deal);
        } else {
            new_deals.push(deal);
        }
    }

    let mut current_open_deals = incumbents.len();
    let mut refused = Vec::new();
    for deal in new_deals {
        if current_open_deals < max_open_deals {
            current_open_deals += 1;
            incumbents.push(deal);
            continue;
        }
        let frame_model = deal
            .market
            .as_ref()
            .map(|market| market.frame_model.as_str())
            .unwrap_or(context.frame_model);
        tracing::error!(
            token_contract = %display_token_contract(&deal.cfg.token_contract),
            frame_model,
            current_open_deals,
            max_open_deals,
            "seller startup did not take deal at max_open_deals"
        );
        // the refusal used to end at that log line. It is kept so the one path that can
        // spend -- residual provisioning in `run_seller_pool` -- can refuse to buy a successor with
        // capacity this startup has just denied.
        refused.push(display_token_contract(&deal.cfg.token_contract));
    }
    Ok(SellerStartupAdmission {
        admitted: incumbents,
        refused,
    })
}

/// `shutdown_requested` is the seller's RECORD that the operator's stop has been observed;
/// it is the same flag `run_seller` already keeps, threaded in so this function can both read it
/// and write to it.

/// It cannot be replaced by polling `shutdown`. That future is a `Fuse`: once its inner future has
/// completed, `Fuse::poll` returns `Poll::Pending` forever by design, so `select!` can keep polling
/// it and ask `is_terminated()` instead. Polling a CONSUMED `Fuse` is therefore byte-for-byte
/// indistinguishable from "the signal has not fired yet", and this function is one of the places
/// that consumes it -- `prepare_seller_offer_with_liveness` below selects on it. Without the record,
/// the disposition of that consumed signal is lost and the pool starts the next deal (a bond, an
/// on-chain `postSellOffer`) as if the operator had never asked it to stop.
/// re-review, finding 3: how a pool deal's start ENDED, with the ask's fate attached.

/// `prepare_pool_deal` flattened every stop into `Err`, and `SellerStartupOutcome::Stopped` carries
/// an `identity` that the `..` pattern threw away. So a start that stopped without proving the ask
/// off the book reached the pool as a bare error: it unregistered the stream and moved on, with no
/// identity in `resting` and no watcher -- the ask could still be crossed and nobody was left to
/// notice or to sweep it.
enum PoolDealStart {
    /// The deal is ready to be watched; the option is its resting identity, if it has one.
    Watching(Option<dexdo::seller::liveness::RestingOfferIdentity>),
    /// The start stopped and the ask is NOT proven off the book. The error is still the operator's
    /// to see, but the identity travels with it so the pool can keep answering for the deal.

    /// r4: `reason` and `disposition` travel too. Without them the startup path could not ASK
    /// `should_rearm_watcher` and had to decide for itself -- which is the second opinion this whole
    /// change exists to remove, reintroduced at the one site that had not yet been wired to it.
    StoppedUnproven {
        identity: Option<dexdo::seller::liveness::RestingOfferIdentity>,
        reason: dexdo::seller::liveness::RestingStopReason,
        disposition: dexdo::seller::liveness::CancellationDisposition,
        error: anyhow::Error,
    },
}

/// finding 3: does a stopped START still oblige the pool to answer for the deal?

/// Named, so the startup path cannot hold a different opinion from the running path and the sweep.
/// All three ask `proven_unmatchable`; this is the startup's spelling of that one question, and
/// having it as a function is what makes the branch drivable from a test at all.
fn startup_stop_keeps_the_deal(
    disposition: &dexdo::seller::liveness::CancellationDisposition,
) -> bool {
    !disposition.proven_unmatchable()
}

/// r4: what the pool must DO when a start stopped without proving its ask gone.

/// Named for the same reason `should_rearm_watcher` is named, and it delegates to it rather than
/// deciding again: the fourth review found this branch seating the deal and never re-arming, which
/// is the running path's old defect reappearing at the one site that had not been wired to the
/// shared question. A branch that decides for itself is a second opinion waiting to drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnprovenStartAction {
    /// Seat the deal so the sweep can find it, and put it back under a watcher when it is owed one.
    Keep { rearm: bool },
    /// There is no identity, so there is nothing to sweep. Watching is a CHOICE, and this is the
    /// reasoning for it -- twice already this comment carried a false premise instead.

    /// A watcher could be armed: `watch_pool_deal` takes an `Option`, and its `None` arm falls
    /// through to `wait_for_match`. (Earlier revisions of this note said otherwise, and then said
    /// the SELL "never landed". Both were wrong.)

    /// WHAT IS ACTUALLY KNOWN: nothing. The only producer of `identity: None` is
    /// `liveness.rs:1762-1785`, and it says so itself -- "interrupted fresh SELL has no terminal
    /// authoritative fact", `known_result` beginning `fresh_sell_submit=unresolved`, and a
    /// disposition of `UnknownFailure`, whose own doc is "nothing established either way, which is
    /// not the same as 'gone'". Its `operator_action` tells the operator to LIST this note's resting
    /// orders and cancel by hand, which only makes sense if the order may well be resting. The
    /// submit is abandoned on a timeout, so the SELL can also land after we stopped looking.

    /// SO IT IS A CHOICE BETWEEN TWO RISKS, not a deduction from a fact:

    /// * arm a watcher -- with no identity it can only `wait_for_match`, and that call takes no
    /// shutdown selector at all (the `Some` arm at least hands the supervisor a `pending()`), so
    /// on an order that never landed it never returns and hangs the shutdown drain that now
    /// awaits every watcher;
    /// * arm nothing -- and if the SELL did land, a buyer who crosses it is unserved.

    /// The buyer's risk is the one taken, because a hung drain is unrecoverable without killing the
    /// process while an unserved match is bounded by the ask's own deadline. The operator is told
    /// exactly what to check, by the message quoted above.

    /// The route goes back either way, as the isolated-failure branch releases it: leaving it
    /// registered leaks an upstream route for a deal nobody sweeps and nobody serves.
    Release,
}

fn plan_unproven_start(
    identity: Option<&dexdo::seller::liveness::RestingOfferIdentity>,
    reason: &dexdo::seller::liveness::RestingStopReason,
    disposition: &dexdo::seller::liveness::CancellationDisposition,
) -> UnprovenStartAction {
    if identity.is_none() {
        return UnprovenStartAction::Release;
    }
    UnprovenStartAction::Keep {
        rearm: should_rearm_watcher(&decide_after_stop(disposition), reason),
    }
}

/// The old entry point, unchanged for its existing callers: an unproven stop is still an `Err`.
async fn prepare_pool_deal<S>(
    seller: &dexdo::seller::RunningSeller,
    deal: &SellerPoolDeal,
    context: &SellerPoolContext<'_>,
    match_was_observed: bool,
    shutdown: Pin<&mut S>,
    shutdown_requested: &mut bool,
) -> Result<Option<dexdo::seller::liveness::RestingOfferIdentity>>
where
    S: futures::future::FusedFuture<Output = ()> + ?Sized,
{
    match prepare_pool_deal_reporting_identity(
        seller,
        deal,
        context,
        match_was_observed,
        shutdown,
        shutdown_requested,
    )
    .await?
    {
        PoolDealStart::Watching(identity) => Ok(identity),
        PoolDealStart::StoppedUnproven { error, .. } => Err(error),
    }
}

async fn prepare_pool_deal_reporting_identity<S>(
    seller: &dexdo::seller::RunningSeller,
    deal: &SellerPoolDeal,
    context: &SellerPoolContext<'_>,
    match_was_observed: bool,
    mut shutdown: Pin<&mut S>,
    shutdown_requested: &mut bool,
) -> Result<PoolDealStart>
where
    S: futures::future::FusedFuture<Output = ()> + ?Sized,
{
    if *shutdown_requested {
        bail!(
            "seller pool refused to start {} after the operator shutdown was observed",
            display_token_contract(&deal.cfg.token_contract)
        );
    }
    seller
        .state
        .route_stream(&deal.cfg.token_contract, deal.upstream.clone());
    if match_was_observed {
        return Ok(PoolDealStart::Watching(None));
    }
    let inspection = dexdo::seller::inspect_seller_offer(
        deal.chain.as_ref(),
        &deal.cfg,
        Some(context.note_addr),
    )
    .await?;
    let inspected_identity = match inspection {
        dexdo::seller::SellerOfferInspection::Resting { order_id } => {
            Some(dexdo::seller::liveness::RestingOfferIdentity {
                owner_note: context.note_addr.to_string(),
                token_contract: deal.cfg.token_contract.clone(),
                order_id,
            })
        }
        dexdo::seller::SellerOfferInspection::Funded
        | dexdo::seller::SellerOfferInspection::Vacant => None,
    };
    let manual_recovery = context.recover_publication.is_some_and(|token_contract| {
        token_contract.eq_ignore_ascii_case(&deal.cfg.token_contract)
    });
    let startup = match if manual_recovery {
        dexdo::seller::liveness::prepare_seller_offer_with_audited_manual_recovery_liveness(
            seller,
            deal.chain.as_ref(),
            &deal.cfg,
            context.note_addr,
            inspected_identity.as_ref(),
            &deal.watch.cursor_path,
            context
                .recover_publication
                .expect("manual recovery was checked"),
            shutdown.as_mut(),
            context.advertise_probe,
        )
        .await
    } else {
        dexdo::seller::liveness::prepare_seller_offer_with_persisted_liveness(
            seller,
            deal.chain.as_ref(),
            &deal.cfg,
            context.note_addr,
            inspected_identity.as_ref(),
            &deal.watch.cursor_path,
            shutdown.as_mut(),
            context.advertise_probe,
        )
        .await
    }? {
        dexdo::seller::liveness::SellerStartupOutcome::Ready(startup) => {
            // shutdown can win the startup select, then an already-matched SELL turns the
            // result back into `Ready`. The completed `Fuse` is the retained witness that this
            // successful startup consumed the stop; copy it into the same record used below.
            if futures::future::FusedFuture::is_terminated(shutdown.as_ref().get_ref()) {
                *shutdown_requested = true;
            }
            startup
        }
        dexdo::seller::liveness::SellerStartupOutcome::Stopped {
            identity,
            reason,
            disposition,
        } => {
            // this stop was produced by consuming the shutdown, so record the disposition
            // here, including the `UnknownFailure` shape below, which consumed it just the same.
            // After this line the `Fuse` can no longer be asked; the flag is the only witness.
            if matches!(reason, dexdo::seller::liveness::RestingStopReason::Shutdown) {
                *shutdown_requested = true;
            }
            // finding 3: the ask's fate decides whether this is merely an error or an error
            // the pool must keep answering for. `proven_unmatchable` is the same question the running
            // path and the sweep ask; asking it here is what stops the three from disagreeing.
            if startup_stop_keeps_the_deal(&disposition) {
                let error = anyhow::anyhow!(
                    "seller pool startup stopped for {} without proving its ask off the book: \
                     reason={reason:?}; cancellation_disposition={disposition}",
                    display_token_contract(&deal.cfg.token_contract)
                );
                return Ok(PoolDealStart::StoppedUnproven {
                    identity,
                    reason,
                    disposition,
                    error,
                });
            }
            return match reason {
                dexdo::seller::liveness::RestingStopReason::Shutdown => Err(anyhow::anyhow!(
                    "seller pool startup interrupted by shutdown"
                )),
                reason => Err(anyhow::anyhow!(
                    "seller pool startup stopped for {}: reason={reason:?}; \
                     cancellation_disposition={disposition}",
                    display_token_contract(&deal.cfg.token_contract)
                )),
            };
        }
    };
    if matches!(
        &startup,
        dexdo::seller::SellerOfferStartup::Posted { outcome: Some(_) }
    ) {
        println!(
            "posting offer: {} ticks (= {} model tokens) at {} SHELL/tick",
            deal.cfg.max_ticks,
            (deal.cfg.max_ticks as u128).saturating_mul(DobParams::canonical().tick_size as u128),
            dexdo_core::shell_amount(u128::from(deal.cfg.price_per_tick)),
        );
    }
    crate::cli::progress::step("posting the offer");
    if let Some(announcement) = seller_offer_startup_line(&startup) {
        println!("{announcement}");
        let _ = std::io::stdout().flush();
    }
    let identity = match &startup {
        dexdo::seller::SellerOfferStartup::ResumedResting { order_id }
        | dexdo::seller::SellerOfferStartup::Posted {
            outcome: Some(SellOfferOutcome::Rested { order_id }),
        } => Some(dexdo::seller::liveness::RestingOfferIdentity {
            owner_note: context.note_addr.to_string(),
            token_contract: deal.cfg.token_contract.clone(),
            order_id: *order_id,
        }),
        dexdo::seller::SellerOfferStartup::ResumedFunded
        | dexdo::seller::SellerOfferStartup::Posted {
            outcome: Some(SellOfferOutcome::Matched),
        } => None,
        dexdo::seller::SellerOfferStartup::Posted { outcome: None } => {
            bail!(
                "seller offer outcome for TokenContract {} has no exact resting order id or match confirmation",
                display_token_contract(&deal.cfg.token_contract)
            );
        }
    };
    if let Some(ready) = seller_ready_line(
        &deal.cfg.token_contract,
        context.gateway_advertise,
        &seller.listen_addr.to_string(),
        &startup,
        identity.as_ref(),
    ) {
        println!("{ready}");
        let _ = std::io::stdout().flush();
        // From here the seller is waiting on somebody else, and the live line says so with its own
        // clock. Everything above stays as it was: those lines are what a caller parses.
        crate::cli::progress::step(match identity.as_ref() {
            Some(resting) => format!(
                "waiting for a buyer -- offer {} rested, gateway {}",
                resting.order_id, context.gateway_advertise
            ),
            None => "waiting for a buyer".to_string(),
        });
    }
    Ok(PoolDealStart::Watching(identity))
}

async fn watch_pool_deal(
    seller: &dexdo::seller::RunningSeller,
    deal: SellerPoolDeal,
    identity: Option<dexdo::seller::liveness::RestingOfferIdentity>,
    fill_tx: tokio::sync::mpsc::UnboundedSender<SellerPoolDeal>,
    advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy,
    // the pool's stop, as a signal EVERY watcher can observe.

    // The pool drains its watchers rather than dropping them, so a watcher that
    // cannot be told to stop makes the drain unbounded: the operator's signal reaches the pool, the
    // loop breaks, and then `watched.next().await` waits forever on a supervisor with no way out.
    // Measured live: the shutdown arm fired, and 591 further lines of chain traffic followed with no
    // terminal and no `stopping` event.

    // `watch` because there is one sender and N watchers, which is exactly its shape, and because it
    // is already this file's idiom for the same job (`liveness.rs` post_submitted/post_release).
    stop: tokio::sync::watch::Receiver<bool>,
) -> (
    SellerPoolDeal,
    // the identity as it stands WHEN THE WATCH ENDS, not as it was handed in. A relist
    // advances it to a successor, and the pool's sweep acts on whatever it holds -- so returning
    // the original would make the sweep prove the absence of a consumed predecessor while the
    // successor rests on unserved.
    Option<dexdo::seller::liveness::RestingOfferIdentity>,
    Result<dexdo::seller::liveness::RestingSellerOutcome>,
) {
    let mut identity = identity;
    let result = async {
        let matched = match identity.as_mut() {
            // a SELL's deadline is mandatory and capped at one hour, so a seller that is still
            // healthy when it arrives must reap its own expired ask and carry the deal's remaining
            // capacity into exactly one successor. Without this the process stays up, the residual
            // capacity leaves the book and nothing but an operator brings it back.
            Some(identity) => match dexdo::seller::liveness::supervise_resting_offer_with_relist(
                seller,
                deal.chain.as_ref(),
                &deal.cfg,
                &deal.watch,
                identity,
                // was `futures::future::pending()`, which no signal could ever complete.
                async move {
                    let mut stop = stop;
                    // Errs only if the pool is gone, which is itself a stop.
                    let _ = stop.wait_for(|stopped| *stopped).await;
                },
                false,
                advertise_probe,
            )
            .await?
            {
                dexdo::seller::liveness::RestingSellerOutcome::Matched(matched) => matched,
                stopped @ dexdo::seller::liveness::RestingSellerOutcome::Stopped { .. } => {
                    return Ok(stopped);
                }
            },
            None => {
                dexdo::seller::wait_for_match(seller, deal.chain.as_ref(), &deal.cfg, &deal.watch)
                    .await?
            }
        };
        fill_tx.send(deal.clone()).map_err(|_| {
            anyhow::anyhow!(
                "seller pool stopped before recording fill for {}",
                display_token_contract(&deal.cfg.token_contract)
            )
        })?;
        dexdo::seller::serve_watched_match(
            seller,
            deal.chain.as_ref(),
            &deal.cfg,
            &deal.watch,
            matched,
        )
        .await
        .map(dexdo::seller::liveness::RestingSellerOutcome::Matched)
    }
    .await;
    (deal, identity, result)
}

/// example 2: an owner fill the pool cannot account for.

/// This finding used to be recorded into `first_error` and, because the owner-fill audit runs
/// BEFORE deal startup, it won the `get_or_insert` race and printed as the process `Error:` while
/// the real root cause (a readiness failure) was only logged. It is now a *cascade note*: it is
/// attached under `secondary` when anything else failed, and is only the reported error when
/// nothing else did.
fn unknown_owner_fill_note(token_contract: &str) -> dexdo_core::DexdoError {
    dexdo_core::DexdoError::new(
        dexdo_core::error_codes::E_POOL_UNKNOWN_OWNER_FILL,
        format!(
            "seller owner fill for TokenContract {} has no same-note deal handle/manifest; \
             refusing to discard unknown capacity",
            display_token_contract(token_contract)
        ),
    )
    .with_hint(
        "an \"owner fill\" is a match against THIS note's own resting order; without that deal's \
         handle/market.json the pool cannot account the capacity it just sold. Run the seller from \
         the directory holding that deal's handle, or close the orphaned deal (`dexdo deals`, then \
         `destroy`/`recover`). Attached as `secondary`, it is a CONSEQUENCE of the primary error \
         above -- fix that first and re-run",
    )
}

/// A same-note owner fill without a local handle is safe to skip only when the deal itself proves
/// that it is over. `deal_state` is the strict authoritative `TokenContract.getState()` read:
/// absence means the terminal self-destruct removed the account, while `is_stopped()` is the
/// retained terminal shape with both deposit and probe escrow drained. An active or unreadable
/// state stays on the existing fail-closed path and returns the exact error unchanged.
async fn unaccounted_owner_fill_note(
    chain: &dyn dexdo_core::ChainBackend,
    token_contract: &dexdo_core::TokenContract,
) -> Option<dexdo_core::DexdoError> {
    match chain.deal_state(token_contract).await {
        Ok(None) => {
            tracing::warn!(
                token_contract = %display_token_contract(token_contract),
                "seller owner fill skipped: TokenContract no longer answers getState, so its settled deal is closed and gone"
            );
            None
        }
        Ok(Some(state)) if state.is_stopped() => {
            tracing::warn!(
                token_contract = %display_token_contract(token_contract),
                tokens_final = state.tokens_final,
                "seller owner fill skipped: TokenContract getState proves the deal is stopped and its escrow is drained"
            );
            None
        }
        Ok(Some(_)) => Some(unknown_owner_fill_note(token_contract)),
        Err(error) => {
            tracing::warn!(
                token_contract = %display_token_contract(token_contract),
                %error,
                "seller owner fill terminal check failed; keeping the unaccounted fill fatal"
            );
            Some(unknown_owner_fill_note(token_contract))
        }
    }
}

/// (issue example 2): the reported process error must be the ROOT cause. Consequence findings
/// are attached under `secondary` instead of replacing it.
fn attach_cascade_notes(
    primary: anyhow::Error,
    notes: Vec<dexdo_core::DexdoError>,
) -> anyhow::Error {
    if notes.is_empty() {
        return primary;
    }
    // A primary that is already structured keeps its own code on the headline; anything else is
    // adopted (its message stays the headline, its source chain is preserved, not flattened).
    let structured = match primary.downcast::<dexdo_core::DexdoError>() {
        Ok(structured) => structured,
        Err(primary) => {
            dexdo_core::DexdoError::adopt(dexdo_core::error_codes::E_SELLER_POOL_FAILED, primary)
        }
    };
    anyhow::Error::new(notes.into_iter().fold(structured, |primary, note| {
        primary.with_secondary("pool owner-fill audit", note)
    }))
}

trait SellerPoolPolicyView {
    fn runtime_policy(&self) -> &policy::SellerRuntimePolicy;

    fn startup_max_open_deals(&self) -> u64 {
        self.runtime_policy().max_open_deals
    }

    /// the startup candidates this run refused for lack of `seller.max_open_deals` capacity.
    /// Empty for a bare runtime policy: a refusal is a fact about a startup, and only the startup
    /// view carries one.
    fn refused_startup_deals(&self) -> &[String] {
        &[]
    }
}

impl SellerPoolPolicyView for policy::SellerRuntimePolicy {
    fn runtime_policy(&self) -> &policy::SellerRuntimePolicy {
        self
    }
}

struct SellerStartupPolicy<'a> {
    runtime: &'a policy::SellerRuntimePolicy,
    retained_deals: usize,
    refused_startup_deals: Vec<String>,
}

impl SellerPoolPolicyView for SellerStartupPolicy<'_> {
    fn runtime_policy(&self) -> &policy::SellerRuntimePolicy {
        self.runtime
    }

    fn startup_max_open_deals(&self) -> u64 {
        self.runtime
            .max_open_deals
            .max(u64::try_from(self.retained_deals).unwrap_or(u64::MAX))
    }

    fn refused_startup_deals(&self) -> &[String] {
        &self.refused_startup_deals
    }
}

async fn run_seller_pool<S, F, Fut, P>(
    seller: &dexdo::seller::RunningSeller,
    deals: Vec<SellerPoolDeal>,
    context: SellerPoolContext<'_>,
    seller_policy: &P,
    provisioner: &mut F,
    mut shutdown: Pin<&mut S>,
    shutdown_requested: &mut bool,
) -> Result<()>
where
    S: futures::future::FusedFuture<Output = ()> + ?Sized,
    F: FnMut(String, u64, u64, u64) -> Fut,
    Fut: Future<
        Output = Result<(
            dexdo_core::MarketManifest,
            Arc<dyn dexdo_core::ChainBackend>,
        )>,
    >,
    P: SellerPoolPolicyView + ?Sized,
{
    // one sender held by the pool, one receiver per watcher. Set once, on the operator's
    // stop, so the unconditional drain below can complete instead of waiting on a supervisor that
    // was handed a future which never resolves.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let startup_max_open_deals =
        usize::try_from(seller_policy.startup_max_open_deals()).unwrap_or(usize::MAX);
    // read before the view is narrowed to its runtime half. The provisioning gate below is
    // the only place in this function that spends, and it is the place that has to know a candidate
    // was already turned away for lack of capacity.
    let refused_startup_deals = seller_policy.refused_startup_deals();
    let seller_policy = seller_policy.runtime_policy();
    let max_open_deals = usize::try_from(seller_policy.max_open_deals).unwrap_or(usize::MAX);
    if max_open_deals == 0 {
        bail!("seller.max_open_deals must be at least 1");
    }
    let (owner_fill_chain, primary_token_contract) = deals
        .first()
        .map(|deal| (deal.chain.clone(), deal.cfg.token_contract.clone()))
        .ok_or_else(|| anyhow::anyhow!("seller pool has no deals to supervise"))?;
    let mut owner_fill_cursor = MatchWatchCursor::new(0);
    let mut active = JoinSet::<SellerAdvanceResult>::new();
    let mut terminal_receipts = JoinSet::<SellerTerminalReceiptResult>::new();
    let mut watched = FuturesUnordered::new();
    let (fill_tx, mut fill_rx) = tokio::sync::mpsc::unbounded_channel();
    let (claim_delivery_tx, mut claim_delivery_rx) =
        tokio::sync::mpsc::unbounded_channel::<ClaimDeliveryRuntimeEvent>();
    let mut resting = std::collections::HashMap::new();
    let mut pending = std::collections::VecDeque::<SellerPoolDeal>::new();
    let mut known_tcs = std::collections::HashSet::new();
    let mut known_nonces = std::collections::HashMap::new();
    let mut first_error = None;
    // findings that are consequences of another failure. They never win the `first_error`
    // race; they are attached to whatever the primary error turns out to be.
    let mut cascade_notes: Vec<dexdo_core::DexdoError> = Vec::new();
    let mut noted_owner_fills = std::collections::HashSet::new();
    let mut candidates = Vec::new();

    for deal in deals {
        let normalized = deals::normalize_addr(&deal.cfg.token_contract);
        if !known_tcs.insert(normalized) {
            bail!(
                "seller pool has duplicate TokenContract {}",
                display_token_contract(&deal.cfg.token_contract)
            );
        }
        if let Some((tc, price, ticks)) = known_nonces.insert(
            deal.nonce,
            (
                deal.cfg.token_contract.clone(),
                deal.cfg.price_per_tick,
                deal.cfg.max_ticks,
            ),
        ) {
            bail!(
                "seller pool nonce {} maps to both TokenContract {} ({},{ticks}) and {} ({},{})",
                deal.nonce,
                display_token_contract(&tc),
                dexdo_core::shell_amount(price),
                display_token_contract(&deal.cfg.token_contract),
                dexdo_core::shell_amount(deal.cfg.price_per_tick),
                deal.cfg.max_ticks
            );
        }
        let fill = match dexdo::seller::read_seller_fill_lineage(
            &deal.watch.cursor_path,
            &deal.cfg.token_contract,
        ) {
            Ok(fill) => fill,
            Err(error) => {
                tracing::error!(
                    token_contract = %display_token_contract(&deal.cfg.token_contract),
                    %error,
                    "seller pool retired deal with invalid fill cursor"
                );
                first_error.get_or_insert(error);
                continue;
            }
        };
        let observed = fill.is_some();
        let state = match deal.chain.deal_state(&deal.cfg.token_contract).await {
            Ok(state) => state,
            Err(error) => {
                tracing::error!(
                    token_contract = %display_token_contract(&deal.cfg.token_contract),
                    %error,
                    "seller pool isolated deal state read failure"
                );
                first_error.get_or_insert_with(|| {
                    anyhow::anyhow!(
                        "seller deal {} state read failed: {error}",
                        display_token_contract(&deal.cfg.token_contract)
                    )
                });
                continue;
            }
        };
        if state.is_none() && observed {
            let fill = fill.expect("the terminal seller fill was just observed");
            if fill.residual_ticks == 0 {
                println!(
                    "seller_residual_not_queued token_contract={} order_id={} offered_ticks={} matched_ticks={} residual_ticks=0 reason=fully_matched",
                    display_token_contract(&deal.cfg.token_contract),
                    fill.order_id,
                    fill.offered_ticks,
                    fill.matched_ticks,
                );
            } else if u128::from(fill.residual_ticks) < dexdo_core::MIN_STREAM_BUY_TICKS {
                println!(
                    "seller_residual_not_posted token_contract={} offered_ticks={} matched_ticks={} residual_ticks={} reason=below_contract_minimum",
                    display_token_contract(&deal.cfg.token_contract),
                    fill.offered_ticks,
                    fill.matched_ticks,
                    fill.residual_ticks,
                );
            } else {
                println!(
                    "seller_residual_queued token_contract={} order_id={} offered_ticks={} matched_ticks={} residual_ticks={} price_per_tick={} reason=restart_after_parent_settlement",
                    display_token_contract(&deal.cfg.token_contract),
                    fill.order_id,
                    fill.offered_ticks,
                    fill.matched_ticks,
                    fill.residual_ticks,
                    dexdo_core::shell_amount(u128::from(fill.price_per_tick)),
                );
                pending.push_back(deal);
            }
            let _ = std::io::stdout().flush();
            continue;
        }
        candidates.push((deal, observed));
    }
    if candidates.len() > startup_max_open_deals {
        bail!(
            "seller pool has {} active/resting deals, exceeding policy seller.max_open_deals={max_open_deals}",
            candidates.len()
        );
    }
    match owner_fill_chain
        .poll_seller_fills(seller.note.as_ref(), &mut owner_fill_cursor)
        .await
    {
        Ok(fills) => {
            for fill in fills {
                if known_tcs.contains(&deals::normalize_addr(&fill.token_contract))
                    || !noted_owner_fills.insert(deals::normalize_addr(&fill.token_contract))
                {
                    continue;
                }
                if let Some(note) =
                    unaccounted_owner_fill_note(owner_fill_chain.as_ref(), &fill.token_contract)
                        .await
                {
                    cascade_notes.push(note);
                    break;
                }
            }
        }
        Err(error) => {
            tracing::warn!(%error, "seller owner fill discovery failed; retrying");
            first_error.get_or_insert_with(|| {
                anyhow::anyhow!("seller owner fill discovery failed: {error}")
            });
        }
    }
    for (deal, observed) in candidates {
        if let Err(error) = save_pool_deal_handle(&context, &deal) {
            first_error.get_or_insert(error);
            continue;
        }
        let identity = match prepare_pool_deal_reporting_identity(
            seller,
            &deal,
            &context,
            observed,
            shutdown.as_mut(),
            shutdown_requested,
        )
        .await
        {
            Ok(PoolDealStart::Watching(identity)) => identity,
            // finding 3: the start stopped and the ask is NOT proven off the book. Retiring
            // here is what left an executable ask with nobody watching it and nothing in `resting`
            // for the sweep to find. The error is still reported; the deal is still answered for.
            Ok(PoolDealStart::StoppedUnproven {
                identity,
                reason,
                disposition,
                error,
            }) => {
                first_error.get_or_insert(error);
                // r4, point 1: ask the SAME question the running path asks, and act on the
                // same answer. Keeping the bookkeeping and the gateway route was never enough --
                // with no watcher, a buyer who crosses this still-live ask is unserved whatever the
                // sweep later concludes. `should_rearm_watcher` exists so these two paths cannot
                // hold different opinions; until now only one of them consulted it.
                let action = plan_unproven_start(identity.as_ref(), &reason, &disposition);
                tracing::error!(
                    token_contract = %display_token_contract(&deal.cfg.token_contract),
                    ?reason,
                    %disposition,
                    action = ?action,
                    "seller pool start stopped without proving its ask off the book"
                );
                match (action, identity) {
                    (UnprovenStartAction::Keep { rearm }, Some(identity)) => {
                        resting.insert(
                            deal.cfg.token_contract.clone(),
                            (deal.clone(), identity.clone()),
                        );
                        if rearm {
                            watched.push(watch_pool_deal(
                                seller,
                                deal.clone(),
                                Some(identity),
                                fill_tx.clone(),
                                context.advertise_probe,
                                stop_rx.clone(),
                            ));
                        }
                    }
                    _ => {
                        seller.state.unregister_stream(&deal.cfg.token_contract);
                    }
                }
                continue;
            }
            Err(error) => {
                tracing::error!(
                    token_contract = %display_token_contract(&deal.cfg.token_contract),
                    %error,
                    "seller pool isolated deal startup failure"
                );
                seller.state.unregister_stream(&deal.cfg.token_contract);
                first_error.get_or_insert(error);
                continue;
            }
        };
        if let Some(identity) = identity.as_ref() {
            resting.insert(
                deal.cfg.token_contract.clone(),
                (deal.clone(), identity.clone()),
            );
        }
        watched.push(watch_pool_deal(
            seller,
            deal,
            identity,
            fill_tx.clone(),
            context.advertise_probe,
            stop_rx.clone(),
        ));
    }

    let mut gateway_poll =
        tokio::time::interval(SellerLivenessParams::canonical().gateway_task_poll);
    gateway_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut owner_fill_poll = tokio::time::interval(DEFAULT_MATCH_POLL_INTERVAL);
    owner_fill_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stop_error = None;
    let mut stopped_by_operator = false;

    'pool: loop {
        // the recorded disposition, not the signal. A shutdown consumed by a deal startup
        // above leaves the `Fuse` permanently `Pending`, so the `select!` arm below can never fire
        // again and a `poll!` here could never tell that apart from "no signal yet". The record can:
        // it is why this loop still stops, and it is read before the provisioning block so no
        // successor TokenContract is deployed after the operator asked the seller to stop.
        if *shutdown_requested {
            stopped_by_operator = true;
            break 'pool;
        }
        while watched.len() + active.len() < max_open_deals {
            if pending.is_empty() {
                break;
            }
            // this run has ALREADY told the operator it cannot take a deal -- on mainnet the
            // very TokenContract `provision` had deployed and funded one command earlier, named in
            // the `market.json` the buyer is handed. Deploying and funding a successor for some
            // OTHER deal spends exactly the capacity that refusal denied: the note paid two
            // 16-SHELL deposits for one deal, and the manifest kept pointing at the contract nobody
            // serves. A refusal must not spend.

            // Fail closed here rather than at the refusal itself: a refusal that costs nothing is
            // ordinary and correct (an at-limit seller keeps serving its live incumbent and simply
            // leaves the new candidate alone), and only the step that would buy a contract is the
            // step that must stop. That is the shape `assert_token_contract_fresh`
            // (`crates/core/src/chain/mod.rs`) already uses: refuse with an action the operator can
            // take, never deploy around the refusal. `break 'pool` rather than a bare return so the
            // exit path below still cancels every ask this run rests.
            if !refused_startup_deals.is_empty() {
                let queued = pending
                    .front()
                    .map(|deal| display_token_contract(&deal.cfg.token_contract))
                    .unwrap_or_else(|| "a queued residual".to_string());
                stop_error = Some(anyhow::anyhow!(
                    "seller refused {} at seller.max_open_deals={max_open_deals} and will not fund \
                     a replacement TokenContract for {queued} instead: a refusal must not spend. \
                     Free capacity first -- finish or retire the deal still holding the slot, or \
                     raise seller.max_open_deals above {max_open_deals} -- then restart. The \
                     refused TokenContract was not touched and is still serviceable.",
                    refused_startup_deals.join(", ")
                ));
                break 'pool;
            }
            // provisioning deploys and funds a fresh TokenContract. Honor the recorded
            // shutdown or observe a newly-ready signal before taking the residual from `pending`
            // and before any durable lineage write, so an operator stop cannot spend money on a
            // successor the process will not use.
            if *shutdown_requested || futures::poll!(shutdown.as_mut()).is_ready() {
                *shutdown_requested = true;
                stopped_by_operator = true;
                tracing::warn!(
                    pending_residuals = pending.len(),
                    reason = "operator_shutdown",
                    "seller pool left residuals unprovisioned because shutdown was requested"
                );
                break 'pool;
            }
            let Some(current) = pending.pop_front() else {
                break;
            };
            let replacement_result: Result<()> = async {
                let fill = dexdo::seller::read_seller_fill_lineage(
                    &current.watch.cursor_path,
                    &current.cfg.token_contract,
                )?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "seller match for {} has no authoritative owner fill lineage; refusing to guess residual capacity",
                        display_token_contract(&current.cfg.token_contract)
                    )
                })?;
                // Same 4.0.33 terminal read as `plan_opened_deal`: `None` here is the filled deal's
                // TokenContract already gone, not an unreadable one. Every other unreadable answer
                // stays fatal.

                // leg 4: this read is a CROSS-CHECK, never the source. The authoritative terms
                // came from this same getter at match discovery and were persisted with the fill
                // (`SellerMatchWatchCursor::record_fill`) while the contract was still readable.
                // Retiring the pending entry when the getter is gone dropped capacity the parent
                // never held: the order book consumes a SELL slot whole on any match ("SELL offer =
                // one-deal slot -> consumed on match (taker BUY), even on partial",
                // `InferenceOrderBook._match`), so the unmatched ticks rest nowhere and only a
                // successor puts them back. On 2026-08-04 the buyer stopped on the probe, the
                // `ProbeBurned` settlement destroyed the account, and 96 of 98 ticks left the book
                // with no successor and, after leg 3 kept the seller alive, no error either. Nor is
                // that one incident's edge: under the canonical `seller.max_open_deals = 1` the
                // pending entry waits for the parent's own slot, which frees only once the parent
                // has settled and paid out.

                // The one thing the terminal costs is the cross-check itself. It is replaced by the
                // local check it duplicated -- the handle this deal was loaded from must agree with
                // the lineage -- and the terms still come only from facts proven while the parent
                // answered. Anything that does not agree publishes nothing.
                match current
                    .chain
                    .sell_offer_terms(&current.cfg.token_contract)
                    .await?
                {
                    Some(terms) => {
                        if terms != (fill.price_per_tick, fill.offered_ticks) {
                            bail!(
                                "persisted seller fill for {} has N/P ({},{}) but TokenContract.getDeal is ({},{}); refusing residual provision",
                                display_token_contract(&current.cfg.token_contract),
                                fill.offered_ticks,
                                dexdo_core::shell_amount(fill.price_per_tick),
                                terms.1,
                                terms.0
                            );
                        }
                    }
                    None => {
                        if (current.cfg.price_per_tick, current.cfg.max_ticks)
                            != (fill.price_per_tick, fill.offered_ticks)
                        {
                            bail!(
                                "terminal parent {} has a deal handle of N/P ({},{}) but a persisted fill of ({},{}); refusing residual provision",
                                display_token_contract(&current.cfg.token_contract),
                                current.cfg.max_ticks,
                                dexdo_core::shell_amount(current.cfg.price_per_tick),
                                fill.offered_ticks,
                                dexdo_core::shell_amount(fill.price_per_tick)
                            );
                        }
                        // The operator's channel, not a log level nobody enabled: the ticks that
                        // are being carried, and why their terms did not come from the parent.
                        println!(
                            "seller_residual_terms_from_lineage token_contract={} order_id={} offered_ticks={} matched_ticks={} residual_ticks={} price_per_tick={} reason=parent_settled_and_gone",
                            display_token_contract(&current.cfg.token_contract),
                            fill.order_id,
                            fill.offered_ticks,
                            fill.matched_ticks,
                            fill.residual_ticks,
                            dexdo_core::shell_amount(u128::from(fill.price_per_tick))
                        );
                        let _ = std::io::stdout().flush();
                    }
                }
                let next_nonce = match fill.replacement_nonce {
                    Some(nonce) => nonce,
                    None => {
                        let mut nonce = current.nonce.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("seller residual nonce overflow at {}", current.nonce)
                        })?;
                        while known_nonces.contains_key(&nonce) {
                            nonce = nonce.checked_add(1).ok_or_else(|| {
                                anyhow::anyhow!("seller residual nonce overflow at {nonce}")
                            })?;
                        }
                        dexdo::seller::persist_seller_replacement(
                            &current.watch.cursor_path,
                            &current.cfg.token_contract,
                            nonce,
                            None,
                        )?;
                        nonce
                    }
                };
                if let Some((tc, price, ticks)) = known_nonces.get(&next_nonce) {
                    let linked = fill.replacement_token_contract.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "seller residual nonce {next_nonce} is occupied by {}, but parent {} has no persisted replacement link",
                            display_token_contract(tc),
                            display_token_contract(&current.cfg.token_contract)
                        )
                    })?;
                    if !tc.eq_ignore_ascii_case(linked)
                        || (*price, *ticks)
                            != (current.cfg.price_per_tick, fill.residual_ticks)
                    {
                        bail!(
                            "seller residual nonce {next_nonce} links to {}, but known deal is {} with ({price},{ticks})",
                            display_token_contract(linked),
                            display_token_contract(tc)
                        );
                    }
                    return Ok(());
                }
                let frame_model = current
                    .market
                    .as_ref()
                    .map(|market| market.frame_model.clone())
                    .unwrap_or_else(|| context.frame_model.to_string());
                let (market, chain) = provisioner(
                    frame_model.clone(),
                    next_nonce,
                    current.cfg.price_per_tick,
                    fill.residual_ticks,
                )
                .await?;
                market.validate().map_err(|error| {
                    anyhow::anyhow!("residual market manifest is invalid: {error}")
                })?;
                assert_market_seller_note(&market.seller_note, context.note_addr)?;
                if market.nonce != next_nonce
                    || market.frame_model != frame_model
                    || market.price_per_tick != u128::from(current.cfg.price_per_tick)
                    || market.max_ticks != u128::from(fill.residual_ticks)
                {
                    bail!(
                        "residual provision returned inconsistent market: expected frame_model={} nonce={} \
                         price={} max_ticks={}, got frame_model={} nonce={} price={} max_ticks={}",
                        frame_model,
                        next_nonce,
                        dexdo_core::shell_amount(current.cfg.price_per_tick),
                        fill.residual_ticks,
                        market.frame_model,
                        market.nonce,
                        dexdo_core::shell_amount(market.price_per_tick),
                        market.max_ticks
                    );
                }
                if let Some(linked) = fill.replacement_token_contract.as_deref() {
                    if !linked.eq_ignore_ascii_case(&market.token_contract) {
                        bail!(
                            "seller replacement nonce {next_nonce} returned {}, but cursor links {}",
                            display_token_contract(&market.token_contract),
                            display_token_contract(linked)
                        );
                    }
                }
                dexdo::seller::persist_seller_replacement(
                    &current.watch.cursor_path,
                    &current.cfg.token_contract,
                    next_nonce,
                    Some(&market.token_contract),
                )?;
                let (authoritative_price, authoritative_ticks) =
                    match chain.sell_offer_terms(&market.token_contract).await? {
                        Some(terms) => terms,
                        None if market.network == "mock" => {
                            (current.cfg.price_per_tick, fill.residual_ticks)
                        }
                        None => {
                            bail!(
                                "residual TokenContract {} getDeal is unavailable after provision",
                                display_token_contract(&market.token_contract)
                            )
                        }
                    };
                if (authoritative_price, authoritative_ticks)
                    != (current.cfg.price_per_tick, fill.residual_ticks)
                {
                    bail!(
                        "residual TokenContract {} getDeal ({},{authoritative_ticks}) \
                         does not match requested ({},{})",
                        display_token_contract(&market.token_contract),
                        dexdo_core::shell_amount(authoritative_price),
                        dexdo_core::shell_amount(current.cfg.price_per_tick),
                        fill.residual_ticks
                    );
                }
                let cfg = dexdo::seller::SellerConfig {
                    token_contract: market.token_contract.clone(),
                    price_per_tick: authoritative_price,
                    max_ticks: authoritative_ticks,
                    subscription: current.cfg.subscription,
                    gateway_advertise: current.cfg.gateway_advertise.clone(),
                    mock_token_count: current.cfg.mock_token_count,
                };
                let replacement = SellerPoolDeal {
                    watch: dexdo::seller::SellerMatchWatchConfig {
                        cursor_path: seller_watch_cursor_path(
                            context.deals_dir,
                            &cfg.token_contract,
                        )?,
                        poll_interval: DEFAULT_MATCH_POLL_INTERVAL,
                    },
                    chain,
                    cfg,
                    upstream: current.upstream.clone(),
                    nonce: next_nonce,
                    market: Some(market),
                };
                let normalized = deals::normalize_addr(&replacement.cfg.token_contract);
                if !known_tcs.insert(normalized) {
                    bail!(
                        "seller residual provision returned duplicate TokenContract {}",
                        display_token_contract(&replacement.cfg.token_contract)
                    );
                }
                known_nonces.insert(
                    next_nonce,
                    (
                        replacement.cfg.token_contract.clone(),
                        replacement.cfg.price_per_tick,
                        replacement.cfg.max_ticks,
                    ),
                );
                save_pool_deal_handle(&context, &replacement)?;
                let identity = prepare_pool_deal(
                    seller,
                    &replacement,
                    &context,
                    false,
                    shutdown.as_mut(),
                    shutdown_requested,
                )
                .await?;
                if let Some(identity) = identity.as_ref() {
                    resting.insert(
                        replacement.cfg.token_contract.clone(),
                        (replacement.clone(), identity.clone()),
                    );
                }
                watched.push(watch_pool_deal(
                    seller,
                    replacement,
                    identity,
                    fill_tx.clone(),
                    context.advertise_probe,
                    stop_rx.clone(),
                ));
                Ok(())
            }
            .await;
            if let Err(error) = replacement_result {
                tracing::error!(
                    token_contract = %display_token_contract(&current.cfg.token_contract),
                    %error,
                    "seller pool isolated residual provision failure"
                );
                first_error.get_or_insert(error);
            }
        }

        if watched.is_empty()
            && active.is_empty()
            && terminal_receipts.is_empty()
            && pending.is_empty()
        {
            break;
        }
        tokio::select! {
            biased;
            _ = shutdown.as_mut() => {
                *shutdown_requested = true;
                stopped_by_operator = true;
                // tell the watchers BEFORE breaking, so the unconditional drain below can
                // finish. Dropping them instead would lose the identity a relist advanced to
                // so the drain stays and the watchers are given a way out.
                let _ = stop_tx.send(true);
                break 'pool;
            }
            _ = owner_fill_poll.tick() => {
                match owner_fill_chain
                    .poll_seller_fills(seller.note.as_ref(), &mut owner_fill_cursor)
                    .await
                {
                    Ok(fills) => {
                        for fill in fills {
                            if known_tcs.contains(&deals::normalize_addr(&fill.token_contract))
                                || !noted_owner_fills
                                    .insert(deals::normalize_addr(&fill.token_contract))
                            {
                                continue;
                            }
                            if let Some(note) = unaccounted_owner_fill_note(
                                owner_fill_chain.as_ref(),
                                &fill.token_contract,
                            )
                            .await
                            {
                                tracing::error!(
                                    error = %note,
                                    "seller pool isolated unknown owner fill"
                                );
                                cascade_notes.push(note);
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "seller owner fill discovery failed; retrying");
                        first_error.get_or_insert_with(|| {
                            anyhow::anyhow!("seller owner fill discovery failed: {error}")
                        });
                    }
                }
            }
            filled = fill_rx.recv() => {
                let Some(deal) = filled else {
                    continue;
                };
                match dexdo::seller::read_seller_fill_lineage(
                    &deal.watch.cursor_path,
                    &deal.cfg.token_contract,
                ) {
                    Ok(Some(fill)) if fill.residual_ticks >= 2 => pending.push_back(deal),
                    Ok(Some(fill)) if fill.residual_ticks == 1 => {
                        println!(
                            "seller_residual_not_posted token_contract={} offered_ticks={} matched_ticks={} residual_ticks=1 reason=below_contract_minimum",
                            display_token_contract(&deal.cfg.token_contract),
                            fill.offered_ticks,
                            fill.matched_ticks
                        );
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        first_error.get_or_insert_with(|| anyhow::anyhow!(
                            "seller match for {} has no authoritative owner fill lineage; refusing to guess residual capacity",
                            display_token_contract(&deal.cfg.token_contract)
                        ));
                    }
                    Err(error) => {
                        tracing::error!(
                            token_contract = %display_token_contract(&deal.cfg.token_contract),
                            %error,
                            "seller pool rejected corrupt fill lineage"
                        );
                        first_error.get_or_insert(error);
                    }
                }
            }
            watched_result = watched.next(), if !watched.is_empty() => {
                let (deal, current_identity, outcome) =
                    watched_result.expect("watched branch is disabled when empty");
                // TAKEN, not discarded. Whether this deal may leave the resting set depends
                // on an outcome that has not been read yet, and dropping it here is what made the
                // shutdown sweep blind to the one case it exists for.
                let resting_entry = resting.remove(&deal.cfg.token_contract);
                let outcome = match outcome {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        tracing::error!(
                            token_contract = %display_token_contract(&deal.cfg.token_contract),
                            %error,
                            "seller pool isolated deal watch/open failure"
                        );
                        seller.state.unregister_stream(&deal.cfg.token_contract);
                        first_error.get_or_insert(error);
                        continue;
                    }
                };
                match outcome {
                    dexdo::seller::liveness::RestingSellerOutcome::Matched(matched) => {
                        println!(
                            "seller_match_opened token_contract={} gateway={} gateway_listen={} cursor={}",
                            dexdo_core::address::display_self_dapp(&matched.token_contract),
                            context.gateway_advertise,
                            seller.listen_addr,
                            deal.watch.cursor_path.display()
                        );
                        let _ = std::io::stdout().flush();
                        if let Err(error) = save_pool_deal_handle(&context, &deal) {
                            seller.state.unregister_stream(&deal.cfg.token_contract);
                            first_error.get_or_insert(error);
                            continue;
                        }
                        match apply_seller_dispute_policy(
                            deal.chain.as_ref(),
                            &deal.cfg.token_contract,
                            seller_policy,
                            "pre-advance",
                        ).await {
                            Ok(false) => {
                              let spawn_result: Result<bool> = async {
                                  let bounds = deal
                                      .chain
                                      .deal_claim_bounds(&deal.cfg.token_contract)
                                      .await
                                      .map_err(|error| {
                                          anyhow::anyhow!(
                                              "--token-contract {}: getConfig() claim bounds are \
                                               unreadable, refusing to start by-fact claiming on a \
                                               guessed cadence: {error}",
                                              display_token_contract(&deal.cfg.token_contract)
                                          )
                                      })?;
                                  let snapshot = match plan_opened_deal(
                                      deal.chain.as_ref(),
                                      &deal.cfg.token_contract,
                                  )
                                  .await?
                                  {
                                      OpenedDealPlan::Drive(snapshot) => *snapshot,
                                      OpenedDealPlan::RetireSettled => return Ok(false),
                                  };
                                  let run_subscription_keeper =
                                      snapshot.subscription.is_subscription();
                                  let windows =
                                      dexdo::seller::AdvanceWindows::from_bounds(bounds);
                                  let note = seller.note.clone();
                                  let delivery = seller.state.delivery(&deal.cfg.token_contract);
                                  let token_contract = deal.cfg.token_contract.clone();
                                  let chain = deal.chain.clone();
                                  let gateway = seller.state.clone();
                                  let tick_size = dexdo_core::DobParams::canonical().tick_size;
                                  let tick_budget = if run_subscription_keeper {
                                      u128::from(deal.cfg.max_ticks)
                                  } else {
                                      funded_tick_budget(
                                          &deal.cfg.token_contract,
                                          snapshot.subscription.funded_tokens,
                                          tick_size,
                                      )?
                                  };
                                  let claim_delivery_tx = claim_delivery_tx.clone();
                                  active.spawn(async move {
                                      let result = if !run_subscription_keeper {
                                          let observer = OrdinaryCapacityObserver {
                                              gateway,
                                              delivery: delivery.clone(),
                                              events: claim_delivery_tx.clone(),
                                          };
                                          dexdo::seller::drive_advance_with_observer(
                                              chain.as_ref(),
                                              &token_contract,
                                              note.as_ref(),
                                              windows,
                                              tick_budget,
                                              tick_size,
                                              true,
                                              delivery.count.clone(),
                                              delivery.done.clone(),
                                              &observer,
                                          )
                                          .await
                                      } else {
                                          let advance_observer = ClaimMeasurementObserver {
                                              gateway: gateway.clone(),
                                              delivery: delivery.clone(),
                                              events: claim_delivery_tx,
                                          };
                                          let advance = dexdo::seller::drive_advance_with_observer(
                                              chain.as_ref(),
                                              &token_contract,
                                              note.as_ref(),
                                              windows,
                                              tick_budget,
                                              tick_size,
                                              false,
                                              delivery.count.clone(),
                                              delivery.done.clone(),
                                              &advance_observer,
                                          );
                                          let observer =
                                              SubscriptionCapacityObserver { gateway };
                                          let keeper =
                                              dexdo::seller::drive_subscription_keeper_with_observer(
                                                  chain.as_ref(),
                                                  &token_contract,
                                                  bounds,
                                                  &observer,
                                          );
                                          tokio::pin!(advance);
                                          tokio::pin!(keeper);
                                          async {
                                              tokio::select! {
                                                  finalized = &mut keeper => finalized,
                                                  claimed = &mut advance => {
                                                      let claimed = claimed?;
                                                      let finalized = keeper.await?;
                                                      Ok(claimed.max(finalized))
                                                  }
                                              }
                                          }
                                          .await
                                      };
                                      (
                                          token_contract,
                                          chain,
                                          delivery,
                                          !run_subscription_keeper,
                                          result,
                                      )
                                  });
                                  Ok(true)
                              }
                              .await;
                              match spawn_result {
                                  Ok(true) => {}
                                  Ok(false) => {
                                      // The buyer settled inside the match->drive window and the
                                      // TokenContract selfdestructed with the settlement. One
                                      // finished deal must not take the rest of the pool with it.
                                      seller.state.unregister_stream(&deal.cfg.token_contract);
                                      tracing::warn!(
                                          token_contract = %display_token_contract(&deal.cfg.token_contract),
                                          "seller pool retired one deal that settled and selfdestructed before its settlement driver started"
                                      );
                                  }
                                  Err(error) => {
                                      seller.state.unregister_stream(&deal.cfg.token_contract);
                                      first_error.get_or_insert(error);
                                  }
                              }
                            }
                            Ok(true) => {
                                seller.state.unregister_stream(&deal.cfg.token_contract);
                            }
                            Err(error) => {
                                seller.state.unregister_stream(&deal.cfg.token_contract);
                                first_error.get_or_insert(error);
                            }
                        }
                    }
                    dexdo::seller::liveness::RestingSellerOutcome::Stopped { reason, disposition } => {
                        // two named decisions, no inline policy. `decide_after_stop` answers
                        // what is OWED; `should_rearm_watcher` answers who watches next.
                        let decision = decide_after_stop(&disposition);
                        match decision {
                            AfterStop::Retire => {
                                seller.state.unregister_stream(&deal.cfg.token_contract);
                            }
                            AfterStop::ServeMatch(_) => {}
                            AfterStop::Retain => {
                                // Re-seat on the identity the WATCH reported: a relist advances it,
                                // and the captured copy names a generation the book has consumed.
                                reseat_resting_after_stop(
                                    &deal.cfg.token_contract,
                                    current_identity.clone(),
                                    resting_entry,
                                    &mut resting,
                                );
                            }
                        }
                        let rearmed = should_rearm_watcher(&decision, &reason);
                        tracing::warn!(
                            token_contract = %display_token_contract(&deal.cfg.token_contract),
                            ?reason,
                            %disposition,
                            decision = ?decision,
                            rearmed,
                            "seller pool decided what a stopped resting deal still owes"
                        );
                        if rearmed {
                            watched.push(watch_pool_deal(
                                seller,
                                deal.clone(),
                                current_identity.clone(),
                                fill_tx.clone(),
                                context.advertise_probe,
                                stop_rx.clone(),
                            ));
                        }
                    }
                }
            }
            failure = seller.state.recv_upstream_failure() => {
                if let Some(failure) = failure {
                    emit_upstream_failure(failure);
                }
            }
            measurement = claim_delivery_rx.recv() => {
                if let Some(measurement) = measurement {
                    emit_claim_delivery_measurement(measurement);
                }
            }
            joined = active.join_next(), if !active.is_empty() => {
                if let Some(joined) = joined {
                    let _ = record_advance_result(
                        seller,
                        joined,
                        &mut terminal_receipts,
                        seller_policy,
                        &mut first_error,
                    ).await;
                }
            }
            joined = terminal_receipts.join_next(), if !terminal_receipts.is_empty() => {
                if let Some(joined) = joined {
                    let _ = record_terminal_receipt_result(joined, &mut first_error);
                }
            }
            _ = gateway_poll.tick() => {
                if seller.server_task.is_finished() {
                    stop_error = Some(anyhow::anyhow!(
                        "seller gateway stopped while pool deals were active"
                    ));
                    break 'pool;
                }
            }
        }
    }

    while let Ok(measurement) = claim_delivery_rx.try_recv() {
        emit_claim_delivery_measurement(measurement);
    }
    // re-review, finding 2: DRAIN, never drop. A dropped watcher takes the identity it
    // advanced with it, and the sweep then proves the absence of a generation nobody cares about.

    // point 2: the drain now has a BOUND on its own completion. Delivering the stop signal
    // (point 1) makes every watcher observable; it does not make any of them finish. Without a
    // bound the operator's signal is honoured and the process still never exits, which is the
    // failure this loop was measured doing: the shutdown arm fired and 591 further lines of chain
    // traffic followed.

    // The bound is `MATCH_OPEN_TIMEOUT`, and it is a DOMAIN fact rather than a smaller number: it is
    // the contract's own funded-but-unopened window (`params.rs`, spec 2.1). After it, a deal that
    // never opened is cleanable BY THE CHAIN, so no watcher can still be doing legitimate work on
    // one. It is also exactly the bound on the second road into this same hang, described at the
    // `identity: None` branch above: that arm can only call `wait_for_match`, which takes no stop
    // selector at all, and its whole risk is "an order that never landed" -- which is the condition
    // `MATCH_OPEN_TIMEOUT` measures. One bound closes both roads.

    // is not traded away for it. Timing out must not hand the sweep a stale generation, so the
    // deals whose watcher never reported back are REMOVED from `resting` before the sweep rather
    // than swept on a predecessor that may already be consumed -- and they are named in the refusal
    // instead. Ground the drain could not establish is reported, not guessed at.
    let expected: std::collections::BTreeSet<String> = resting.keys().cloned().collect();
    let undrained = drain_watchers_within(
        &mut watched,
        dexdo_core::params::MATCH_OPEN_TIMEOUT,
        &expected,
        |(deal, _, _)| deal.cfg.token_contract.clone(),
        async |(deal, current_identity, outcome)| {
            apply_drained_outcome(
                seller,
                deal,
                current_identity,
                outcome,
                &mut resting,
                &mut first_error,
            )
            .await;
        },
    )
    .await;
    if let Some((outstanding, refusal)) = undrained {
        for token_contract in &outstanding {
            resting.remove(token_contract);
        }
        first_error.get_or_insert(refusal);
    }
    // point 2, second half: the sweep is the drain's twin and was unbounded for the same
    // reason. It proves absence on chain, so it is bounded by the same chain fact -- past
    // `MATCH_OPEN_TIMEOUT` there is nothing left to prove about a deal that never opened.
    let sweep = sweep_unconfirmed_resting_offers(seller, resting.into_values().collect());
    match tokio::time::timeout(dexdo_core::params::MATCH_OPEN_TIMEOUT, sweep).await {
        Ok(Some(error)) => {
            first_error.get_or_insert(error);
        }
        Ok(None) => {}
        Err(_) => {
            first_error.get_or_insert_with(|| {
                anyhow::anyhow!(
                    "seller pool shutdown: the resting-offer sweep did not finish within \
                     MATCH_OPEN_TIMEOUT ({}s); the offers it was proving absent were left unproven",
                    dexdo_core::params::MATCH_OPEN_TIMEOUT.as_secs()
                )
            });
        }
    }
    seller.server_task.abort();
    if stopped_by_operator && first_error.is_none() && cascade_notes.is_empty() {
        emit_seller_shutdown_event(&primary_token_contract);
        return Ok(());
    }
    // the primary failure is the process error. A cascade note only becomes the process error
    // when it is the ONLY thing that went wrong; otherwise it hangs off the primary as `secondary`.
    match stop_error.or(first_error) {
        Some(error) => Err(attach_cascade_notes(error, cascade_notes)),
        None => match cascade_notes.into_iter().next() {
            Some(note) => Err(anyhow::Error::new(note)),
            None => Ok(()),
        },
    }
}

/// Nobody named a model, said in the terms of the path the operator is on.

/// On the practice market there is a mock upstream to offer them instead; on a real chain there is
/// not, and the name is also what `model_hash` is taken from. One copy of each sentence, so the
/// check that runs at the command boundary refuses in the same words as the run itself.
fn no_model_named(args: &SellerArgs) -> anyhow::Error {
    if args.mock.mock_chain {
        anyhow::anyhow!("set --model <name from config> (or --mock-model for a mock upstream)")
    } else {
        anyhow::anyhow!(format!(
            "real {}: set --model <name from config> (needed for model_hash)",
            dexdo_core::params::current_network()
        ))
    }
}

fn seller_upstream(
    args: &SellerArgs,
    model_name: Option<&str>,
    claimed_frame_model: Option<&str>,
) -> Result<dexdo::seller::UpstreamConfig> {
    if args.mock.mock_model {
        return Ok(match claimed_frame_model {
            Some(frame_model) => {
                dexdo::seller::UpstreamConfig::MockWithClaimedModel(frame_model.to_string())
            }
            None => dexdo::seller::UpstreamConfig::Mock,
        });
    }
    let name = model_name
        .filter(|name| !name.is_empty())
        .ok_or_else(|| no_model_named(args))?;
    let models = dexdo::seller::ModelsConfig::load(&args.models)?;
    let model = models.get(name)?;
    model.require_api_key_present()?;
    Ok(if dexdo::seller::AnthropicConfig::supports(model) {
        dexdo::seller::UpstreamConfig::Anthropic(dexdo::seller::AnthropicConfig::from_model(
            model,
            claimed_frame_model,
        ))
    } else {
        dexdo::seller::UpstreamConfig::OpenAi(dexdo::seller::OpenAiConfig::from_model(
            model,
            claimed_frame_model,
        ))
    })
}

pub(crate) async fn run_seller(args: SellerArgs) -> Result<()> {
    run_seller_with_deal_gas_overhead(args, None).await
}

pub(crate) async fn run_seller_with_deal_gas_overhead(
    // `mut` only where something mutates it: the note the picker settles is written back into
    // `args.identity`, and that picker exists under the chain build. The default build warned on a `mut`
    // it could not see a use for, and a warning nobody can act on is one everybody learns to skip.
    mut args: SellerArgs,
    deal_gas_overhead_raw: Option<u128>,
) -> Result<()> {
    // reject an invalid limit SELL price at the command boundary, before any file or chain work.
    super::support::validate_price_step(args.price_per_tick as u128)?;
    // the advertised gateway is what a REMOTE buyer dials out of the handover. Validate it at
    // the command boundary -- before any file, chain or order-book work -- so a non-routable address
    // can never reach `postSellOffer` and leave a resting ask no buyer can connect to.
    let gateway_advertise = args.checked_gateway_advertise_addr()?;
    // The live line below announces network work, and a refusal the arguments have ALREADY earned
    // has to come before it: a run that cannot start says why, and says nothing else first. Nobody
    // naming a model is knowable right here -- no chain, no note, no manifest -- yet it used to be
    // found only where the gateway is built, so a forgotten `--model` printed "checking the network
    // and contracts" before refusing, claiming a step it had not taken.

    // Only the NAME is settled here. The models file and the upstream key stay where they were on
    // purpose: has a restarting seller adopt its resting ask and cancel it exactly before it
    // dies of a missing credential, and a boundary that refused first would leave that ask resting
    // in the book with nobody behind it.

    // `--market` is left out for the same reason: there the name may come from the manifest, which
    // is read below, so that pairing is still settled where the manifest is.
    if args.market.is_none()
        && !args.mock.mock_model
        && args
            .model
            .as_deref()
            .filter(|name| !name.is_empty())
            .is_none()
    {
        return Err(no_model_named(&args));
    }
    // Which note, settled ONCE -- but only after the argument checks above, the model's name among
    // them. Those refuse a bad price, an unroutable gateway or a nameless model before any file is
    // touched, and asking the operator to choose a note in front of a refusal their arguments had
    // already earned is a question they should never see.

    // Settled here rather than in each of the three places that need it -- the pool lock, the
    // gateway's own directory, the backend -- so the operator is asked once. On a terminal the
    // pool's notes are offered; anywhere else this is the refusal that names `--note-addr`.
    if args.identity.note_addr.is_none() && !args.mock.mock_chain {
        args.identity.note_addr = Some(
            // The endpoint is the manifest's: it names the network this run is on, and a second
            // source for it here would be a second answer to the same question.
            crate::cli::note_pick::ask_which_note(&crate::cli::commands::manifest_path()?, None)
                .await?,
        );
    }
    // Issue: the deal token_contract comes from `--market` (a provision manifest) or `--token-contract`.
    // The manifest's frame_model (if any) is validated against `--model` inside `seller_real_backend`.
    let (mut token_contract, mut market_frame_model, market_nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    match (
        args.recover_publication.as_deref(),
        args.confirm_recover_publication,
    ) {
        (Some(_), false) => bail!(
            "--recover-publication requires --confirm-recover-publication; automatic service/timer restarts must not use this escape hatch"
        ),
        (Some(operator_tc), true) => {
            // A market manifest and a human command may name the same account in
            // different accepted address representations.  Compare the parsed
            // account, not its spelling; the recovery still binds to the selected
            // manifest token contract below.
            let operator_tc = dexdo_core::normalize_wallet_address(operator_tc).map_err(|error| {
                anyhow::anyhow!("--recover-publication has an invalid TokenContract: {error}")
            })?;
            let selected_tc = dexdo_core::normalize_wallet_address(&token_contract).map_err(|error| {
                anyhow::anyhow!("selected TokenContract is invalid: {error}")
            })?;
            if operator_tc != selected_tc {
                bail!(
                    "--recover-publication does not match the selected TokenContract"
                );
            }
        }
        (None, true) => unreachable!("clap requires --recover-publication"),
        _ => {}
    }
    let mut startup_market = args.market.as_deref().map(load_market).transpose()?;
    // Review: the deal nonce comes from `--market` (the manifest) or the explicit `--nonce` flag --
    // never both (the manifest is the single source of truth). The real-chain seller path requires
    // it (see `seller_real_backend`); the mock path ignores it.
    if args.market.is_some() && args.nonce.is_some() {
        bail!("--market and --nonce are mutually exclusive -- the nonce comes from the manifest");
    }
    // The policy is a file on this machine, and a run it refuses never reaches a gateway or the
    // chain (E2E-ADV-13). So it is read here, with the other refusals the operator's own inputs
    // earn, and above the live line: a seller that will not start must not first say it is checking
    // a network it is not going to talk to.
    let seller_policy = if !args.mock.mock_chain || args.policy.is_some() {
        policy::load_seller_runtime_policy(args.policy.as_deref())?
    } else {
        policy::SellerRuntimePolicy {
            after_deal_done: policy::SellerAfterDealDoneAction::Retire,
            buyer_no_show: policy::SellerBuyerNoShowAction::RetireGateway,
            dispute_against_me: policy::SellerDisputeAgainstMeAction::Hold,
            max_open_deals: 2,
        }
    };
    let chain_unavailable_action = if !args.mock.mock_chain || args.policy.is_some() {
        policy::load_seller_chain_unavailable_action(args.policy.as_deref())?
    } else {
        dexdo::seller::gateway::ChainUnavailableAction::Stop
    };
    // The three layers asks for, on the command that had none of them. A sale posts an
    // offer in seconds and then waits for a buyer for hours: measured on a three-minute live run,
    // the last line addressed to the operator arrived at t=8.4s and nothing followed it. The records
    // that used to fill that silence were the wrong answer to it -- a heartbeat every four seconds
    // is not a window, and it was only ever visible with `RUST_LOG=info`.

    // Held for the whole command: dropping it takes the live line down, which is what should happen
    // when the seller stops.
    let _display = crate::cli::progress::Status::with_plan(
        "checking the network and contracts",
        [
            (
                "checking the network and contracts",
                "network and contracts checked",
            ),
            // In the order the seller actually does them: the gateway is listening before the offer
            // that advertises it is posted. Declared the other way round, the third step was never
            // reached and its tick never appeared -- a checklist that ends one short reads as a
            // command that stopped early.
            ("bringing the gateway up", "gateway ready"),
            ("posting the offer", "offer posted"),
            // The wait is a step, because it is a state and it is where the seller spends its hours:
            // undeclared, it left the counter sitting on the last piece of work while the live line
            // said something else. The label it is reached by carries the order id and the gateway
            // after this prefix, which the checklist matches by.
            ("waiting for a buyer", "a buyer connected"),
        ],
    );
    let shutdown = operator_shutdown_signal().fuse();
    tokio::pin!(shutdown);
    let mut shutdown_requested = futures::poll!(shutdown.as_mut()).is_ready();
    tracing::debug!(
        policy_after_deal_done = seller_policy.after_deal_done.as_str(),
        policy_buyer_no_show = seller_policy.buyer_no_show.as_str(),
        policy_dispute_against_me = seller_policy.dispute_against_me.as_str(),
        policy_max_open_deals = seller_policy.max_open_deals,
        ?chain_unavailable_action,
        "seller policy loaded"
    );
    let (_seller_pool_lock, mut persistent_gateway_tls) = if args.mock.mock_chain {
        (None, None)
    } else {
        let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
            anyhow::anyhow!(format!(
                "real {}: --note-addr is required for seller pool locking",
                dexdo_core::params::current_network()
            ))
        })?;
        let pool_dir = seller_pool_dir(args.deals_dir.as_deref(), note_addr)?;
        (
            Some(acquire_seller_pool_lock(&pool_dir)?),
            Some(load_or_create_gateway_tls(&pool_dir)?),
        )
    };
    // on the real path, the --market manifest's seller_note must be this seller's --note-addr -- else the
    // offer posts a non-canonical TC the InferenceOrderBook won't rest, and the seller never matches.
    if !args.mock.mock_chain {
        if let (Some(manifest), Some(note_addr)) =
            (startup_market.as_ref(), args.identity.note_addr.as_deref())
        {
            assert_market_seller_note(&manifest.seller_note, note_addr)?;
        }
    }
    let mut deal_nonce = market_nonce.or(args.nonce);
    let recovery_frame_model = if args.mock.mock_chain {
        None
    } else {
        let name = args
            .model
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| no_model_named(&args))?;
        Some(match market_frame_model.as_ref() {
            Some(frame_model) => frame_model.clone(),
            None => dexdo::seller::ModelsConfig::load(&args.models)?
                .get(name)?
                .frame_model
                .clone(),
        })
    };
    let registry_policy = if !args.mock.mock_chain && !shutdown_requested {
        load_enabled_model_registry_policy(
            RegistryRole::Seller,
            &args.registry,
            &crate::cli::commands::manifest_path()?,
        )?
    } else {
        None
    };
    if registry_policy.is_some() {
        let manifest = crate::cli::commands::manifest_path()?;
        let registry_preload = preload_model_registry_policy(
            RegistryRole::Seller,
            registry_policy.as_ref(),
            &manifest,
        );
        tokio::pin!(registry_preload);
        tokio::select! {
            biased;
            _ = shutdown.as_mut() => shutdown_requested = true,
            result = &mut registry_preload => result?,
        }
    }
    // The locally selected market/config model is enough to find and cancel a SELL from an
    // earlier run. Fallible registry, credential and note-readiness checks happen after that
    // exact identity is captured.
    let mock_endpoints_file = args
        .mock
        .mock_chain
        .then(|| resolve_endpoints_file(args.endpoints_file.clone()))
        .transpose()?;
    let (mut chain, mut note) = if let Some(endpoints_file) = mock_endpoints_file.as_ref() {
        mock_chain_and_note(endpoints_file.clone(), &args.identity)?
    } else {
        seller_real_backend_with_deal_gas_overhead(
            &args,
            market_frame_model.as_deref(),
            deal_nonce,
            recovery_frame_model.as_deref(),
            deal_gas_overhead_raw,
        )?
    };
    // a withdrawn PrivateNote is final for seller writes. Fail before per-deal TC term
    // reads or resume checks so the fresh-note action remains the primary error. A shutdown
    // cancels this retrying read but still continues through exact resting-offer inspection so
    // can cancel and confirm the captured identity below.
    if !shutdown_requested {
        let note_post_eligibility = async {
            #[cfg(test)]
            if std::env::var_os("DEXDO_TEST_335_PENDING_EARLY_WITHDRAWN_READ").is_some() {
                println!("seller-early-withdrawn-read-pending");
                let _ = std::io::stdout().flush();
                std::future::pending::<()>().await;
            }
            chain.assert_note_can_post_sell_offer().await
        };
        tokio::pin!(note_post_eligibility);
        tokio::select! {
            biased;
            _ = shutdown.as_mut() => {
                shutdown_requested = true;
            },
            result = &mut note_post_eligibility => result?,
        }
    }
    let seller_owner = args.identity.note_addr.clone().unwrap_or_else(|| {
        format!(
            "0:{}",
            note.pubkey()
                .ed
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        )
    });
    let mut resumed_descendant = false;
    // Terms come from the selected TC. If that lineage is already terminal, only its persisted
    // replacement link may redirect startup to a same-note deal handle.
    let mut terms = chain.sell_offer_terms(&token_contract).await;
    let mut unavailable_consumed = false;
    if !matches!(&terms, Ok(Some(_))) {
        let initial_fill = dexdo::seller::read_seller_fill_lineage(
            &seller_watch_cursor_path(args.deals_dir.as_deref(), &token_contract)?,
            &token_contract,
        )?;
        if let Some(mut linked) = initial_fill
            .as_ref()
            .and_then(|fill| fill.replacement_token_contract.clone())
        {
            let markets = seller_market_manifests(args.deals_dir.as_deref(), &seller_owner)?;
            let mut ancestor = token_contract.clone();
            let mut visited = std::collections::HashSet::new();
            loop {
                if !visited.insert(deals::normalize_addr(&ancestor)) {
                    bail!(
                        "seller replacement lineage contains a cycle at {}",
                        display_token_contract(&ancestor)
                    );
                }
                let market = markets
                    .get(&deals::normalize_addr(&linked))
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "seller replacement lineage links {} to {}, but no same-note deal handle carries its market",
                            display_token_contract(&ancestor),
                            display_token_contract(&linked)
                        )
                    })?;
                let (descendant_chain, descendant_note) =
                    if let Some(endpoints_file) = mock_endpoints_file.as_ref() {
                        let chain: Arc<dyn dexdo_core::ChainBackend> =
                            Arc::new(dexdo_core::MockChainBackend::new(
                                endpoints_file.clone(),
                                dexdo_core::ProtocolConsts::canonical(),
                                DobParams::canonical(),
                            ));
                        (chain, note.clone())
                    } else {
                        seller_real_backend_with_deal_gas_overhead(
                            &args,
                            Some(&market.frame_model),
                            Some(market.nonce),
                            Some(&market.frame_model),
                            deal_gas_overhead_raw,
                        )?
                    };
                let descendant_terms = descendant_chain.sell_offer_terms(&linked).await;
                let descendant_fill = if matches!(&descendant_terms, Ok(Some(_))) {
                    None
                } else {
                    dexdo::seller::read_seller_fill_lineage(
                        &seller_watch_cursor_path(args.deals_dir.as_deref(), &linked)?,
                        &linked,
                    )?
                };
                let next = descendant_fill
                    .as_ref()
                    .and_then(|fill| fill.replacement_token_contract.clone());
                let mock_unposted = args.mock.mock_chain
                    && matches!(&descendant_terms, Ok(None))
                    && descendant_fill.is_none();
                if matches!(&descendant_terms, Ok(Some(_))) || mock_unposted {
                    terms = if mock_unposted {
                        Ok(Some((
                            u64::try_from(market.price_per_tick).map_err(|_| {
                                anyhow::anyhow!(
                                    "seller market {} price exceeds u64",
                                    display_token_contract(&market.token_contract)
                                )
                            })?,
                            u64::try_from(market.max_ticks).map_err(|_| {
                                anyhow::anyhow!(
                                    "seller market {} max_ticks exceeds u64",
                                    display_token_contract(&market.token_contract)
                                )
                            })?,
                        )))
                    } else {
                        descendant_terms
                    };
                    token_contract = linked;
                    market_frame_model = Some(market.frame_model.clone());
                    deal_nonce = Some(market.nonce);
                    startup_market = Some(market);
                    chain = descendant_chain;
                    note = descendant_note;
                    resumed_descendant = true;
                    break;
                }
                let Some(next) = next else {
                    terms = descendant_terms;
                    unavailable_consumed = descendant_fill.is_some();
                    break;
                };
                ancestor = linked;
                linked = next;
            }
        } else {
            unavailable_consumed = initial_fill.is_some();
        }
    }
    let (price, ticks) = match terms? {
        Some(terms) => terms,
        None if args.mock.mock_chain && !unavailable_consumed => (args.price_per_tick, 1024),
        None => {
            bail!(
                "seller requires a deployed per-deal TokenContract; selected {} is unavailable \
                 and has no active persisted replacement descendant",
                display_token_contract(&token_contract)
            )
        }
    };
    let (offer_ticks, offer_price) = (ticks, price);
    // The real path publishes the TC getter value, not the CLI fallback. Validate the actual
    // write-bound price as well, after read-only term discovery and before postSellOffer.
    super::support::validate_price_step(offer_price as u128)?;
    let mut cfg = dexdo::seller::SellerConfig {
        token_contract: token_contract.clone(),
        price_per_tick: offer_price,
        max_ticks: offer_ticks,
        subscription: args.subscription,
        gateway_advertise: gateway_advertise.clone(),
        mock_token_count: args.mock_token_count,
    };
    let inspection =
        dexdo::seller::inspect_seller_offer(chain.as_ref(), &cfg, Some(&seller_owner)).await?;
    let mut inspected_identity = match inspection {
        dexdo::seller::SellerOfferInspection::Resting { order_id } => {
            Some(dexdo::seller::liveness::RestingOfferIdentity {
                owner_note: seller_owner.clone(),
                token_contract: token_contract.clone(),
                order_id,
            })
        }
        dexdo::seller::SellerOfferInspection::Funded
        | dexdo::seller::SellerOfferInspection::Vacant => None,
    };

    let preflight_result = if shutdown_requested {
        None
    } else {
        let preflight = async {
            #[cfg(test)]
            if std::env::var_os("DEXDO_TEST_668_PENDING_SELLER_PREFLIGHT").is_some() {
                println!("seller-restart-preflight-pending");
                let _ = std::io::stdout().flush();
                std::future::pending::<()>().await;
            }
            let mut registry_frame_model = None;
            if !args.mock.mock_chain {
                chain_doctor_preflight(
                    &crate::cli::commands::manifest_path()?,
                    (!resumed_descendant)
                        .then_some(args.market.as_deref())
                        .flatten(),
                )
                .await?;
                // ASK THE REGISTRY BEFORE THE LISTING IS PAID FOR, ON THE DEFAULT PATH.

                // `dexdo seller` creates a market. Its `post_offer` reads the book's stats and, when
                // the book is absent, deploys it out of the note before it writes the ask
                // (`crates/core/src/chain/backends.rs`, "An operate exception...
                // deploy it (model listing)"). That is the same spend `provision` and
                // `deploy-market` already refuse to make on a name no buyer can resolve, and this
                // command was the one of the three still making it.

                // What stood here alone is the block below, and it is behind `if let Some(policy)`:
                // `RegistryValidationPolicy::disabled()` sets `seller_check_model_registry: false`,
                // so without an explicit `--model-registry-validation <config.json>` the policy is
                // `None` and NOTHING asked the catalog anything. `require_model_name` is the only
                // other name gate on this path and it refuses an empty or space-padded name, which

                // switch as the mechanism a wrong spelling reached mainnet through, and rules that
                // the check is not configurable: the one opt-out is the flag.

                // The SAME resolver and the SAME refusal rule as the other two commands, by calling
                // the same function -- one identity rule with two defaults on two sides is the
                // defect reports, and a seller-local copy of it would be a third.

                // Placed inside this preflight, and not at the command boundary, because the models
                // file is deliberately read late: has a restarting seller adopt its resting ask
                // and cancel it before it dies of a missing credential, and reading `models.json`
                // earlier would leave that ask resting with nobody behind it. Nothing above this
                // line writes to the chain -- the first write is the offer post, far below.
                {
                    let name = args
                        .model
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                format!("real {}: set --model <name from config> (needed for model registry validation)", dexdo_core::params::current_network())
                            )
                        })?;
                    let listed_frame_model = dexdo::seller::ModelsConfig::load(&args.models)?
                        .get(name)?
                        .frame_model
                        .clone();
                    let listed_frame_model = crate::cli::support::require_model_name(
                        &listed_frame_model,
                        "the `frame_model` field of this model's entry in models.json",
                        "Fix that field in the models config.",
                    )?;
                    crate::cli::admin::ensure_model_resolves(
                        crate::cli::admin::ModelResolutionCaller::Seller,
                        &crate::cli::commands::manifest_path()?,
                        None,
                        &listed_frame_model,
                        args.allow_unverified_model,
                    )
                    .await?;
                }
                if let Some(policy) = registry_policy.as_ref() {
                    let name = args
                        .model
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                format!("real {}: set --model <name from config> (needed for model registry validation)", dexdo_core::params::current_network())
                            )
                        })?;
                    let configured_frame_model = dexdo::seller::ModelsConfig::load(&args.models)?
                        .get(name)?
                        .frame_model
                        .clone();
                    let selected_market = startup_market.clone();
                    let target = resolve_model_registry_target(
                        RegistryRole::Seller,
                        Some(policy),
                        &crate::cli::commands::manifest_path()?,
                        &configured_frame_model,
                        BookTarget {
                            frame_model: selected_market
                                .as_ref()
                                .map(|market| market.frame_model.clone())
                                .unwrap_or_else(|| configured_frame_model.clone()),
                            model_hash: selected_market
                                .as_ref()
                                .map(|market| market.model_hash.clone())
                                .unwrap_or_else(|| {
                                    dexdo_core::model_hash_for(&configured_frame_model)
                                }),
                            order_book: selected_market
                                .as_ref()
                                .map(|market| market.inference_order_book.clone()),
                            root_model: selected_market
                                .as_ref()
                                .map(|market| market.root_model.clone()),
                            note_addr: args.identity.note_addr.clone(),
                        },
                    )
                    .await?;
                    let frame_model = target.frame_model;
                    check_market_model_match(market_frame_model.as_deref(), &frame_model, name)?;
                    let expected_order_book = if let Some(order_book) = target.order_book {
                        order_book
                    } else {
                        let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
                            anyhow::anyhow!(format!(
                                "real {}: --note-addr is required to derive the seller order book",
                                dexdo_core::params::current_network()
                            ))
                        })?;
                        expected_order_book_for_note(
                            &crate::cli::commands::manifest_path()?,
                            note_addr,
                            &frame_model,
                        )
                        .await?
                    };
                    let order_book_active = order_book_active_from_contracts(
                        &crate::cli::commands::manifest_path()?,
                        &expected_order_book,
                    )
                    .await?;
                    enforce_model_registry_policy(
                        RegistryRole::Seller,
                        policy,
                        &crate::cli::commands::manifest_path()?,
                        &frame_model,
                        &expected_order_book,
                        order_book_active,
                        BuyerMissingBookPolicy::Reject,
                    )
                    .await?;
                    if inspected_identity.is_some()
                        && recovery_frame_model.as_deref() != Some(frame_model.as_str())
                    {
                        bail!(
                            "model registry resolved {frame_model}, but the existing exact SELL \
                             was found under {}; cancelling it before restart",
                            recovery_frame_model.as_deref().unwrap_or("the local model")
                        );
                    }
                    registry_frame_model = Some(frame_model);
                }
            }

            if registry_frame_model.as_deref() != recovery_frame_model.as_deref() {
                if let Some(frame_model) = registry_frame_model.as_deref() {
                    let (resolved_chain, resolved_note) =
                        seller_real_backend_with_deal_gas_overhead(
                            &args,
                            market_frame_model.as_deref(),
                            deal_nonce,
                            Some(frame_model),
                            deal_gas_overhead_raw,
                        )?;
                    chain = resolved_chain;
                    note = resolved_note;
                    let inspection = dexdo::seller::inspect_seller_offer(
                        chain.as_ref(),
                        &cfg,
                        Some(&seller_owner),
                    )
                    .await?;
                    inspected_identity = match inspection {
                        dexdo::seller::SellerOfferInspection::Resting { order_id } => {
                            Some(dexdo::seller::liveness::RestingOfferIdentity {
                                owner_note: seller_owner.clone(),
                                token_contract: token_contract.clone(),
                                order_id,
                            })
                        }
                        dexdo::seller::SellerOfferInspection::Funded
                        | dexdo::seller::SellerOfferInspection::Vacant => None,
                    };
                }
            }

            Ok(registry_frame_model)
        };
        tokio::pin!(preflight);
        tokio::select! {
            biased;
            _ = shutdown.as_mut() => {
                shutdown_requested = true;
                None
            },
            result = &mut preflight => Some(result),
        }
    };
    let (registry_frame_model, preflight_error) = match preflight_result {
        Some(Ok(ready)) => (ready, None),
        Some(Err(error)) => (None, Some(error)),
        None => (None, None),
    };
    let seller_frame_model_for_handle = if args.mock.mock_chain {
        market_frame_model
            .clone()
            .or_else(|| args.model.clone())
            .or_else(|| Some("mock".to_string()))
    } else {
        Some(
            registry_frame_model
                .clone()
                .or_else(|| recovery_frame_model.clone())
                .expect("real seller model was resolved"),
        )
    };
    let seller_deals_dir = deals::resolve_deals_dir(args.deals_dir.as_deref())?;

    // The step the plan declared and nothing set: binding the port, loading the pinned TLS material
    // and probing the advertised address is the slowest thing between the contracts and the offer,
    // and until now it happened under the label of the step before it.
    crate::cli::progress::step("bringing the gateway up");
    let seller = match start_seller_gateway_with_liveness(
        async {
            if let Some(error) = preflight_error {
                return Err(error);
            }
            let upstream = seller_upstream(
                &args,
                args.model.as_deref(),
                registry_frame_model.as_deref(),
            )?;
            // these fallible note readiness reads are protected by the exact
            // restart identity above, before any new seller-chain write is possible.
            chain.assert_note_current().await?;
            chain.assert_note_can_post_sell_offer().await?;
            match persistent_gateway_tls.take() {
                Some(tls) => {
                    dexdo::seller::start_gateway_with_note_tls_and_deals_dir(
                        args.gateway_listen,
                        upstream,
                        note,
                        seller_deals_dir.clone(),
                        tls,
                    )
                    .await
                }
                None => {
                    dexdo::seller::start_gateway_with_note_and_deals_dir(
                        args.gateway_listen,
                        upstream,
                        note,
                        seller_deals_dir.clone(),
                    )
                    .await
                }
            }
        },
        chain.as_ref(),
        &cfg,
        inspected_identity.as_ref(),
        async {
            if !shutdown_requested {
                shutdown.as_mut().await;
            }
        },
    )
    .await?
    {
        SellerGatewayStartup::Ready(seller) => seller,
        SellerGatewayStartup::Stopped {
            reason,
            disposition,
        } => {
            if let dexdo::seller::liveness::RestingStopReason::Health(failure) = &reason {
                emit_seller_liveness_event(
                    &token_contract,
                    inspected_identity
                        .as_ref()
                        .map(|identity| identity.owner_note.as_str())
                        .or(Some(&seller_owner)),
                    inspected_identity
                        .as_ref()
                        .map(|identity| identity.order_id),
                    "seller_health",
                    Some(failure.component.as_str()),
                    if failure.timed_out { "timeout" } else { "fail" },
                    None,
                );
            }
            emit_seller_liveness_event(
                &token_contract,
                inspected_identity
                    .as_ref()
                    .map(|identity| identity.owner_note.as_str())
                    .or(Some(&seller_owner)),
                inspected_identity
                    .as_ref()
                    .map(|identity| identity.order_id),
                "seller_offer_terminal",
                None,
                disposition.as_str(),
                disposition.known_result(),
            );
            return match reason {
                dexdo::seller::liveness::RestingStopReason::Shutdown
                    if matches!(
                        &disposition,
                        dexdo::seller::liveness::CancellationDisposition::Cancelled
                            | dexdo::seller::liveness::CancellationDisposition::AlreadyAbsent
                    ) =>
                {
                    emit_seller_shutdown_event(&token_contract);
                    Ok(())
                }
                dexdo::seller::liveness::RestingStopReason::Shutdown => Err(anyhow::anyhow!(
                    "seller shutdown during gateway startup did not reach a cancellable terminal: \
                     token_contract={}; cancellation_disposition={disposition}",
                    display_token_contract(&token_contract)
                )),
                dexdo::seller::liveness::RestingStopReason::Health(failure) => {
                    Err(anyhow::anyhow!(
                        "seller gateway startup failed while exact SELL rested: {failure}; \
                         cancellation_disposition={disposition}"
                    ))
                }
                // the seller's offer reached its own on-chain deadline. This is the ordinary end
                // of a mandatory-TTL SELL, not a fault, so it is reported as a terminal outcome naming
                // the deadline and the time it was observed rather than as a startup failure.
                dexdo::seller::liveness::RestingStopReason::Expired(expired) => {
                    println!(
                        "seller_offer_outcome EXPIRED token_contract={} deadline={} observed_at={} \
                         cancellation_disposition={disposition}",
                        display_token_contract(&token_contract),
                        expired.deadline, expired.observed_at
                    );
                    Ok(())
                }
                dexdo::seller::liveness::RestingStopReason::Watcher(error) => Err(anyhow::anyhow!(
                    "seller gateway startup watcher failed: {error}; \
                     cancellation_disposition={disposition}"
                )),
            };
        }
    };
    seller
        .state
        .set_chain_unavailable_action(chain_unavailable_action);
    // `--gateway-listen <host>:0` asks the OS for a free port, and an advertise inherited from
    // it was resolved before the listener existed, so it still says `:0` -- a port nobody can dial,
    // neither the buyer out of the handover nor this seller's own self-probe. The gateway is bound
    // now, so adopt the real port here: before readiness, before the deal handle and the handover
    // are written, and before any order is posted.
    let gateway_advertise = args.bound_gateway_advertise(gateway_advertise, seller.listen_addr);
    cfg.gateway_advertise.clone_from(&gateway_advertise);
    let watch = dexdo::seller::SellerMatchWatchConfig {
        cursor_path: seller_watch_cursor_path(args.deals_dir.as_deref(), &token_contract)?,
        poll_interval: DEFAULT_MATCH_POLL_INTERVAL,
    };
    let note_addr = args.identity.note_addr.as_deref().unwrap_or(&seller_owner);
    let frame_model = seller_frame_model_for_handle.as_deref().unwrap_or("mock");
    let context = SellerPoolContext {
        deals_dir: args.deals_dir.as_deref(),
        contracts: &crate::cli::commands::manifest_path()?,
        note_addr,
        frame_model,
        gateway_advertise: &gateway_advertise,
        advertise_probe: args.advertise_probe_policy(),
        // The operator's spelling was checked above.  Pass the selected runtime
        // identity forward so the audited one-shot permit uses precisely the
        // token contract that this seller will serve.
        recover_publication: args
            .recover_publication
            .as_deref()
            .map(|_| token_contract.as_str()),
    };
    if !args.mock.mock_chain {
        sweep_configured_seller_model_books(&args, note_addr, context.frame_model, &token_contract)
            .await?;
    }
    let active_deals = sweep_seller_startup_offers(&context, &chain, &token_contract, |market| {
        if let Some(endpoints_file) = mock_endpoints_file.as_ref() {
            Ok(Arc::new(dexdo_core::MockChainBackend::new(
                endpoints_file.clone(),
                dexdo_core::ProtocolConsts::canonical(),
                DobParams::canonical(),
            )) as Arc<dyn dexdo_core::ChainBackend>)
        } else {
            let (chain, _) = seller_real_backend_with_deal_gas_overhead(
                &args,
                Some(&market.frame_model),
                Some(market.nonce),
                Some(&market.frame_model),
                deal_gas_overhead_raw,
            )?;
            Ok(chain)
        }
    })
    .await?;
    let initial = SellerPoolDeal {
        chain,
        cfg,
        watch,
        upstream: seller_upstream(
            &args,
            args.model.as_deref(),
            registry_frame_model.as_deref(),
        )?,
        nonce: deal_nonce.unwrap_or(0),
        market: startup_market,
    };
    let pool = load_seller_pool_deals_with_scope(
        &context,
        initial,
        args.mock_token_count,
        Some(&active_deals),
        |market| {
            if let Some(endpoints_file) = mock_endpoints_file.as_ref() {
                let chain: Arc<dyn dexdo_core::ChainBackend> =
                    Arc::new(dexdo_core::MockChainBackend::new(
                        endpoints_file.clone(),
                        dexdo_core::ProtocolConsts::canonical(),
                        DobParams::canonical(),
                    ));
                Ok((
                    chain,
                    seller_upstream(&args, args.model.as_deref(), Some(&market.frame_model))?,
                ))
            } else {
                let (chain, _) = seller_real_backend_with_deal_gas_overhead(
                    &args,
                    Some(&market.frame_model),
                    Some(market.nonce),
                    Some(&market.frame_model),
                    deal_gas_overhead_raw,
                )?;
                Ok((
                    chain,
                    seller_upstream(&args, Some(&market.frame_model), Some(&market.frame_model))?,
                ))
            }
        },
    )
    .await?;
    let admission = admit_seller_startup_deals(pool, &context, &seller_policy).await?;
    let pool = admission.admitted;
    let args_ref = &args;
    let provision_endpoints = mock_endpoints_file.clone();
    let provision_seller = seller_owner.clone();
    let mut provisioner = |frame_model: String, nonce, price_per_tick, max_ticks| {
        let provision_endpoints = provision_endpoints.clone();
        let provision_seller = provision_seller.clone();
        async move {
            if let Some(endpoints_file) = provision_endpoints.as_ref() {
                let identity = format!("{provision_seller}:{frame_model}:{nonce}");
                let token_contract = format!(
                    "0:{}",
                    dexdo_core::model_hash_for(&identity).trim_start_matches("0x")
                );
                let market = dexdo_core::MarketManifest {
                    network: "mock".to_string(),
                    frame_model: frame_model.clone(),
                    model_hash: dexdo_core::model_hash_for(&frame_model),
                    inference_order_book: "mock".to_string(),
                    root_model: "mock".to_string(),
                    token_contract,
                    seller_note: provision_seller.clone(),
                    nonce,
                    price_per_tick: u128::from(price_per_tick),
                    max_ticks: u128::from(max_ticks),
                };
                let chain: Arc<dyn dexdo_core::ChainBackend> =
                    Arc::new(dexdo_core::MockChainBackend::new(
                        endpoints_file.clone(),
                        dexdo_core::ProtocolConsts::canonical(),
                        DobParams::canonical(),
                    ));
                return Ok((market, chain));
            }
            provision_replacement_seller_with_deal_gas_overhead(
                args_ref,
                &frame_model,
                nonce,
                price_per_tick,
                max_ticks,
                deal_gas_overhead_raw,
            )
            .await
        }
    };
    let startup_policy = SellerStartupPolicy {
        runtime: &seller_policy,
        retained_deals: pool.len(),
        refused_startup_deals: admission.refused,
    };
    let result = run_seller_pool(
        &seller,
        pool,
        context,
        &startup_policy,
        &mut provisioner,
        shutdown.as_mut(),
        &mut shutdown_requested,
    )
    .await;
    if result.is_err() {
        seller.server_task.abort();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use dexdo::seller::{
        liveness::RestingOfferIdentity, prepare_seller_offer, SellerConfig, SellerOfferStartup,
    };
    use dexdo_core::{
        ChainBackend, ChainError, DealBuyerBond, DealChainSnapshot, DealChainState, DealSellerBond,
        DealSubscription, DobParams, LocalNote, Match, MatchWatchCursor, MatchedFill,
        MockChainBackend, Note, NotePubkey, OfferListing, ProtocolConsts, SellOffer,
        SellOfferOutcome, Settlement, StreamSnapshot, TokenContract,
    };
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    include!("seller_1056_restart_tests.rs");
    include!("seller_1402_refusal_tests.rs");

    #[tokio::test]
    async fn persisted_gateway_tls_survives_restart_and_pinned_reconnects() {
        let root = tempfile::tempdir().unwrap();
        let note_addr = format!("0:{}", "a".repeat(64));
        let pool_dir = seller_pool_dir(Some(root.path()), &note_addr).unwrap();
        let tls = load_or_create_gateway_tls(&pool_dir).unwrap();
        let fingerprint = tls.fingerprint.clone();
        let note = Arc::new(LocalNote::generate());
        let first = dexdo::seller::start_gateway_with_note_tls(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            note.clone(),
            tls,
        )
        .await
        .unwrap();
        dexdo::buyer::tls::connect_pinned(&format!("https://{}", first.listen_addr), &fingerprint)
            .await
            .expect("first pinned TLS connection");
        first.server_task.abort();
        let _ = first.server_task.await;

        let restored = load_or_create_gateway_tls(&pool_dir).unwrap();
        assert_eq!(restored.fingerprint, fingerprint);
        // shape C: re-binding the first gateway's exact port races whoever the kernel hands it
        // to in between (and the first gateway's own closed connections keep it in TIME_WAIT). What
        // this test proves is that the PERSISTED certificate survives a restart, and the reconnect
        // below already dials `second.listen_addr` -- so the restart takes a fresh ephemeral port.
        let second = dexdo::seller::start_gateway_with_note_tls(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            note,
            restored,
        )
        .await
        .unwrap();
        dexdo::buyer::tls::connect_pinned(&format!("https://{}", second.listen_addr), &fingerprint)
            .await
            .expect("restart must present the certificate pinned in the existing handover");
        second.server_task.abort();
        let _ = second.server_task.await;
    }

    #[tokio::test]
    async fn seller_pool_lock_contention_fails_before_any_chain_write() {
        let root = tempfile::tempdir().unwrap();
        let note_addr = format!("0:{}", "a".repeat(64));
        let deals_dir = root.path().join("deals");
        let pool_dir = seller_pool_dir(Some(&deals_dir), &note_addr).unwrap();
        let _lock = acquire_seller_pool_lock(&pool_dir).unwrap();
        let policy_path = root.path().join("policy.json");
        std::fs::write(
            &policy_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 1,
                "seller": {
                    "on": {
                        "after_deal_done": "retire",
                        "buyer_no_show": "retire_gateway",
                        "dispute_against_me": "hold"
                    },
                    "max_open_deals": 2
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let endpoints = root.path().join("must-not-be-written.json");
        let error = super::run_seller(crate::cli::args::SellerArgs {
            mock: crate::cli::args::MockFlags {
                mock_model: true,
                mock_chain: false,
            },
            identity: crate::cli::args::IdentityArgs {
                note_key: Some(root.path().join("missing-note.key")),
                note_index: 0,
                note_addr: Some(note_addr),
            },
            registry: crate::cli::args::ModelRegistryValidationArgs::default(),
            gateway_listen: "127.0.0.1:0".parse().unwrap(),
            gateway_advertise: None,
            // this test is about the pool lock, so opt into the loopback advertise explicitly.
            allow_private_advertise: true,
            require_advertise_probe: false,
            endpoints_file: Some(endpoints.clone()),
            deals_dir: Some(deals_dir),
            token_contract: Some(format!("0:{}", "b".repeat(64))),
            market: None,
            nonce: Some(7),
            subscription: false,
            price_per_tick: dexdo_core::PRICE_STEP as u64,
            mock_token_count: 8,
            model: Some("unused".to_string()),
            allow_unverified_model: false,
            models: root.path().join("missing-models.json"),
            policy: Some(policy_path),
            recover_publication: None,
            confirm_recover_publication: false,
        })
        .await
        .expect_err("second seller process for one note must fail on the production lock");
        assert!(error.to_string().contains("already running"), "{error:#}");
        assert!(
            !endpoints.exists(),
            "lock contention must fail before constructing a chain backend or writing endpoints"
        );
    }

    /// a re-bound gateway must not lock a seller out of its own deal.

    /// Driven through `seller_market_handles` - the real startup entry - against a handle actually
    /// written to disk, because the defect is exactly that this reader refused the deal before any
    /// of it could be reconstructed. Building the map by hand would never reach the refusal.

    /// Three things are asserted together, and each one fails a different wrong fix: the deal is
    /// reconstructed rather than refused; the record now carries the address this run really serves
    /// it from, so the NEXT start sees the truth instead of the same stale value; and nothing else in
    /// the handle moved, so adopting the address is not a licence to rewrite the manifest under it.
    #[test]
    fn a_rebound_gateway_adopts_the_runs_address_instead_of_refusing_the_deal() {
        let account = |c: char| std::iter::repeat_n(c, 64).collect::<String>();
        let note_addr = format!("0:{}", account('4'));
        let token_contract = format!("0:{}", account('3'));
        let root = tempfile::tempdir().unwrap();
        let deals_dir = root.path().join("deals");
        let market = dexdo_core::MarketManifest {
            network: "net-a".into(),
            frame_model: "qwen/qwen3-32b".into(),
            model_hash: dexdo_core::model_hash_for("qwen/qwen3-32b"),
            inference_order_book: format!("0:{}", account('1')),
            root_model: format!("0:{}", account('2')),
            token_contract: token_contract.clone(),
            seller_note: note_addr.clone(),
            nonce: 7,
            // A thousand SHELL a tick: the manifest carries whole SHELL.
            price_per_tick: 1000 * dexdo_core::PRICE_STEP,
            max_ticks: 1024,
        };
        let handle = deals::DealHandle {
            version: deals::DEAL_HANDLE_VERSION,
            handle: deals::make_handle_id(&token_contract, deals::DealHandleRole::Seller),
            role: deals::DealHandleRole::Seller,
            network: "net-a".into(),
            token_contract: token_contract.clone(),
            note_addr: note_addr.clone(),
            frame_model: "qwen/qwen3-32b".into(),
            model_hash: Some(dexdo_core::model_hash_for("qwen/qwen3-32b")),
            order_book: Some(market.inference_order_book.clone()),
            root_model: Some(market.root_model.clone()),
            market: Some(market.clone()),
            contracts: "manifest/deployed.manifest.json".into(),
            endpoint: Some(deals::DealEndpointInfo {
                kind: "gateway".into(),
                value: "127.0.0.1:38671".into(),
            }),
            created_order_ids: vec![],
            created_at_unix: 1,
        };
        let path = deals::save_deal_handle(&deals_dir, &handle).unwrap();

        let markets =
            seller_market_handles(Some(&deals_dir), &note_addr, "127.0.0.1:45677").unwrap();
        assert_eq!(
            markets.get(&deals::normalize_addr(&token_contract)),
            Some(&market),
            "the deal a re-bound service owns must still be reconstructed"
        );

        let reread = deals::load_deal_handle(&path).unwrap();
        assert_eq!(
            reread.endpoint.as_ref().map(|e| e.value.as_str()),
            Some("127.0.0.1:45677"),
            "the record must name the address this run actually serves the deal from"
        );
        assert_eq!(
            deals::DealHandle {
                endpoint: handle.endpoint.clone(),
                ..reread.clone()
            },
            handle,
            "adopting the run's gateway must change nothing else in the handle"
        );

        // Nothing to adopt is not a licence to rewrite: the same address leaves the file alone.
        let before = std::fs::read(&path).unwrap();
        seller_market_handles(Some(&deals_dir), &note_addr, "127.0.0.1:45677").unwrap();
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "an unchanged gateway must not rewrite the handle"
        );
    }

    fn signal_test_token_contract() -> String {
        format!("0:{}", "a".repeat(64))
    }

    #[test]
    fn ordinary_advance_stops_at_the_matched_funded_ticks_not_the_listing_depth() {
        let listing_ticks = 1_024u128;
        let matched_ticks = 3u128;
        let tick_size = dexdo_core::DobParams::canonical().tick_size;
        let budget = funded_tick_budget(
            "0:funded-budget",
            matched_ticks * u128::from(tick_size),
            tick_size,
        )
        .unwrap();

        assert_eq!(budget, matched_ticks);
        assert_ne!(budget, listing_ticks);
    }

    #[tokio::test]
    async fn subscription_keeper_observer_updates_week_cap_and_removes_terminal_record() {
        let gateway = Arc::new(dexdo::seller::gateway::GatewayState::new());
        let token_contract = "0:subscription-capacity-observer".to_string();
        let buyer = LocalNote::generate();
        let state = dexdo_core::DealChainState {
            funded: true,
            opened: true,
            probe_accepted: true,
            disputed: false,
            deposit: 1,
            finalized_owed: 0,
            tokens_final: dexdo_core::TICK_SIZE,
            tokens_pending: dexdo_core::TICK_SIZE,
            probe_tick: 0,
            funded_time: Some(1),
            probe_time: 1,
            last_claim_time: 1,
            dispute_time: 0,
        };
        let week_zero = dexdo_core::DealSubscription {
            deal_flags: dexdo_core::order_flags::SUBSCRIPTION,
            sub_weeks: dexdo_core::SUBSCRIPTION_WEEKS,
            week_index: 0,
            tokens_per_week: 2 * dexdo_core::TICK_SIZE,
            funded_tokens: u128::from(dexdo_core::SUBSCRIPTION_WEEKS) * 2 * dexdo_core::TICK_SIZE,
            tokens_paid: 0,
            period_start: 1,
            week_base_tokens: 0,
        };
        gateway
            .register_stream(&token_contract, buyer.pubkey(), 100, state, week_zero)
            .unwrap();

        let week_one = dexdo_core::DealSubscription {
            week_index: 1,
            tokens_paid: 2 * dexdo_core::TICK_SIZE,
            week_base_tokens: dexdo_core::TICK_SIZE,
            ..week_zero
        };
        let snapshot = dexdo_core::DealChainSnapshot {
            account_code_hash: "test-code".to_string(),
            account_boc_hash: "test-boc".to_string(),
            state,
            subscription: week_one,
            seller_bond: dexdo_core::DealSellerBond {
                bond_funded: true,
                bond_held: 1,
                bond_required: 1,
            },
            buyer_bond: dexdo_core::DealBuyerBond {
                bond_held: 1,
                bond_required: 1,
            },
        };
        let observer = SubscriptionCapacityObserver {
            gateway: gateway.clone(),
        };
        dexdo::seller::SubscriptionKeeperObserver::observe(
            &observer,
            &token_contract,
            Some(&snapshot),
        )
        .await
        .unwrap();
        assert_eq!(
            gateway
                .reconcile_subscription_capacity(&token_contract, state, week_one)
                .unwrap()
                .unwrap()
                .authoritative_cap,
            3 * dexdo_core::TICK_SIZE
        );

        let final_week = dexdo_core::DealSubscription {
            week_index: dexdo_core::SUBSCRIPTION_WEEKS,
            tokens_paid: week_zero.funded_tokens,
            week_base_tokens: state.tokens_pending,
            ..week_zero
        };
        let final_snapshot = dexdo_core::DealChainSnapshot {
            subscription: final_week,
            ..snapshot.clone()
        };
        dexdo::seller::SubscriptionKeeperObserver::observe(
            &observer,
            &token_contract,
            Some(&final_snapshot),
        )
        .await
        .unwrap();
        let final_capacity = gateway
            .reconcile_subscription_capacity(&token_contract, state, final_week)
            .unwrap()
            .unwrap();
        assert_eq!(final_capacity.authoritative_cap, state.tokens_pending);
        assert_eq!(final_capacity.available().unwrap(), 0);

        dexdo::seller::SubscriptionKeeperObserver::observe(&observer, &token_contract, None)
            .await
            .unwrap();

        let nonce = vec![7; 32];
        gateway.auth.issue_challenge(&token_contract, nonce.clone());
        let signature = buyer.sign(&dexdo::seller::auth::challenge_bytes(
            &token_contract,
            &nonce,
        ));
        let service = dexdo::seller::gateway::GatewayService::new(gateway);
        let error = dexdo_proto::Gateway::open_stream(
            &service,
            tonic::Request::new(dexdo_proto::StreamRequest {
                token_contract,
                nonce,
                signature: signature.0.to_vec(),
                request: Some(dexdo_proto::CanonRequest {
                    messages: Vec::new(),
                    params: Some(dexdo_proto::SamplingParams {
                        max_tokens: 1,
                        ..dexdo_proto::SamplingParams::default()
                    }),
                }),
                billing_grant_tokens: 1,
            }),
        )
        .await
        .err()
        .expect("terminal capacity removal must reject before upstream");
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        assert!(error.message().contains("capacity is not registered"));
    }

    async fn existing_resting_offer(
        name: &str,
        note: Arc<LocalNote>,
        gateway_advertise: String,
    ) -> (
        MockChainBackend,
        SellerConfig,
        RestingOfferIdentity,
        tempfile::TempDir,
    ) {
        let token_contract = signal_test_token_contract();
        // `temp_dir()/dexdo-668-<name>-<pid>` is not unique -- a container's PID namespace
        // hands the test process the same small pid every run, and to both CI pipelines at once --
        // and nothing ever removed it. `tempfile` gives a random name and removes it on drop.
        let root = tempfile::Builder::new()
            .prefix(name)
            .tempdir()
            .expect("seller test directory");
        let root_path = root.path();
        let chain = MockChainBackend::new(
            root_path.join("endpoints.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let owner = format!(
            "0:{}",
            note.pubkey()
                .ed
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let config = SellerConfig {
            token_contract: token_contract.clone(),
            price_per_tick: dexdo_core::PRICE_STEP as u64,
            max_ticks: 1024,
            subscription: false,
            gateway_advertise,
            mock_token_count: 8,
        };
        let startup = prepare_seller_offer(note.as_ref(), &chain, &config, Some(&owner))
            .await
            .expect("post existing resting SELL");
        let order_id = match startup {
            SellerOfferStartup::Posted {
                outcome: Some(SellOfferOutcome::Rested { order_id }),
            } => order_id,
            other => panic!("expected one resting SELL, got {other:?}"),
        };
        (
            chain,
            config,
            RestingOfferIdentity {
                owner_note: owner,
                token_contract,
                order_id,
            },
            root,
        )
    }

    #[tokio::test]
    async fn occupied_gateway_startup_cancels_existing_exact_sell_without_repost() {
        let note = Arc::new(LocalNote::generate());
        let (chain, config, identity, _root) =
            existing_resting_offer("occupied-bind", note.clone(), "127.0.0.1:1".to_string()).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let outcome = start_seller_gateway_with_liveness(
            dexdo::seller::start_gateway_with_note(addr, dexdo::seller::UpstreamConfig::Mock, note),
            &chain,
            &config,
            Some(&identity),
            std::future::pending(),
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            SellerGatewayStartup::Stopped {
                reason: dexdo::seller::liveness::RestingStopReason::Health(
                    dexdo::seller::liveness::HealthFailure {
                        component: dexdo::seller::liveness::HealthComponent::GatewayTask,
                        ..
                    }
                ),
                disposition: dexdo::seller::liveness::CancellationDisposition::Cancelled,
            }
        ));
        assert!(chain
            .raw_resting_sell_orders_for_tc(&config.token_contract)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            chain
                .confirm_offer_outcome(&config.token_contract)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn startup_signal_cancels_existing_exact_sell_before_gateway_ready() {
        let note = Arc::new(LocalNote::generate());
        let (chain, config, identity, _root) =
            existing_resting_offer("startup-signal", note, "127.0.0.1:1".to_string()).await;

        let outcome = start_seller_gateway_with_liveness(
            std::future::pending::<anyhow::Result<dexdo::seller::RunningSeller>>(),
            &chain,
            &config,
            Some(&identity),
            std::future::ready(()),
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            SellerGatewayStartup::Stopped {
                reason: dexdo::seller::liveness::RestingStopReason::Shutdown,
                disposition: dexdo::seller::liveness::CancellationDisposition::Cancelled,
            }
        ));
        assert!(chain
            .raw_resting_sell_orders_for_tc(&config.token_contract)
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn unknown_cancellation_event_is_structured_and_preserves_rejection() {
        let event = seller_liveness_event(
            "0:tc",
            Some("0:owner"),
            Some(17),
            "seller_offer_terminal",
            None,
            "unknown_failure",
            Some("cancel_submit=rejected: owner check failed"),
        );

        assert_eq!(event["token_contract"], "0:tc");
        assert_eq!(event["owner_note"], "0:owner");
        assert_eq!(event["order_id"], "17");
        assert_eq!(event["outcome"], "unknown_failure");
        assert_eq!(
            event["known_result"],
            "cancel_submit=rejected: owner check failed"
        );
    }

    #[tokio::test]
    async fn consumed_match_race_shutdown_does_not_require_a_second_signal() {
        let shutdown = std::future::pending::<()>();
        tokio::pin!(shutdown);
        tokio::time::timeout(
            std::time::Duration::from_millis(20),
            react_to_seller_shutdown_signal(shutdown.as_mut(), true, "0:tc"),
        )
        .await
        .expect("an already-observed shutdown must complete immediately");
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "subprocess helper for seller signal regression tests"]
    async fn seller_signal_child() {
        if std::env::var_os("DEXDO_SELLER_SIGNAL_CHILD").is_none() {
            return;
        }
        use dexdo::seller::{start_gateway_with_note, SellerMatchWatchConfig, UpstreamConfig};
        use std::time::Duration;

        let token_contract = signal_test_token_contract();
        let note = Arc::new(LocalNote::generate());
        let seller = start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            note.clone(),
        )
        .await
        .expect("start signal-test gateway");
        let (chain, config, identity, root) =
            existing_resting_offer("signal", note, seller.listen_addr.to_string()).await;
        let root = root.path();
        let buyer_note = LocalNote::generate();
        chain
            .place_buy(&token_contract, &buyer_note)
            .await
            .expect("match signal-test SELL before starting the pool");
        let chain = Arc::new(chain);
        let order_id = identity.order_id;
        let contracts = root.join("unused-contracts.json");
        let deal = SellerPoolDeal {
            chain: chain.clone(),
            cfg: config.clone(),
            watch: SellerMatchWatchConfig {
                cursor_path: root.join("seller-watch.json"),
                poll_interval: Duration::from_millis(10),
            },
            upstream: UpstreamConfig::Mock,
            nonce: 1,
            market: None,
        };
        let context = SellerPoolContext {
            deals_dir: Some(root),
            contracts: &contracts,
            note_addr: &identity.owner_note,
            frame_model: "mock",
            gateway_advertise: &config.gateway_advertise,
            advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::default(),
            recover_publication: None,
        };
        let mut provisioner = |_: String, _: u64, _: u64, _: u64| {
            futures::future::ready(Err::<
                (dexdo_core::MarketManifest, Arc<dyn ChainBackend>),
                anyhow::Error,
            >(anyhow::anyhow!(
                "unexpected residual provision"
            )))
        };
        let shutdown = crate::operator_shutdown_signal().fuse();
        tokio::pin!(shutdown);
        run_seller_pool(
            &seller,
            vec![deal],
            context,
            &pool_test_policy(1),
            &mut provisioner,
            shutdown.as_mut(),
            &mut false,
        )
        .await
        .expect("seller pool signal shutdown completes");
        assert!(
            chain
                .raw_resting_sell_orders_for_tc(&token_contract)
                .await
                .expect("reread signal-test book")
                .is_empty(),
            "signal test must leave no resting SELL before process exit"
        );
        println!("seller-signal-order-absent order_id={order_id}");
    }

    #[cfg(unix)]
    fn assert_seller_signal_emits_shutdown_jsonl(signal: &str) {
        use std::io::{BufRead as _, Read as _};
        use std::process::{Command, Stdio};

        let mut child = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "cli::seller::tests::seller_signal_child",
                "--ignored",
                "--nocapture",
            ])
            .env("DEXDO_SELLER_SIGNAL_CHILD", "1")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn seller signal regression child");
        let stdout = child.stdout.take().expect("capture child stdout");
        let mut stdout = std::io::BufReader::new(stdout);
        let mut output = String::new();
        loop {
            let mut line = String::new();
            assert_ne!(
                stdout.read_line(&mut line).expect("read child readiness"),
                0,
                "seller child exited before readiness; output={output}"
            );
            output.push_str(&line);
            if line.contains("seller_match_opened token_contract=") {
                break;
            }
        }

        let signal = Command::new("kill")
            .args([signal, &child.id().to_string()])
            .status()
            .expect("send signal to seller child");
        assert!(signal.success(), "kill command failed: {signal}");
        stdout
            .read_to_string(&mut output)
            .expect("read seller child output");
        let status = child.wait().expect("wait for seller child");
        assert!(
            status.success(),
            "seller child failed: {status}; output={output}"
        );
        assert!(
            output.contains("seller-signal-order-absent order_id="),
            "seller signal path exited with a resting SELL: {output}"
        );
        assert!(
            output.contains("seller_match_opened token_contract="),
            "seller signal path never entered the live pool loop: {output}"
        );

        let shutdown = output
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|event| event["event"] == "stopping")
            .collect::<Vec<_>>();
        assert_eq!(
            shutdown.len(),
            1,
            "seller pool must emit exactly one stopping event: {output}"
        );
        assert_eq!(
            shutdown[0],
            serde_json::json!({
                "event": "stopping",
                "role": "seller",
                "token_contract": signal_test_token_contract(),
                "reason": "signal"
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn seller_sigint_emits_shutdown_jsonl() {
        assert_seller_signal_emits_shutdown_jsonl("-INT");
    }

    #[cfg(unix)]
    #[test]
    fn seller_sigterm_emits_shutdown_jsonl() {
        assert_seller_signal_emits_shutdown_jsonl("-TERM");
    }

    #[test]
    fn seller_ready_is_emitted_only_for_confirmed_resting_sell() {
        let identity = dexdo::seller::liveness::RestingOfferIdentity {
            owner_note: "0:owner".to_string(),
            token_contract: "0:tc".to_string(),
            order_id: 17,
        };
        let resumed = dexdo::seller::SellerOfferStartup::ResumedResting { order_id: 17 };
        let posted = dexdo::seller::SellerOfferStartup::Posted {
            outcome: Some(SellOfferOutcome::Rested { order_id: 17 }),
        };

        assert!(
            seller_ready_line("0:tc", "gateway", "listen", &resumed, Some(&identity))
                .unwrap()
                .contains("readiness=resumed_resting_offer")
        );
        assert!(
            seller_ready_line("0:tc", "gateway", "listen", &posted, Some(&identity))
                .unwrap()
                .contains("readiness=exact_tc_offer_accepted")
        );
        assert!(seller_ready_line(
            "0:tc",
            "gateway",
            "listen",
            &dexdo::seller::SellerOfferStartup::ResumedFunded,
            None,
        )
        .is_none());
        assert!(seller_ready_line(
            "0:tc",
            "gateway",
            "listen",
            &dexdo::seller::SellerOfferStartup::Posted {
                outcome: Some(SellOfferOutcome::Matched),
            },
            None,
        )
        .is_none());
    }

    #[test]
    fn seller_offer_placed_reports_rested_with_order_id() {
        assert_eq!(
            seller_offer_outcome_line(&SellOfferOutcome::Rested { order_id: 835 }),
            "seller_offer_outcome RESTED order_id=835"
        );
    }

    #[test]
    fn seller_offer_immediate_match_reports_matched() {
        assert_eq!(
            seller_offer_outcome_line(&SellOfferOutcome::Matched),
            "seller_offer_outcome MATCHED"
        );
    }

    #[test]
    fn seller_offer_startup_line_covers_every_resting_startup() {
        assert_eq!(
            seller_offer_startup_line(&dexdo::seller::SellerOfferStartup::Posted {
                outcome: Some(SellOfferOutcome::Rested { order_id: 5 }),
            })
            .as_deref(),
            Some("seller_offer_outcome RESTED order_id=5")
        );
        assert_eq!(
            seller_offer_startup_line(&dexdo::seller::SellerOfferStartup::ResumedResting {
                order_id: 5,
            })
            .as_deref(),
            Some("seller_offer_resume RESTING order_id=5")
        );
        assert_eq!(
            seller_offer_startup_line(&dexdo::seller::SellerOfferStartup::Posted {
                outcome: Some(SellOfferOutcome::Matched),
            })
            .as_deref(),
            Some("seller_offer_outcome MATCHED")
        );
        assert_eq!(
            seller_offer_startup_line(&dexdo::seller::SellerOfferStartup::ResumedFunded),
            None
        );
        assert_eq!(
            seller_offer_startup_line(&dexdo::seller::SellerOfferStartup::Posted { outcome: None }),
            None
        );
    }

    const STARTUP_CHILD_CASE: &str = "DEXDO_TEST_798_STARTUP_CASE";
    const STARTUP_CHILD_DONE: &str = "seller-offer-startup-child-done case=";

    /// regression driver: run the production `prepare_pool_deal` dispatch in a child process so
    /// the assertions read the bytes the seller actually writes to stdout, not a formatter in isolation
    /// (the emitted-nowhere formatter is exactly what made the release gate hang).
    #[tokio::test]
    #[ignore = "subprocess helper for seller offer startup announcement regressions"]
    async fn seller_offer_startup_child() {
        let Some(case) = std::env::var_os(STARTUP_CHILD_CASE) else {
            return;
        };
        use dexdo::seller::{start_gateway_with_note, SellerMatchWatchConfig, UpstreamConfig};
        use std::time::Duration;

        let case = case.to_string_lossy().into_owned();
        let note = Arc::new(LocalNote::generate());
        let seller = start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            note.clone(),
        )
        .await
        .expect("start startup-announcement gateway");
        // `<name>-<pid>` is not unique across containers; `tempfile` is, and it cleans up.
        let root = tempfile::tempdir().expect("seller test directory");
        let root = root.path();
        let chain = MockChainBackend::new(
            root.join("endpoints.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let owner = format!(
            "0:{}",
            note.pubkey()
                .ed
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let token_contract = signal_test_token_contract();
        let gateway_advertise = seller.listen_addr.to_string();
        let cfg = SellerConfig {
            token_contract: token_contract.clone(),
            price_per_tick: dexdo_core::PRICE_STEP as u64,
            max_ticks: 1024,
            subscription: false,
            gateway_advertise: gateway_advertise.clone(),
            mock_token_count: 8,
        };
        // Bring the exact TC into the authoritative startup state under test; "fresh" leaves it vacant
        // so the pool itself has to post.
        if case != "fresh" {
            let seeded = prepare_seller_offer(note.as_ref(), &chain, &cfg, Some(&owner))
                .await
                .expect("seed the exact resting SELL");
            assert!(
                matches!(
                    seeded,
                    SellerOfferStartup::Posted {
                        outcome: Some(SellOfferOutcome::Rested { .. }),
                    }
                ),
                "seed must leave one exact resting SELL, got {seeded:?}"
            );
            if case == "funded" {
                chain
                    .place_buy(&token_contract, &LocalNote::generate())
                    .await
                    .expect("match the seeded SELL before the pool prepares it");
            }
        }
        let deal = SellerPoolDeal {
            chain: Arc::new(chain),
            cfg,
            watch: SellerMatchWatchConfig {
                cursor_path: root.join("seller-watch.json"),
                poll_interval: Duration::from_millis(10),
            },
            upstream: UpstreamConfig::Mock,
            nonce: 1,
            market: None,
        };
        let contracts = root.join("unused-contracts.json");
        let context = SellerPoolContext {
            deals_dir: Some(root),
            contracts: &contracts,
            note_addr: &owner,
            frame_model: "mock",
            gateway_advertise: &gateway_advertise,
            advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::default(),
            recover_publication: None,
        };
        let shutdown = futures::future::pending::<()>();
        tokio::pin!(shutdown);
        let identity = prepare_pool_deal(
            &seller,
            &deal,
            &context,
            false,
            shutdown.as_mut(),
            &mut false,
        )
        .await
        .expect("prepare the pool deal under test");
        match case.as_str() {
            "fresh" | "resume" => assert!(
                identity.is_some(),
                "case {case} must leave one exact resting SELL identity"
            ),
            "funded" => assert!(identity.is_none(), "a funded TC has no resting SELL"),
            other => panic!("unknown startup child case: {other}"),
        }
        seller.server_task.abort();
        println!("{STARTUP_CHILD_DONE}{case}");
        let _ = std::io::stdout().flush();
    }

    fn seller_offer_startup_child_output(case: &str) -> String {
        use std::process::Command;

        let output = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "cli::seller::tests::seller_offer_startup_child",
                "--ignored",
                "--show-output",
            ])
            .env(STARTUP_CHILD_CASE, case)
            .output()
            .expect("spawn seller offer startup child");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.status.success(),
            "startup child case {case} failed: {text}"
        );
        assert!(
            text.contains(&format!("{STARTUP_CHILD_DONE}{case}")),
            "startup child case {case} never reached the assertion point: {text}"
        );
        text
    }

    /// the fresh post that the chain accepts as resting (`readiness=exact_tc_offer_accepted`)
    /// must print `seller_offer_outcome RESTED order_id=<id>` -- the exact line the two-runner release
    /// gate waits on -- exactly once, before `seller_ready`.
    #[test]
    fn accepted_fresh_offer_prints_seller_offer_outcome_rested_before_ready() {
        let output = seller_offer_startup_child_output("fresh");
        assert_eq!(
            output
                .matches("seller_offer_outcome RESTED order_id=")
                .count(),
            1,
            "the accepted fresh offer must announce its resting order exactly once: {output}"
        );
        let rested = output
            .lines()
            .find(|line| line.starts_with("seller_offer_outcome RESTED order_id="))
            .expect("resting announcement line");
        let order_id = rested
            .trim_end()
            .rsplit_once('=')
            .expect("order id suffix")
            .1
            .to_string();
        assert!(
            order_id.chars().all(|c| c.is_ascii_digit()) && !order_id.is_empty(),
            "the gate parses this order id as decimal digits: {rested}"
        );
        let ready = output
            .lines()
            .find(|line| line.starts_with("seller_ready token_contract="))
            .expect("seller_ready line");
        assert!(
            ready.contains(&format!(
                "order_id={order_id} readiness=exact_tc_offer_accepted"
            )),
            "the announced order must be the one the seller declares ready: {output}"
        );
        assert!(
            output.find(rested).unwrap() < output.find(ready).unwrap(),
            "the gate waits for the resting order before readiness: {output}"
        );
        assert!(
            !output.contains("seller_offer_resume RESTING"),
            "a fresh post is not a resume: {output}"
        );
    }

    /// adopting an existing exact raw resting SELL announces that resting order exactly once too
    /// -- on its own `seller_offer_resume RESTING` contract, which relies on to prove a restart did
    /// not re-post (`seller_offer_outcome RESTED` stays the fresh-post-only marker).
    #[test]
    fn resumed_resting_offer_prints_its_resting_order_exactly_once() {
        let output = seller_offer_startup_child_output("resume");
        assert_eq!(
            output
                .matches("seller_offer_resume RESTING order_id=")
                .count(),
            1,
            "the adopted resting order must be announced exactly once: {output}"
        );
        let resumed = output
            .lines()
            .find(|line| line.starts_with("seller_offer_resume RESTING order_id="))
            .expect("resume announcement line");
        let order_id = resumed.trim_end().rsplit_once('=').expect("order id").1;
        let ready = output
            .lines()
            .find(|line| line.starts_with("seller_ready token_contract="))
            .expect("seller_ready line");
        assert!(
            ready.contains(&format!(
                "order_id={order_id} readiness=resumed_resting_offer"
            )),
            "the resumed order must be the one the seller declares ready: {output}"
        );
        assert!(
            !output.contains("seller_offer_outcome RESTED"),
            ": a resume must stay distinguishable from a fresh post: {output}"
        );
    }

    /// negative: a TC already funded by a matched buyer has no resting SELL, so the seller must
    /// announce no offer outcome at all.
    #[test]
    fn funded_startup_prints_no_offer_announcement() {
        let output = seller_offer_startup_child_output("funded");
        assert!(
            !output.contains("seller_offer_outcome ")
                && !output.contains("seller_offer_resume ")
                && !output.contains("seller_ready "),
            "nothing rested, so nothing may be announced: {output}"
        );
    }

    /// regression: `run_seller` delegates every mock/real deal to the shared pool and never restores
    /// the old bounded one-deal match wait.
    #[test]
    fn seller_run_path_uses_gateway_watcher_not_bounded_read_match() {
        let source = include_str!("seller.rs");
        let start = source
            .find("pub(crate) async fn run_seller")
            .expect("run_seller present");
        let end = source[start..]
            .find("#[cfg(test)]\nmod tests")
            .map(|offset| start + offset)
            .expect("run_seller end marker present");
        let body = &source[start..end];

        assert!(
            body.contains("run_seller_pool"),
            "seller match wait must be owned by the shared pool"
        );
        assert!(
            body.contains("seller_watch_cursor_path"),
            "gateway watcher must persist a cursor"
        );
        assert!(
            body.contains("DEFAULT_MATCH_POLL_INTERVAL"),
            "gateway watcher must use the ~30s default poll interval"
        );
        assert!(
            !body.contains("read_match(&token_contract)"),
            "run_seller must not block on the old read_match loop"
        );
        assert!(
            !body.contains("DEAL_WAIT_SECS"),
            "run_seller must not carry the old 300s seller deadline"
        );
    }

    /// regression: the withdrawn-note guard must win over an unavailable per-deal TC.
    #[test]
    fn seller_withdrawn_guard_precedes_first_tc_terms_read() {
        let source = include_str!("seller.rs");
        let start = source
            .find("pub(crate) async fn run_seller")
            .expect("run_seller present");
        let end = source[start..]
            .find("#[cfg(test)]\nmod tests")
            .map(|offset| start + offset)
            .expect("run_seller end marker present");
        let body = &source[start..end];
        let withdrawn_guard = body
            .find("chain.assert_note_can_post_sell_offer().await")
            .expect("withdrawn-note guard present");
        let first_terms_read = body
            .find("chain.sell_offer_terms(&token_contract).await")
            .expect("initial per-deal TC terms read present");

        assert!(
            withdrawn_guard < first_terms_read,
            "withdrawn-note guard must run before the initial per-deal TC terms read"
        );
        let guard_to_terms = &body[withdrawn_guard..first_terms_read];
        assert!(
            guard_to_terms.contains("tokio::select!")
                && guard_to_terms.contains("_ = shutdown.as_mut()")
                && guard_to_terms.contains("shutdown_requested = true"),
            "the early withdrawn-note read must be shutdown-cancellable before TC term reads"
        );
    }

    #[cfg(unix)]
    const RESTART_CHILD_CASE: &str = "DEXDO_TEST_668_RESTART_CASE";
    #[cfg(unix)]
    const RESTART_MISSING_KEY: &str = "DEXDO_TEST_668_MISSING_MODEL_CREDENTIAL";
    #[cfg(unix)]
    const RESTART_NOTE_SEED: [u8; 32] = [0x42; 32];

    #[cfg(unix)]
    async fn exercise_restart_preflight(case: &str) -> Option<String> {
        let note = Arc::new(
            dexdo_core::NoteTree::from_secret_hex(&hex::encode(RESTART_NOTE_SEED))
                .unwrap()
                .node(0)
                .unwrap(),
        );
        let (chain, config, identity, root) =
            existing_resting_offer(case, note.clone(), "127.0.0.1:0".to_string()).await;
        let root = root.path();
        crate::cli::support::write_owner_only_key_fixture(
            &root.join("note.key"),
            &hex::encode(RESTART_NOTE_SEED),
        );
        std::fs::write(
            root.join("models.json"),
            serde_json::to_vec(&serde_json::json!({
                "models": {"restart-model": {
                    "frame_model": "restart-model",
                    "base_url": "https://example.invalid",
                    "served_model": "restart-model",
                    "api_key_env": RESTART_MISSING_KEY,
                    "tokenizer_family": "test",
                    "price_per_tick": dexdo_core::PRICE_STEP
                }}
            }))
            .unwrap(),
        )
        .unwrap();
        let control = SellerConfig {
            token_contract: format!("0:{}", "b".repeat(64)),
            price_per_tick: config.price_per_tick,
            max_ticks: config.max_ticks,
            subscription: false,
            gateway_advertise: config.gateway_advertise.clone(),
            mock_token_count: config.mock_token_count,
        };
        prepare_seller_offer(note.as_ref(), &chain, &control, Some(&identity.owner_note))
            .await
            .expect("control SELL rests");
        let control_row = chain
            .raw_resting_sell_orders_for_tc(&control.token_contract)
            .await
            .unwrap()
            .remove(0);
        let result = super::run_seller(crate::cli::args::SellerArgs {
            mock: crate::cli::args::MockFlags {
                mock_model: false,
                mock_chain: true,
            },
            identity: crate::cli::args::IdentityArgs {
                note_key: Some(root.join("note.key")),
                note_index: 0,
                note_addr: None,
            },
            registry: crate::cli::args::ModelRegistryValidationArgs::default(),
            gateway_listen: "127.0.0.1:0".parse().unwrap(),
            gateway_advertise: None,
            allow_private_advertise: false,
            require_advertise_probe: false,
            endpoints_file: Some(root.join("endpoints.json")),
            deals_dir: Some(root.join("deals")),
            token_contract: Some(config.token_contract.clone()),
            market: None,
            nonce: None,
            subscription: false,
            price_per_tick: config.price_per_tick,
            mock_token_count: config.mock_token_count,
            model: Some("restart-model".to_string()),
            allow_unverified_model: false,
            models: root.join("models.json"),
            policy: None,
            recover_publication: None,
            confirm_recover_publication: false,
        })
        .await;
        let error = match case {
            "missing-credential" => {
                let error = format!("{:#}", result.expect_err("missing key must fail"));
                assert!(error.contains(RESTART_MISSING_KEY), "{error}");
                assert!(
                    error.contains("cancellation_disposition=cancelled"),
                    "{error}"
                );
                Some(error)
            }
            "pending-preflight-signal" | "pending-withdrawn-signal" => {
                result.expect("signal shutdown must terminate cleanly after exact cancellation");
                None
            }
            _ => panic!("unknown restart child case: {case}"),
        };
        let reopened = MockChainBackend::new(
            root.join("endpoints.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        assert!(reopened
            .raw_resting_sell_orders_for_tc(&identity.token_contract)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            reopened
                .confirm_offer_outcome(&identity.token_contract)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            reopened
                .raw_resting_sell_orders_for_tc(&control.token_contract)
                .await
                .unwrap(),
            vec![control_row]
        );
        error
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "subprocess helper for seller restart/preflight regression"]
    async fn seller_restart_preflight_child() {
        let Some(case) = std::env::var_os(RESTART_CHILD_CASE) else {
            return;
        };
        exercise_restart_preflight(&case.to_string_lossy()).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_missing_credentials_and_preflight_signal_use_exact_cancellation_path() {
        use std::io::{BufRead as _, Read as _};
        use std::process::{Command, Stdio};

        assert!(std::env::var_os(RESTART_MISSING_KEY).is_none());
        let error = exercise_restart_preflight("missing-credential")
            .await
            .unwrap();
        assert!(!error.contains(&hex::encode(RESTART_NOTE_SEED)), "{error}");

        for (case, pending_env, pending_marker) in [
            (
                "pending-preflight-signal",
                "DEXDO_TEST_668_PENDING_SELLER_PREFLIGHT",
                "seller-restart-preflight-pending",
            ),
            (
                "pending-withdrawn-signal",
                "DEXDO_TEST_335_PENDING_EARLY_WITHDRAWN_READ",
                "seller-early-withdrawn-read-pending",
            ),
        ] {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cli::seller::tests::seller_restart_preflight_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env(RESTART_CHILD_CASE, case)
                .env_remove("DEXDO_TEST_668_PENDING_SELLER_PREFLIGHT")
                .env_remove("DEXDO_TEST_335_PENDING_EARLY_WITHDRAWN_READ")
                .env(pending_env, "1")
                .env_remove(RESTART_MISSING_KEY)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
            let mut output = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(stdout.read_line(&mut line).unwrap(), 0, "{output}");
                output.push_str(&line);
                if line.contains(pending_marker) {
                    break;
                }
            }
            assert!(Command::new("kill")
                .args(["-TERM", &child.id().to_string()])
                .status()
                .unwrap()
                .success());
            stdout.read_to_string(&mut output).unwrap();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut output)
                .unwrap();
            assert!(child.wait().unwrap().success(), "{case}: {output}");
            assert!(
                output.contains("\"outcome\":\"cancelled\""),
                "{case}: {output}"
            );
            assert!(
                output.contains("\"event\":\"stopping\""),
                "{case}: {output}"
            );
            assert!(!output.contains("seller_ready "), "{case}: {output}");
            assert!(
                !output.contains("seller_offer_outcome RESTED"),
                "{case}: {output}"
            );
            assert!(
                !output.contains(&hex::encode(RESTART_NOTE_SEED)),
                "{case}: {output}"
            );
        }
    }

    struct PoolTestBackend {
        owner_fills: Arc<Mutex<Vec<(i64, MatchedFill)>>>,
        token_contract: String,
        price_per_tick: u64,
        offered_ticks: u64,
        matched_ticks: u64,
        buyer_pubkey: NotePubkey,
        exists: AtomicBool,
        matched: AtomicBool,
        opened: AtomicBool,
        probe_accepted: AtomicBool,
        post_calls: AtomicU64,
        open_calls: AtomicU64,
        seller_stop_calls: AtomicU64,
        buyer_stop_calls: AtomicU64,
        open_fail_once: AtomicBool,
        inspection_fail_once: AtomicBool,
        created_at: i64,
        /// The immutable terminal receipt the destroyed contract left behind. It outlives
        /// `exists`, exactly as the on-chain event outlives the account.
        terminal_settlement: Mutex<Option<dexdo_core::DealTerminalSettlement>>,
        /// (load-bearing): report one resting SELL row for this TokenContract.

        /// Default `false` keeps the inherited `ChainBackend::raw_resting_sell_orders_for_tc`
        /// behaviour, which is an empty list, so every fixture written before this field sees
        /// exactly what it saw. Without a resting row a supervised offer is ABSENT, and the
        /// supervisor leaves down a path that never consults its stop.
        rests: AtomicBool,
        /// (load-bearing): give that resting row a deadline far in the future.

        /// Default `false` leaves the row's deadline at 0, which `order_deadline_is_live` reads as
        /// already expired -- and an expired offer returns through the expiry arm, again without
        /// consulting the stop. Only a row that is BOTH resting and unexpired reaches the branch
        /// where an operator stop is the thing that ends the watch.
        never_expires: AtomicBool,
        /// (NOT load-bearing -- speed only).

        /// This field does not affect any assertion: a cancellation that is never confirmed still
        /// returns `Stopped { reason: Shutdown,.. }`, because `unknown_cancellation` yields a
        /// DISPOSITION and the reason is taken from the trigger. What it changes is how long that
        /// takes. Without it the supervisor spends the whole `health_cycle_timeout` budget --
        /// 60 seconds -- polling for a terminal fact this mock will never produce. With it the row
        /// disappears on the first read after a cancel, and the wait collapses to immediate.
        cancel_confirms: AtomicBool,
        /// set when the supervisor actually SUBMITS a cancel, so `cancel_confirms` retires
        /// the row for that reason and no other. Keyed on the cancel and not on a read count: the
        /// supervisor's routine liveness polls read the same list, so counting reads retired the row
        /// mid-supervision and the watcher concluded the offer had expired.
        cancel_submitted: AtomicBool,
        /// leg 4: a terminal settlement destroys the account, so `getDeal` stops answering.
        /// `exists` already governs the state getters; this governs the offer-terms getter, so a
        /// deal can be put in the shape a settled parent leaves behind without changing what any
        /// test that only moves `exists` sees.
        getdeal_gone: AtomicBool,
        /// Unknown owner-fill TokenContracts whose terminal account disappearance is scripted by
        /// the pool-entry regressions. Empty for every pre-existing pool test.
        terminal_owner_fills: Mutex<std::collections::HashSet<String>>,
    }

    impl PoolTestBackend {
        /// opt in to a resting, unexpired row. `confirms_cancel` is speed only.
        fn resting_unexpired(self, confirms_cancel: bool) -> Self {
            self.rests.store(true, Ordering::Relaxed);
            self.never_expires.store(true, Ordering::Relaxed);
            self.cancel_confirms
                .store(confirms_cancel, Ordering::Relaxed);
            self
        }

        fn new(
            owner_fills: Arc<Mutex<Vec<(i64, MatchedFill)>>>,
            token_contract: String,
            offered_ticks: u64,
            matched_ticks: u64,
            matched: bool,
            created_at: i64,
        ) -> Self {
            let price_per_tick = 1_000_000_000;
            if matched {
                owner_fills.lock().unwrap().push((
                    created_at,
                    MatchedFill {
                        order_id: u128::from(offered_ticks),
                        token_contract: token_contract.clone(),
                        ticks: u128::from(matched_ticks),
                        price_per_tick: u128::from(price_per_tick),
                    },
                ));
            }
            Self {
                owner_fills,
                token_contract,
                price_per_tick,
                offered_ticks,
                matched_ticks,
                buyer_pubkey: LocalNote::generate().pubkey(),
                exists: AtomicBool::new(true),
                matched: AtomicBool::new(matched),
                opened: AtomicBool::new(false),
                probe_accepted: AtomicBool::new(false),
                post_calls: AtomicU64::new(0),
                open_calls: AtomicU64::new(0),
                seller_stop_calls: AtomicU64::new(0),
                buyer_stop_calls: AtomicU64::new(0),
                open_fail_once: AtomicBool::new(false),
                inspection_fail_once: AtomicBool::new(false),
                // all three default OFF, so every pre-existing fixture keeps the inherited
                // empty-list behaviour unchanged.
                rests: AtomicBool::new(false),
                never_expires: AtomicBool::new(false),
                cancel_confirms: AtomicBool::new(false),
                cancel_submitted: AtomicBool::new(false),
                created_at,
                terminal_settlement: Mutex::new(None),
                getdeal_gone: AtomicBool::new(false),
                terminal_owner_fills: Mutex::new(std::collections::HashSet::new()),
            }
        }

        fn with_probe_burn(self, settlement: (u128, u128, u128)) -> Self {
            let (burned_probe, burned_bond, refund_to_buyer) = settlement;
            *self.terminal_settlement.lock().unwrap() =
                Some(dexdo_core::DealTerminalSettlement::ProbeBurned {
                    burned_probe,
                    burned_bond,
                    refund_to_buyer,
                });
            self
        }

        fn with_terminal_settlement(self, settlement: dexdo_core::DealTerminalSettlement) -> Self {
            *self.terminal_settlement.lock().unwrap() = Some(settlement);
            self
        }

        /// The deal settled and the account went with it: `getDeal` answers nothing from here on,
        /// while whatever receipt it emitted stays queryable.
        fn settle_and_lose_getdeal(&self) {
            self.getdeal_gone.store(true, Ordering::Relaxed);
        }

        fn mark_owner_fill_terminal(&self, token_contract: &str) {
            self.terminal_owner_fills
                .lock()
                .unwrap()
                .insert(token_contract.to_string());
        }

        fn with_inspection_failure(self) -> Self {
            self.inspection_fail_once.store(true, Ordering::Relaxed);
            self
        }

        fn with_open_failure(self) -> Self {
            self.open_fail_once.store(true, Ordering::Relaxed);
            self
        }

        fn matched(&self) -> Match {
            Match {
                token_contract: self.token_contract.clone(),
                buyer_pubkey: self.buyer_pubkey.clone(),
                price_per_tick: self.price_per_tick,
            }
        }
    }

    #[async_trait::async_trait]
    impl ChainBackend for PoolTestBackend {
        // only a fixture that opted into `cancel_confirms` accepts a cancel; every other
        // fixture keeps the trait's default refusal, unchanged.
        async fn cancel_resting_sell_order(
            &self,
            token_contract: &dexdo_core::TokenContract,
            order_id: u128,
        ) -> Result<(), ChainError> {
            if !self.cancel_confirms.load(Ordering::Relaxed) {
                return Err(ChainError::Chain(format!(
                    "exact resting SELL cancellation is not supported for TokenContract {}, order {order_id}",
                    dexdo_core::address::display_self_dapp(token_contract)
                )));
            }
            self.cancel_submitted.store(true, Ordering::Relaxed);
            Ok(())
        }

        // default is the inherited empty list; only a fixture that opts in sees a row.
        async fn raw_resting_sell_orders_for_tc(
            &self,
            token_contract: &dexdo_core::TokenContract,
        ) -> Result<Vec<dexdo_core::OrderBookOrder>, ChainError> {
            if !self.rests.load(Ordering::Relaxed) {
                return Ok(Vec::new());
            }
            // Speed only (see the field's doc): once the supervisor has SUBMITTED its cancel, the
            // row is gone, which is what a confirmed cancellation looks like from its side. It
            // changes no outcome, only how long the outcome takes.
            if self.cancel_confirms.load(Ordering::Relaxed)
                && self.cancel_submitted.load(Ordering::Relaxed)
            {
                return Ok(Vec::new());
            }
            let deadline = if self.never_expires.load(Ordering::Relaxed) {
                i64::MAX as u64
            } else {
                0
            };
            Ok(vec![dexdo_core::OrderBookOrder {
                order_id: 1,
                owner_note: format!("0:{}", "a".repeat(64)),
                token_contract: Some(token_contract.to_string()),
                is_buy: false,
                price_per_tick: u128::from(self.price_per_tick),
                ticks: u128::from(self.offered_ticks),
                escrow: 0,
                deadline,
                flags: 0,
                timestamp: 0,
            }])
        }

        async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
            Ok(Vec::new())
        }

        async fn post_offer(&self, offer: SellOffer, _: &dyn Note) -> Result<(), ChainError> {
            assert_eq!(offer.token_contract, self.token_contract);
            assert_eq!(offer.price_per_tick, self.price_per_tick);
            assert_eq!(offer.max_ticks, self.offered_ticks);
            assert!(!self.matched.swap(true, Ordering::Relaxed));
            self.owner_fills.lock().unwrap().push((
                self.created_at,
                MatchedFill {
                    order_id: u128::from(self.offered_ticks),
                    token_contract: self.token_contract.clone(),
                    ticks: u128::from(self.matched_ticks),
                    price_per_tick: u128::from(self.price_per_tick),
                },
            ));
            self.post_calls.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn confirm_offer_outcome(
            &self,
            _: &TokenContract,
        ) -> Result<Option<SellOfferOutcome>, ChainError> {
            Ok(self
                .matched
                .load(Ordering::Relaxed)
                .then_some(SellOfferOutcome::Matched))
        }

        async fn sell_offer_terms(
            &self,
            _: &TokenContract,
        ) -> Result<Option<(u64, u64)>, ChainError> {
            if self.getdeal_gone.load(Ordering::Relaxed) {
                return Ok(None);
            }
            Ok(Some((self.price_per_tick, self.offered_ticks)))
        }

        async fn read_openable_match_now(
            &self,
            _: &TokenContract,
        ) -> Result<Option<Match>, ChainError> {
            if self.inspection_fail_once.swap(false, Ordering::Relaxed) {
                return Err(ChainError::Transport(
                    "injected restart after provision and before POST".to_string(),
                ));
            }
            Ok(self.matched.load(Ordering::Relaxed).then(|| self.matched()))
        }

        async fn poll_seller_fills(
            &self,
            _: &dyn Note,
            cursor: &mut MatchWatchCursor,
        ) -> Result<Vec<MatchedFill>, ChainError> {
            let mut batch = self
                .owner_fills
                .lock()
                .unwrap()
                .iter()
                .filter(|(created_at, fill)| !cursor.has_seen(*created_at, &fill.token_contract))
                .cloned()
                .collect::<Vec<_>>();
            batch.sort_by(|left, right| {
                left.0
                    .cmp(&right.0)
                    .then_with(|| left.1.token_contract.cmp(&right.1.token_contract))
            });
            cursor.record_seen_batch(
                batch
                    .iter()
                    .map(|(created_at, fill)| (*created_at, fill.token_contract.clone())),
            );
            Ok(batch.into_iter().map(|(_, fill)| fill).collect())
        }

        async fn place_buy(&self, _: &TokenContract, _: &dyn Note) -> Result<(), ChainError> {
            unreachable!("the scripted seller backend matches immediately after post")
        }

        async fn read_match(&self, _: &TokenContract) -> Result<Match, ChainError> {
            Ok(self.matched())
        }

        async fn open_stream(
            &self,
            _: &TokenContract,
            _: Vec<u8>,
            _: &dyn Note,
        ) -> Result<(), ChainError> {
            self.open_calls.fetch_add(1, Ordering::Relaxed);
            if self.open_fail_once.swap(false, Ordering::Relaxed) {
                return Err(ChainError::Transport(
                    "injected per-deal open failure".to_string(),
                ));
            }
            self.opened.store(true, Ordering::Relaxed);
            Ok(())
        }

        async fn read_handover(&self, _: &TokenContract) -> Result<Option<Vec<u8>>, ChainError> {
            Ok(self.opened.load(Ordering::Relaxed).then_some(vec![1]))
        }

        async fn claim_tokens(
            &self,
            _: &TokenContract,
            _: &dyn Note,
            _: u128,
        ) -> Result<(), ChainError> {
            Ok(())
        }

        async fn accept_probe(&self, _: &TokenContract) -> Result<(), ChainError> {
            Ok(())
        }

        async fn stop(&self, _: &TokenContract, _: &dyn Note) -> Result<Settlement, ChainError> {
            self.buyer_stop_calls.fetch_add(1, Ordering::Relaxed);
            unreachable!("the test exits before the fixed probe window")
        }

        async fn seller_stop(
            &self,
            token_contract: &TokenContract,
        ) -> Result<Settlement, ChainError> {
            assert_eq!(token_contract, &self.token_contract);
            self.seller_stop_calls.fetch_add(1, Ordering::Relaxed);
            self.opened.store(false, Ordering::Relaxed);
            Ok(Settlement::AmicableSplit {
                to_seller_ticks: self.matched_ticks,
                to_buyer_refund: 0,
            })
        }

        async fn deal_state(
            &self,
            token_contract: &TokenContract,
        ) -> Result<Option<DealChainState>, ChainError> {
            if self
                .terminal_owner_fills
                .lock()
                .unwrap()
                .contains(token_contract)
            {
                return Ok(None);
            }
            Ok(self.exists.load(Ordering::Relaxed).then(|| DealChainState {
                funded: self.matched.load(Ordering::Relaxed),
                opened: self.opened.load(Ordering::Relaxed),
                probe_accepted: self.probe_accepted.load(Ordering::Relaxed),
                disputed: false,
                deposit: if self.matched.load(Ordering::Relaxed) {
                    u128::from(self.price_per_tick) * u128::from(self.matched_ticks)
                } else {
                    0
                },
                finalized_owed: 0,
                tokens_final: 0,
                tokens_pending: 0,
                probe_tick: 0,
                funded_time: self.matched.load(Ordering::Relaxed).then_some(1),
                probe_time: 0,
                last_claim_time: 0,
                dispute_time: 0,
            }))
        }

        async fn deal_snapshot(
            &self,
            _: &TokenContract,
        ) -> Result<Option<DealChainSnapshot>, ChainError> {
            let Some(state) = self.deal_state(&self.token_contract).await? else {
                return Ok(None);
            };
            let funded_tokens = u128::from(self.matched_ticks) * dexdo_core::TICK_SIZE;
            let bond_required = u128::from(self.price_per_tick) * 2;
            Ok(Some(DealChainSnapshot {
                account_code_hash: "pool-test-code".to_string(),
                account_boc_hash: format!("pool-test:{}", self.token_contract),
                state,
                subscription: DealSubscription {
                    deal_flags: 0,
                    sub_weeks: 0,
                    week_index: 0,
                    tokens_per_week: funded_tokens,
                    funded_tokens,
                    tokens_paid: 0,
                    period_start: 0,
                    week_base_tokens: 0,
                },
                seller_bond: DealSellerBond {
                    bond_funded: true,
                    bond_held: bond_required,
                    bond_required,
                },
                buyer_bond: DealBuyerBond {
                    bond_held: 0,
                    bond_required: 0,
                },
            }))
        }

        async fn snapshot(&self, _: &TokenContract) -> Option<StreamSnapshot> {
            None
        }

        async fn probe_burned_settlement(
            &self,
            _: &TokenContract,
        ) -> Result<Option<(u128, u128, u128)>, ChainError> {
            Ok(match &*self.terminal_settlement.lock().unwrap() {
                Some(dexdo_core::DealTerminalSettlement::ProbeBurned {
                    burned_probe,
                    burned_bond,
                    refund_to_buyer,
                }) => Some((*burned_probe, *burned_bond, *refund_to_buyer)),
                _ => None,
            })
        }

        async fn terminal_settlement(
            &self,
            _: &TokenContract,
        ) -> Result<Option<dexdo_core::DealTerminalSettlement>, ChainError> {
            Ok(self.terminal_settlement.lock().unwrap().clone())
        }
    }

    /// Measured live on the test chain, 4.0.33, campaign run of 2026-08-04: the buyer's `stop()` (function id
    /// `0x6601a0b2`, sent from the buyer note) was the last message TokenContract
    /// `0:9b26f73f...a95e37` ever processed -- the on-chain transaction carries `destroyed=true`,
    /// `Active -> NonExist`, `exit_code=0`. Two seconds later the seller read the deal back, got
    /// the account-is-gone answer, and died with
    /// "strict coherent deal snapshot returned no data after stream open", taking the whole pool
    /// with it. A settled deal is a finished deal, not an unreadable one.
    #[tokio::test]
    async fn opened_deal_that_selfdestructed_on_buyer_stop_retires_instead_of_failing_the_pool() {
        let owner_fills = Arc::new(Mutex::new(Vec::new()));
        let backend = PoolTestBackend::new(owner_fills, "0:settled".to_string(), 8, 3, true, 1);
        backend.opened.store(true, Ordering::Relaxed);
        let token_contract = backend.token_contract.clone();

        // While the account is Active the snapshot drives the settlement driver as before.
        match plan_opened_deal(&backend, &token_contract)
            .await
            .expect("an active TokenContract reads back")
        {
            OpenedDealPlan::Drive(_) => {}
            other => panic!("an active TokenContract must drive settlement: {other:?}"),
        }

        // `stop()` selfdestructs the contract: every getter now answers "no such account".
        backend.exists.store(false, Ordering::Relaxed);
        match plan_opened_deal(&backend, &token_contract)
            .await
            .expect("a settled-and-destroyed TokenContract is not an error")
        {
            OpenedDealPlan::RetireSettled => {}
            other => panic!("a destroyed TokenContract must retire the deal: {other:?}"),
        }
    }

    /// every terminal takes the per-deal TokenContract account with it. The immutable receipt
    /// must retire only that deal instead of turning a late getter failure into the pool's fatal
    /// error. The reported `ProbeBurned` carried the exact 2026-08-04 amounts below; the 2026-09-03
    /// occurrence was an ordinary paid `StreamStopped` under `after_deal_done: retire`.

    /// The seller's next advance then read the account that no longer existed and failed with
    /// "getState returned no data while reconciling the cumulative claim high-water". That message
    /// carries no exit code, so `is_err_not_open` refuses it; the dispute policy it fell through to
    /// asks for the deal state, gets none, and answers `Ok(false)`. Unresolved became the pool's
    /// first fatal error and the whole seller process exited -- on an outcome the protocol allows
    /// and about which there was nothing left for the seller to do.

    /// The same unreadable deal without one exact terminal receipt is still an unexplained failure
    /// and must stay fatal.
    #[tokio::test]
    async fn issue_1923_every_terminal_retires_the_deal_instead_of_killing_the_seller() {
        let advance_failure = |token_contract: &str| {
            ChainError::Chain(format!(
                "TokenContract {token_contract} getState returned no data while reconciling the \
                 cumulative claim high-water"
            ))
        };
        let mut seller = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let mut terminal_receipts = tokio::task::JoinSet::new();

        let terminals = [
            (
                "0:probe-burned",
                dexdo_core::DealTerminalSettlement::ProbeBurned {
                    burned_probe: 4_000_000_000,
                    burned_bond: 4_000_000_000,
                    refund_to_buyer: 4_200_000_000,
                },
            ),
            (
                "0:stream-stopped",
                dexdo_core::DealTerminalSettlement::StreamStopped {
                    to_seller: 3_000_400_000,
                    refund_to_buyer: 4_075_000_000,
                },
            ),
            (
                "0:dispute-resolved",
                dexdo_core::DealTerminalSettlement::DisputeResolved {
                    to_seller: 1_000_000_000,
                    refund_to_buyer: 5_100_000_000,
                    released: false,
                },
            ),
        ];
        for (address, terminal) in terminals {
            let backend = PoolTestBackend::new(
                Arc::new(Mutex::new(Vec::new())),
                address.to_string(),
                98,
                2,
                true,
                1,
            )
            .with_terminal_settlement(terminal);
            backend.opened.store(true, Ordering::Relaxed);
            backend.exists.store(false, Ordering::Relaxed);
            let token_contract = backend.token_contract.clone();
            let chain: Arc<dyn ChainBackend> = Arc::new(backend);
            assert!(
                chain.deal_state(&token_contract).await.unwrap().is_none(),
                "the terminal settlement destroyed the account this test is about"
            );
            let mut first_error = None;
            record_advance_result(
                &seller,
                Ok((
                    token_contract.clone(),
                    chain,
                    seller.state.delivery(&token_contract),
                    false,
                    Err(advance_failure(&token_contract)),
                )),
                &mut terminal_receipts,
                &pool_test_policy(4),
                &mut first_error,
            )
            .await;
            assert!(
                first_error.is_none(),
                "a proven terminal must not become the seller's fatal error: {first_error:?}"
            );
            assert!(
                !seller.server_task.is_finished(),
                "retiring one terminal deal must leave the seller gateway running"
            );
        }

        // The same unreadable deal without that proof stays exactly as fatal as it was.
        let unexplained = PoolTestBackend::new(
            Arc::new(Mutex::new(Vec::new())),
            "0:unexplained".to_string(),
            98,
            2,
            true,
            1,
        );
        unexplained.opened.store(true, Ordering::Relaxed);
        unexplained.exists.store(false, Ordering::Relaxed);
        let token_contract = unexplained.token_contract.clone();
        let chain: Arc<dyn ChainBackend> = Arc::new(unexplained);
        let mut first_error = None;
        record_advance_result(
            &seller,
            Ok((
                token_contract.clone(),
                chain,
                seller.state.delivery(&token_contract),
                false,
                Err(advance_failure(&token_contract)),
            )),
            &mut terminal_receipts,
            &pool_test_policy(4),
            &mut first_error,
        )
        .await;
        let error = first_error.expect("an unreadable deal with no terminal receipt stays a fault");
        assert!(
            error.to_string().contains("by-fact advance failed"),
            "{error:#}"
        );

        terminal_receipts.abort_all();
        seller.server_task.abort();
        let _ = (&mut seller.server_task).await;
    }

    /// What one `run_seller_pool` run does with a 98-tick offer that matched `matched_ticks` and then
    /// settled its TokenContract away, as the 2026-08-04 `ProbeBurned` did.
    struct SettledParentRun {
        /// Every `(nonce, price_per_tick, max_ticks)` the pool asked its provisioner to deploy.
        provisions: Vec<(u64, u64, u64)>,
        /// The price the parent offered at, so the successor's can be checked against it.
        parent_price_per_tick: u64,
        /// SELLs posted for the scripted successor.
        successor_posts: u64,
        successor_token_contract: String,
        /// The parent lineage's durable link to its successor, after the run.
        replacement: (Option<u64>, Option<String>),
        outcome: Result<()>,
    }

    /// Drive the real pool over a parent that matched part of its offer and then settled.

    /// The order is the incident's: the match is recorded through `poll_match_and_maybe_open` while the
    /// parent's `getDeal` still answers -- that is where the authoritative terms become durable -- and
    /// only then does the settlement take the getter with it. Everything after that is
    /// `run_seller_pool`, including the residual provisioning under test.
    async fn pool_run_after_parent_settled(
        root: &std::path::Path,
        matched_ticks: u64,
    ) -> SettledParentRun {
        let residual_ticks = 98 - matched_ticks;
        let note = Arc::new(LocalNote::generate());
        let note_addr = format!("0:{}", "a".repeat(64));
        let frame_model = "openai/gpt-oss-20b";
        let owner_fills = Arc::new(Mutex::new(Vec::new()));
        let parent = Arc::new(
            PoolTestBackend::new(
                owner_fills.clone(),
                format!("0:{}", "1".repeat(64)),
                98,
                matched_ticks,
                true,
                i64::MAX - 4,
            )
            // The incident's exact receipt. The relist does not read it -- `None` from a deal getter
            // is already the settled-and-gone fact -- but the deal under test is the one that
            // happened.
            .with_probe_burn((4_000_000_000, 4_000_000_000, 4_200_000_000)),
        );
        // Sized for the residual it would carry; a full fill has none and never reaches it.
        let successor = Arc::new(PoolTestBackend::new(
            owner_fills.clone(),
            format!("0:{}", "2".repeat(64)),
            residual_ticks.max(2),
            residual_ticks.max(2),
            false,
            i64::MAX - 3,
        ));
        let seller = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            note,
        )
        .await
        .unwrap();
        let gateway = seller.listen_addr.to_string();
        let parent_cfg = SellerConfig {
            token_contract: parent.token_contract.clone(),
            price_per_tick: parent.price_per_tick,
            max_ticks: parent.offered_ticks,
            subscription: false,
            gateway_advertise: gateway.clone(),
            mock_token_count: 8,
        };
        let parent_watch = dexdo::seller::SellerMatchWatchConfig {
            cursor_path: root.join("parent.cursor.json"),
            poll_interval: std::time::Duration::from_millis(1),
        };
        dexdo::seller::poll_match_and_maybe_open(
            &seller,
            parent.as_ref(),
            &parent_cfg,
            &parent_watch.cursor_path,
        )
        .await
        .unwrap()
        .expect("the owner fill against the 98-tick offer");
        let lineage = dexdo::seller::read_seller_fill_lineage(
            &parent_watch.cursor_path,
            &parent.token_contract,
        )
        .unwrap()
        .expect("match discovery persists the authoritative fill lineage");
        assert_eq!(
            (
                lineage.offered_ticks,
                lineage.matched_ticks,
                lineage.residual_ticks,
                lineage.price_per_tick
            ),
            (98, matched_ticks, residual_ticks, parent_cfg.price_per_tick),
        );

        // The settlement: the account is gone and `getDeal` answers nothing from here on.
        parent.settle_and_lose_getdeal();

        let provisions = Arc::new(Mutex::new(Vec::new()));
        let mut provision = {
            let provisions = provisions.clone();
            let successor = successor.clone();
            let note_addr = note_addr.clone();
            move |model: String, nonce: u64, price: u64, ticks: u64| {
                provisions.lock().unwrap().push((nonce, price, ticks));
                let market = dexdo_core::MarketManifest {
                    network: "net-a".to_string(),
                    model_hash: dexdo_core::model_hash_for(&model),
                    frame_model: model,
                    inference_order_book: format!("0:{}", "d".repeat(64)),
                    root_model: format!("0:{}", "e".repeat(64)),
                    token_contract: successor.token_contract.clone(),
                    seller_note: note_addr.clone(),
                    nonce,
                    price_per_tick: u128::from(price),
                    max_ticks: u128::from(ticks),
                };
                let chain: Arc<dyn ChainBackend> = successor.clone();
                futures::future::ready(Ok((market, chain)))
            }
        };
        // The relist run stops the moment its successor SELL is posted. A full fill has no successor
        // to wait for, so that run spends the whole window inside the pool loop -- thousands of turns
        // through the provisioning gate this asserts nothing happened in.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let shutdown = {
            let successor = successor.clone();
            async move {
                while successor.post_calls.load(Ordering::Relaxed) == 0
                    && std::time::Instant::now() < deadline
                {
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                }
            }
        }
        .fuse();
        tokio::pin!(shutdown);
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run_seller_pool(
                &seller,
                vec![SellerPoolDeal {
                    chain: parent.clone(),
                    cfg: parent_cfg,
                    watch: parent_watch.clone(),
                    upstream: dexdo::seller::UpstreamConfig::Mock,
                    nonce: 10,
                    market: None,
                }],
                SellerPoolContext {
                    deals_dir: Some(root),
                    contracts: std::path::Path::new("manifest/deployed.manifest.json"),
                    note_addr: &note_addr,
                    frame_model,
                    gateway_advertise: &gateway,
                    advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::default(),
                    recover_publication: None,
                },
                &pool_test_policy(2),
                &mut provision,
                shutdown.as_mut(),
                &mut false,
            ),
        )
        .await
        .expect("the pool run must end on its own shutdown, not on the test timeout");
        let after = dexdo::seller::read_seller_fill_lineage(
            &parent_watch.cursor_path,
            &parent.token_contract,
        )
        .unwrap()
        .expect("the parent lineage survives the run");
        seller.server_task.abort();
        let _ = seller.server_task.await;
        let provisions = provisions.lock().unwrap().clone();
        SettledParentRun {
            provisions,
            parent_price_per_tick: parent.price_per_tick,
            successor_posts: successor.post_calls.load(Ordering::Relaxed),
            successor_token_contract: successor.token_contract.clone(),
            replacement: (after.replacement_nonce, after.replacement_token_contract),
            outcome,
        }
    }

    /// Measured live on the test chain, 2026-08-04. Order 2 offered 98 ticks and the authoritative
    /// owner-fill lineage recorded `matched_ticks=2 residual_ticks=96`. The buyer could not open the
    /// seller's gateway, stopped the deal on the probe, and the `ProbeBurned` settlement
    /// (`burnedProbe=4000000000 burnedBond=4000000000 refundToBuyer=4200000000`) destroyed the
    /// TokenContract. No successor SELL was posted for the remaining 96 ticks and the executable book
    /// held no ask -- and once leg 3 stopped that outcome from killing the seller, the 96 ticks left
    /// with no error either. The operator was told nothing.

    /// Those ticks were never inside the parent. The order book consumes a SELL slot whole on any
    /// match, partial included ("SELL offer = one-deal slot -> consumed on match (taker BUY), even on
    /// partial", `InferenceOrderBook._match`), so unmatched capacity rests nowhere and only a
    /// successor puts it back. Its terms were read from the parent's own `getDeal` at match discovery
    /// and persisted with the fill while the contract still answered, so a parent that has since
    /// settled costs the cross-check, not the capacity.

    /// Both halves run, because a build that relists whatever it finds passes the first one alone: the
    /// same offer sold in full, settling exactly the same way, must leave nothing behind.

    /// E2E-ROW: E2E-SELL-14/L0
    #[tokio::test]
    async fn a_settled_parent_relists_its_residual_from_the_persisted_lineage() {
        let root = tempfile::tempdir().expect("residual relist test directory");

        let relisted = pool_run_after_parent_settled(root.path(), 2).await;
        relisted
            .outcome
            .expect("a settled parent must not fail the pool it left behind");
        assert_eq!(
            relisted.provisions,
            vec![(11, relisted.parent_price_per_tick, 96)],
            "the 96 residual ticks must be provisioned exactly once, at the parent's own price"
        );
        assert_eq!(
            relisted.successor_posts, 1,
            "exactly one successor SELL carries the residual back to the book"
        );
        assert_eq!(
            relisted.replacement,
            (Some(11), Some(relisted.successor_token_contract.clone())),
            "the parent must link its successor durably, so a restart reconciles it instead of \
             provisioning a second one"
        );

        let full = tempfile::tempdir().expect("full fill test directory");
        let sold_out = pool_run_after_parent_settled(full.path(), 98).await;
        sold_out
            .outcome
            .expect("a fully sold offer that settles must not fail the pool either");
        assert!(
            sold_out.provisions.is_empty(),
            "a fully matched offer has no residual and must provision nothing: {:?}",
            sold_out.provisions
        );
        assert_eq!(
            sold_out.successor_posts, 0,
            "nothing may be posted for capacity that was entirely sold"
        );
        assert_eq!(
            sold_out.replacement,
            (None, None),
            "a fully matched parent must not reserve a successor nonce"
        );
    }

    #[tokio::test]
    async fn seller_fill_poll_returns_whole_owner_batch_once() {
        let owner_fills = Arc::new(Mutex::new(Vec::new()));
        let first = PoolTestBackend::new(owner_fills.clone(), "0:first".to_string(), 8, 3, true, 2);
        let _second = PoolTestBackend::new(owner_fills, "0:second".to_string(), 5, 2, true, 1);
        let note = LocalNote::generate();
        let mut cursor = MatchWatchCursor::new(0);

        let batch = first.poll_seller_fills(&note, &mut cursor).await.unwrap();
        assert_eq!(
            batch
                .iter()
                .map(|fill| fill.token_contract.as_str())
                .collect::<Vec<_>>(),
            ["0:second", "0:first"]
        );
        assert!(
            first
                .poll_seller_fills(&note, &mut cursor)
                .await
                .unwrap()
                .is_empty(),
            "the owner-wide batch must be consumed exactly once"
        );
    }

    struct PoolTestProvisioner {
        note_addr: String,
        frame_model: String,
        backends: VecDeque<Arc<PoolTestBackend>>,
        calls: Arc<Mutex<Vec<(u64, u64, u64)>>>,
    }

    impl PoolTestProvisioner {
        fn provision(
            &mut self,
            nonce: u64,
            price_per_tick: u64,
            max_ticks: u64,
        ) -> anyhow::Result<(dexdo_core::MarketManifest, Arc<dyn ChainBackend>)> {
            self.calls
                .lock()
                .unwrap()
                .push((nonce, price_per_tick, max_ticks));
            let backend = self
                .backends
                .pop_front()
                .expect("one scripted backend per residual");
            assert_eq!(backend.price_per_tick, price_per_tick);
            assert_eq!(backend.offered_ticks, max_ticks);
            let token_contract = backend.token_contract.clone();
            let market = dexdo_core::MarketManifest {
                network: "net-a".to_string(),
                frame_model: self.frame_model.clone(),
                model_hash: dexdo_core::model_hash_for(&self.frame_model),
                inference_order_book: format!("0:{}", "d".repeat(64)),
                root_model: format!("0:{}", "e".repeat(64)),
                token_contract,
                seller_note: self.note_addr.clone(),
                nonce,
                price_per_tick: u128::from(price_per_tick),
                max_ticks: u128::from(max_ticks),
            };
            let chain: Arc<dyn ChainBackend> = backend;
            Ok((market, chain))
        }
    }

    fn pool_test_policy(max_open_deals: u64) -> policy::SellerRuntimePolicy {
        policy::SellerRuntimePolicy {
            after_deal_done: policy::SellerAfterDealDoneAction::Retire,
            buyer_no_show: policy::SellerBuyerNoShowAction::RetireGateway,
            dispute_against_me: policy::SellerDisputeAgainstMeAction::ReleaseIfClean,
            max_open_deals,
        }
    }

    struct UnknownOwnerFillPoolRun {
        outcome: Result<()>,
        open_calls: u64,
        opened: bool,
        unknown_token_contract: String,
    }

    /// Drive the real pool entry with one fully matched live deal plus one same-note owner fill for
    /// which this run has no handle. `terminal_unknown` controls only the authoritative getState
    /// fact for the unknown TokenContract; every other pool input is identical between the two
    /// regressions.
    async fn run_pool_with_unknown_owner_fill(terminal_unknown: bool) -> UnknownOwnerFillPoolRun {
        let root = tempfile::tempdir().expect(" pool test directory");
        let note = Arc::new(LocalNote::generate());
        let note_addr = format!("0:{}", "a".repeat(64));
        let owner_fills = Arc::new(Mutex::new(Vec::new()));
        let live = Arc::new(PoolTestBackend::new(
            owner_fills.clone(),
            format!("0:{}", "1".repeat(64)),
            8,
            8,
            true,
            2,
        ));
        let unknown_token_contract = format!("0:{}", "9".repeat(64));
        owner_fills.lock().unwrap().push((
            1,
            MatchedFill {
                order_id: 1170,
                token_contract: unknown_token_contract.clone(),
                ticks: 2,
                price_per_tick: u128::from(live.price_per_tick),
            },
        ));
        if terminal_unknown {
            live.mark_owner_fill_terminal(&unknown_token_contract);
        }

        let mut seller = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            note,
        )
        .await
        .unwrap();
        let gateway = seller.listen_addr.to_string();
        let deal = SellerPoolDeal {
            chain: live.clone(),
            cfg: SellerConfig {
                token_contract: live.token_contract.clone(),
                price_per_tick: live.price_per_tick,
                max_ticks: live.offered_ticks,
                subscription: false,
                gateway_advertise: gateway.clone(),
                mock_token_count: 8,
            },
            watch: dexdo::seller::SellerMatchWatchConfig {
                cursor_path: root.path().join("live.cursor.json"),
                poll_interval: std::time::Duration::from_millis(1),
            },
            upstream: dexdo::seller::UpstreamConfig::Mock,
            nonce: 1,
            market: None,
        };
        let live_for_shutdown = live.clone();
        let shutdown = async move {
            while live_for_shutdown.open_calls.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        }
        .fuse();
        tokio::pin!(shutdown);
        let mut provision = |_: String, _: u64, _: u64, _: u64| {
            futures::future::ready(Err(anyhow::anyhow!(
                "a full match must not provision residual capacity"
            )))
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run_seller_pool(
                &seller,
                vec![deal],
                SellerPoolContext {
                    deals_dir: Some(root.path()),
                    contracts: std::path::Path::new("manifest/deployed.manifest.json"),
                    note_addr: &note_addr,
                    frame_model: "openai/gpt-oss-20b",
                    gateway_advertise: &gateway,
                    advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::default(),
                    recover_publication: None,
                },
                &pool_test_policy(1),
                &mut provision,
                shutdown.as_mut(),
                &mut false,
            ),
        )
        .await
        .expect("the  pool run must stop after serving the live match");
        let run = UnknownOwnerFillPoolRun {
            outcome,
            open_calls: live.open_calls.load(Ordering::Relaxed),
            opened: live.opened.load(Ordering::Relaxed),
            unknown_token_contract,
        };
        let _ = (&mut seller.server_task).await;
        run
    }

    /// money-liveness half: the owner-wide audit may encounter a fill left by an already
    /// settled deal before the live deal this run is serving. The residue must not turn a clean
    /// pool stop into an error, and the assertion is the real `open_stream` call for the live match.
    #[tokio::test]
    async fn settled_unknown_owner_fill_does_not_stop_a_live_matched_deal() {
        let run = run_pool_with_unknown_owner_fill(true).await;
        if let Err(error) = run.outcome {
            panic!(
                "settled owner fill {} stopped the pool after the live match was served: {error:#}",
                run.unknown_token_contract
            );
        }
        assert_eq!(
            run.open_calls, 1,
            "the live matched deal must be served once"
        );
        assert!(
            run.opened,
            "the live matched deal never reached open_stream"
        );
    }

    /// money-safety half: an owner fill whose TokenContract still reports a non-terminal
    /// state remains unaccounted capacity. Its stable code, headline and hint stay byte-for-byte the
    /// same as before the terminal-residue discrimination.
    #[tokio::test]
    async fn non_terminal_unknown_owner_fill_still_fails_with_exact_pool_error() {
        let run = run_pool_with_unknown_owner_fill(false).await;
        assert_eq!(
            run.open_calls, 1,
            "the matched deal must reach the real pool entry"
        );
        assert!(run.opened, "the matched deal never reached open_stream");
        let error = run
            .outcome
            .expect_err("a genuinely unaccounted non-terminal fill must remain fatal");
        let structured = error
            .downcast_ref::<dexdo_core::DexdoError>()
            .expect("the unknown owner fill must remain a structured pool error");
        assert_eq!(
            structured.code(),
            dexdo_core::error_codes::E_POOL_UNKNOWN_OWNER_FILL.code()
        );
        // the message names the TokenContract canonically. A TokenContract is a self-DApp
        // account, so its DApp half is its own account id; `unknown_token_contract` is the chain form.
        let unknown_account = run
            .unknown_token_contract
            .strip_prefix("0:")
            .expect(" fixture holds the chain form");
        assert_eq!(
            structured.message(),
            format!(
                "seller owner fill for TokenContract {unknown_account}::{unknown_account} has no \
                 same-note deal handle/manifest; refusing to discard unknown capacity"
            )
        );
        assert_eq!(
            structured.hint(),
            Some(
                "an \"owner fill\" is a match against THIS note's own resting order; without that \
                 deal's handle/market.json the pool cannot account the capacity it just sold. Run \
                 the seller from the directory holding that deal's handle, or close the orphaned \
                 deal (`dexdo deals`, then `destroy`/`recover`). Attached as `secondary`, it is a \
                 CONSEQUENCE of the primary error above -- fix that first and re-run"
            )
        );
    }

    fn elapse_mock_probe_window(state_path: &std::path::Path, token_contract: &str) {
        let mut state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(state_path).unwrap()).unwrap();
        let opened_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .saturating_sub(ProtocolConsts::canonical().probe_window.as_secs());
        let stream = &mut state["streams"][token_contract];
        stream["probe_time"] = opened_at.into();
        stream["last_claim_time"] = opened_at.into();
        std::fs::write(state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    }

    async fn record_terminal_for_test(
        seller: &dexdo::seller::RunningSeller,
        chain: Arc<dyn ChainBackend>,
        token_contract: &str,
        delivery: dexdo::seller::gateway::DealDelivery,
        terminal_receipts: &mut tokio::task::JoinSet<SellerTerminalReceiptResult>,
        first_error: &mut Option<anyhow::Error>,
    ) {
        let token_contract = token_contract.to_string();
        record_advance_result(
            seller,
            Ok((token_contract, chain, delivery, false, Ok(2))),
            terminal_receipts,
            &pool_test_policy(4),
            first_error,
        )
        .await;
    }

    #[tokio::test]
    async fn exhausted_billing_stops_the_open_deal_from_the_seller_side_only_after_advance() {
        let mut seller = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let mut terminal_receipts = tokio::task::JoinSet::new();
        for probe_accepted in [false, true] {
            let token_contract = format!("0:billing-exhausted-{probe_accepted}");
            let backend = Arc::new(PoolTestBackend::new(
                Arc::new(Mutex::new(Vec::new())),
                token_contract.clone(),
                8,
                3,
                true,
                1,
            ));
            backend.opened.store(true, Ordering::Relaxed);
            backend
                .probe_accepted
                .store(probe_accepted, Ordering::Relaxed);
            let delivery = seller.state.delivery(&token_contract);
            delivery.count.store(2, Ordering::Release);
            delivery.done.store(true, Ordering::Release);
            delivery.billing_exhausted.store(true, Ordering::Release);
            let mut first_error = None;
            let chain: Arc<dyn ChainBackend> = backend.clone();

            record_advance_result(
                &seller,
                Ok((token_contract, chain, delivery, false, Ok(2))),
                &mut terminal_receipts,
                &pool_test_policy(4),
                &mut first_error,
            )
            .await;

            assert!(first_error.is_none(), "{first_error:?}");
            assert_eq!(backend.seller_stop_calls.load(Ordering::Relaxed), 1);
            assert_eq!(backend.buyer_stop_calls.load(Ordering::Relaxed), 0);
        }
        assert!(
            terminal_receipts.is_empty(),
            "sellerStop must not be reported through the buyer-stop receipt path"
        );

        let ordinary_done = Arc::new(PoolTestBackend::new(
            Arc::new(Mutex::new(Vec::new())),
            "0:ordinary-session-done".to_string(),
            8,
            3,
            true,
            1,
        ));
        ordinary_done.opened.store(true, Ordering::Relaxed);
        ordinary_done.probe_accepted.store(true, Ordering::Relaxed);
        let token_contract = ordinary_done.token_contract.clone();
        let delivery = seller.state.delivery(&token_contract);
        delivery.done.store(true, Ordering::Release);
        let chain: Arc<dyn ChainBackend> = ordinary_done.clone();
        let mut first_error = None;
        record_advance_result(
            &seller,
            Ok((token_contract, chain, delivery, false, Ok(2))),
            &mut terminal_receipts,
            &pool_test_policy(4),
            &mut first_error,
        )
        .await;
        assert!(first_error.is_none(), "{first_error:?}");
        assert_eq!(ordinary_done.seller_stop_calls.load(Ordering::Relaxed), 0);

        terminal_receipts.abort_all();
        seller.server_task.abort();
        let _ = (&mut seller.server_task).await;
    }

    #[tokio::test]
    async fn successful_full_advance_accepts_the_terminal_self_destruct() {
        let root = tempfile::tempdir().unwrap();
        let chain: Arc<dyn ChainBackend> = Arc::new(MockChainBackend::new(
            root.path().join("endpoints.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        ));
        let mut seller = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let token_contract = "finalized-and-destroyed";
        let delivery = seller.state.delivery(token_contract);
        let mut terminal_receipts = tokio::task::JoinSet::new();
        let mut first_error = None;

        record_advance_result(
            &seller,
            Ok((
                token_contract.to_string(),
                chain,
                delivery,
                true,
                Ok(3 * dexdo_core::TICK_SIZE),
            )),
            &mut terminal_receipts,
            &pool_test_policy(1),
            &mut first_error,
        )
        .await;

        assert!(first_error.is_none(), "{first_error:?}");
        terminal_receipts.abort_all();
        seller.server_task.abort();
        let _ = (&mut seller.server_task).await;
    }

    #[tokio::test]
    async fn terminal_policy_dispatch_uses_authoritative_probe_state_at_zero_delivery() {
        let root = tempfile::tempdir().unwrap();
        let chain = Arc::new(MockChainBackend::new(
            root.path().join("endpoints.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        ));
        let seller_note = Arc::new(LocalNote::generate());
        let buyer_note = LocalNote::generate();
        let mut seller = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            seller_note.clone(),
        )
        .await
        .unwrap();
        let policy = policy::SellerRuntimePolicy {
            after_deal_done: policy::SellerAfterDealDoneAction::Retire,
            buyer_no_show: policy::SellerBuyerNoShowAction::CleanupAndRetire,
            dispute_against_me: policy::SellerDisputeAgainstMeAction::ReleaseIfClean,
            max_open_deals: 2,
        };
        let mut terminal_receipts = tokio::task::JoinSet::new();

        for (token_contract, probe_accepted) in [
            ("accepted-probe-zero-delivery", true),
            ("true-buyer-no-show", false),
        ] {
            let token_contract = token_contract.to_string();
            chain
                .post_offer(
                    SellOffer {
                        price_per_tick: dexdo_core::PRICE_STEP as u64,
                        max_ticks: 4,
                        token_contract: token_contract.clone(),
                        flags: 0,
                    },
                    seller_note.as_ref(),
                )
                .await
                .unwrap();
            chain.place_buy(&token_contract, &buyer_note).await.unwrap();
            chain
                .open_stream(&token_contract, Vec::new(), seller_note.as_ref())
                .await
                .unwrap();
            if probe_accepted {
                elapse_mock_probe_window(
                    &root.path().join("endpoints.chainstate.json"),
                    &token_contract,
                );
                chain.accept_probe(&token_contract).await.unwrap();
            }
            chain.stop(&token_contract, &buyer_note).await.unwrap();

            let state = chain.deal_state(&token_contract).await.unwrap().unwrap();
            assert_eq!(state.probe_accepted, probe_accepted);
            let delivery = seller.state.delivery(&token_contract);
            assert_eq!(delivery.count.load(Ordering::Acquire), 0);
            let backend: Arc<dyn ChainBackend> = chain.clone();
            let mut first_error = None;
            record_advance_result(
                &seller,
                Ok((token_contract, backend, delivery, false, Ok(0))),
                &mut terminal_receipts,
                &policy,
                &mut first_error,
            )
            .await;

            if probe_accepted {
                assert!(first_error.is_none(), "{first_error:?}");
            } else {
                let error = first_error.expect("true no-show must use buyer_no_show policy");
                assert!(
                    error.to_string().contains("failure_class=buyer_no_show"),
                    "{error:#}"
                );
            }
        }

        terminal_receipts.abort_all();
        seller.server_task.abort();
        let _ = (&mut seller.server_task).await;
    }

    #[tokio::test(start_paused = true)]
    async fn terminal_worker_emits_exact_order_once_per_tc_without_false_stop() {
        let root = tempfile::tempdir().unwrap();
        let chain = Arc::new(MockChainBackend::new(
            root.path().join("endpoints.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        ));
        let seller_note = Arc::new(LocalNote::generate());
        let buyer_note = LocalNote::generate();
        for (tc, accept_probe) in [
            ("clean-1", true),
            ("clean-2", true),
            ("probe-stop", false),
            ("disputed", true),
        ] {
            let tc = tc.to_string();
            chain
                .post_offer(
                    SellOffer {
                        price_per_tick: dexdo_core::PRICE_STEP as u64,
                        max_ticks: 4,
                        token_contract: tc.clone(),
                        flags: 0,
                    },
                    seller_note.as_ref(),
                )
                .await
                .unwrap();
            chain.place_buy(&tc, &buyer_note).await.unwrap();
            chain
                .open_stream(&tc, Vec::new(), seller_note.as_ref())
                .await
                .unwrap();
            if accept_probe {
                elapse_mock_probe_window(&root.path().join("endpoints.chainstate.json"), &tc);
                chain.accept_probe(&tc).await.unwrap();
            }
            if tc == "clean-1" {
                // Keep TC-A open so the first production receipt read returns `None`.
            } else if tc == "disputed" {
                chain.dispute(&tc, &buyer_note).await.unwrap();
                chain.release_dispute(&tc).await.unwrap();
            } else {
                chain.stop(&tc, &buyer_note).await.unwrap();
            }
        }
        let chain_state_path = root.path().join("endpoints.chainstate.json");
        let open_chain_state = std::fs::read(&chain_state_path).unwrap();
        let mut seller = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            seller_note,
        )
        .await
        .unwrap();
        let receipt_chain: Arc<dyn ChainBackend> = chain.clone();
        let mut terminal_receipts = tokio::task::JoinSet::new();
        let mut first_error = None;

        let delivery_a = seller.state.delivery("clean-1");
        record_terminal_for_test(
            &seller,
            receipt_chain.clone(),
            "clean-1",
            delivery_a.clone(),
            &mut terminal_receipts,
            &mut first_error,
        )
        .await;
        record_terminal_for_test(
            &seller,
            receipt_chain.clone(),
            "clean-2",
            seller.state.delivery("clean-2"),
            &mut terminal_receipts,
            &mut first_error,
        )
        .await;

        let joined_b = terminal_receipts
            .join_next()
            .await
            .expect("TC-B receipt task");
        let events_b = record_terminal_receipt_result(joined_b, &mut first_error);
        assert_eq!(events_b[0]["token_contract"], "clean-2");

        std::fs::write(&chain_state_path, b"{").unwrap();
        tokio::time::advance(dexdo_core::params::SELLER_TERMINAL_RECEIPT_POLL_INTERVAL).await;
        tokio::task::yield_now().await;
        tokio::time::advance(dexdo_core::params::SELLER_TERMINAL_RECEIPT_POLL_INTERVAL).await;
        tokio::task::yield_now().await;
        assert!(
            terminal_receipts.try_join_next().is_none(),
            "TC-A must remain pending across transient receipt errors"
        );
        std::fs::write(&chain_state_path, open_chain_state).unwrap();
        chain
            .stop(&"clean-1".to_string(), &buyer_note)
            .await
            .unwrap();
        tokio::time::advance(dexdo_core::params::SELLER_TERMINAL_RECEIPT_POLL_INTERVAL).await;
        let joined_a = terminal_receipts
            .join_next()
            .await
            .expect("TC-A receipt task");
        let events_a = record_terminal_receipt_result(joined_a, &mut first_error);

        for (tc, events) in [("clean-1", events_a), ("clean-2", events_b)] {
            assert_eq!(
                events
                    .iter()
                    .map(|event| event["event"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["buyer_stop_observed", "settled", "exiting"]
            );
            for (index, event) in events.iter().enumerate() {
                assert_eq!(event["schema"], SELLER_EVENT_SCHEMA);
                assert_eq!(event["seq"], u64::try_from(index + 1).unwrap());
                assert_eq!(event["role"], "seller");
                assert_eq!(event["token_contract"], tc);
                let line = serde_json::to_string(event).unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&line).unwrap(),
                    *event
                );
                assert!(!line.contains("settlement_submitted"));
            }
            assert_eq!(events[1]["outcome"], "AmicableSplit");
            assert_eq!(events[2]["exit_code"], 0);
        }

        record_terminal_for_test(
            &seller,
            receipt_chain.clone(),
            "clean-1",
            delivery_a,
            &mut terminal_receipts,
            &mut first_error,
        )
        .await;
        let replay = terminal_receipts
            .join_next()
            .await
            .expect("replayed TC-A receipt task");
        assert!(
            record_terminal_receipt_result(replay, &mut first_error).is_empty(),
            "replayed terminal read must not duplicate clean-1"
        );

        for tc in ["probe-stop", "disputed"] {
            record_terminal_for_test(
                &seller,
                receipt_chain.clone(),
                tc,
                seller.state.delivery(tc),
                &mut terminal_receipts,
                &mut first_error,
            )
            .await;
        }
        tokio::task::yield_now().await;
        tokio::time::advance(dexdo_core::params::SELLER_TERMINAL_RECEIPT_TIMEOUT).await;
        for tc in ["probe-stop", "disputed"] {
            let joined = terminal_receipts
                .join_next()
                .await
                .expect("generic terminal receipt task");
            assert!(
                record_terminal_receipt_result(joined, &mut first_error).is_empty(),
                "{tc} must not produce a terminal trail"
            );
        }
        assert!(first_error.is_none(), "{first_error:?}");

        record_terminal_for_test(
            &seller,
            receipt_chain,
            "unfunded-generic-terminal",
            seller.state.delivery("unfunded-generic-terminal"),
            &mut terminal_receipts,
            &mut first_error,
        )
        .await;
        let error = first_error.expect("missing authoritative state must fail closed");
        assert!(
            error
                .to_string()
                .contains("refusing to guess buyer_no_show"),
            "{error:#}"
        );
        tokio::task::yield_now().await;
        tokio::time::advance(dexdo_core::params::SELLER_TERMINAL_RECEIPT_TIMEOUT).await;
        let joined = terminal_receipts
            .join_next()
            .await
            .expect("missing-state terminal receipt task");
        assert!(
            record_terminal_receipt_result(joined, &mut None).is_empty(),
            "missing authoritative state must not produce a terminal trail"
        );
        seller.server_task.abort();
        let _ = (&mut seller.server_task).await;
    }

    /// Drive the production pool entry point through the successful side of the shutdown / match
    /// race: inspection first sees this seller's resting SELL, then polling the already-armed stop
    /// matches that exact offer before `stop_exact_offer` can cancel it. Startup therefore succeeds as
    /// `Ready(ResumedFunded)`, but consuming the operator's stop must still set the same disposition
    /// record used by the failing startup path.
    #[tokio::test]
    async fn a_stop_consumed_by_a_succeeding_startup_records_its_disposition() {
        let note = Arc::new(LocalNote::generate());
        let mut seller = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            note.clone(),
        )
        .await
        .unwrap();
        let gateway = seller.listen_addr.to_string();
        let (chain, cfg, identity, root) =
            existing_resting_offer("succeeding-startup-stop", note, gateway.clone()).await;
        let token_contract = cfg.token_contract.clone();
        let deal = SellerPoolDeal {
            watch: dexdo::seller::SellerMatchWatchConfig {
                cursor_path: root.path().join("startup.cursor.json"),
                poll_interval: std::time::Duration::from_millis(1),
            },
            chain: Arc::new(chain.clone()),
            cfg,
            upstream: dexdo::seller::UpstreamConfig::Mock,
            nonce: 1,
            market: None,
        };
        let buyer = Arc::new(LocalNote::generate());
        let chain_for_shutdown = chain.clone();
        let token_contract_for_shutdown = token_contract.clone();
        let shutdown = async move {
            chain_for_shutdown
                .place_buy(&token_contract_for_shutdown, buyer.as_ref())
                .await
                .expect("the stop races with a real mock-chain match");
        }
        .fuse();
        tokio::pin!(shutdown);
        let mut shutdown_requested = false;

        let startup = prepare_pool_deal(
            &seller,
            &deal,
            &SellerPoolContext {
                deals_dir: Some(root.path()),
                contracts: std::path::Path::new("manifest/deployed.manifest.json"),
                note_addr: &identity.owner_note,
                frame_model: "openai/gpt-oss-20b",
                gateway_advertise: &gateway,
                advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::default(),
                recover_publication: None,
            },
            false,
            shutdown.as_mut(),
            &mut shutdown_requested,
        )
        .await
        .expect("AlreadyMatched makes the interrupted startup succeed");
        seller.server_task.abort();
        let _ = (&mut seller.server_task).await;

        assert!(
            startup.is_none(),
            "the successful AlreadyMatched startup resumes a funded deal, not a resting offer"
        );
        assert!(
            futures::future::FusedFuture::is_terminated(shutdown.as_ref().get_ref()),
            "the real startup entry point must consume the armed stop"
        );
        assert!(
            shutdown_requested,
            "the successful startup must leave the stop disposition on the seller's existing record"
        );
        assert!(
            chain
                .raw_resting_sell_orders_for_tc(&token_contract)
                .await
                .unwrap()
                .is_empty(),
            "the match must consume the exact resting SELL"
        );
    }

    /// The operator's stop is a `Fuse`, and a pool deal startup CONSUMES it:
    /// `prepare_seller_offer_with_liveness` selects on it and, when it wins, stops that deal. From
    /// that moment `Fuse::poll` answers `Poll::Pending` forever -- by design, so `select!` can keep
    /// polling it and ask `is_terminated()` instead -- which makes a consumed stop byte-for-byte
    /// indistinguishable from one that never arrived. Nothing recorded that it had arrived, so the
    /// pool carried on: it started the NEXT deal in the pool (a bond and an on-chain
    /// `postSellOffer`), and its own `select!` shutdown arm was already spent, so the loop ran until
    /// something else stopped it and then reported THAT as the reason.

    /// Three real deals, one gateway. The stop lands in the second deal's startup, which is where
    /// the signal is consumed; the third must never be started, and the reason the operator is given
    /// must be the shutdown they asked for.

    /// The third deal's witness is its own `inspection_fail_once`: `read_openable_match_now` is the
    /// FIRST chain call `prepare_pool_deal` makes, and that flag is swapped to false when it lands.
    /// A one-way record is the point -- every error path in this loop calls `unregister_stream`,
    /// which would erase a route-based witness and hide the very thing this test is about.
    #[tokio::test]
    async fn a_shutdown_consumed_by_a_deal_startup_stops_the_pool_and_names_itself() {
        let root = tempfile::tempdir().expect("pool test directory");
        let root = root.path();
        let note = Arc::new(LocalNote::generate());
        let note_addr = format!("0:{}", "a".repeat(64));
        let frame_model = "openai/gpt-oss-20b";
        let owner_fills = Arc::new(Mutex::new(Vec::new()));
        // Started before the stop arrives, and still supervised when it does: without it the loop
        // would have nothing left to do and would exit on its own, whatever the shutdown did.
        let running = Arc::new(PoolTestBackend::new(
            owner_fills.clone(),
            format!("0:{}", "1".repeat(64)),
            8,
            3,
            false,
            1,
        ));
        // The deal whose startup the operator's stop lands in.
        let stopper = Arc::new(PoolTestBackend::new(
            owner_fills.clone(),
            format!("0:{}", "2".repeat(64)),
            6,
            2,
            false,
            2,
        ));
        // The deal that must never be started.
        let later = Arc::new(PoolTestBackend::new(
            owner_fills.clone(),
            format!("0:{}", "3".repeat(64)),
            4,
            2,
            false,
            3,
        ));
        later.inspection_fail_once.store(true, Ordering::Relaxed);

        let mut seller = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            note.clone(),
        )
        .await
        .unwrap();
        let gateway = seller.listen_addr.to_string();
        let deal_for = |backend: &Arc<PoolTestBackend>, nonce: u64, cursor: &str| SellerPoolDeal {
            cfg: SellerConfig {
                token_contract: backend.token_contract.clone(),
                price_per_tick: backend.price_per_tick,
                max_ticks: backend.offered_ticks,
                subscription: false,
                gateway_advertise: gateway.clone(),
                mock_token_count: 8,
            },
            watch: dexdo::seller::SellerMatchWatchConfig {
                cursor_path: root.join(cursor),
                poll_interval: std::time::Duration::from_millis(1),
            },
            chain: backend.clone(),
            upstream: dexdo::seller::UpstreamConfig::Mock,
            nonce,
            market: None,
        };

        // The operator's stop, tied to a real production event: the first deal's `postSellOffer`
        // has landed, so the pool is up and the next startup is the one that consumes it.
        let running_for_shutdown = running.clone();
        let shutdown = async move {
            while running_for_shutdown.post_calls.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        }
        .fuse();
        tokio::pin!(shutdown);
        let mut shutdown_requested = false;
        let mut provision = |_: String, _: u64, _: u64, _: u64| {
            futures::future::ready(Err(anyhow::anyhow!(
                "no residual is provisioned in this test"
            )))
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            run_seller_pool(
                &seller,
                vec![
                    deal_for(&running, 10, "running.cursor.json"),
                    deal_for(&stopper, 20, "stopper.cursor.json"),
                    deal_for(&later, 30, "later.cursor.json"),
                ],
                SellerPoolContext {
                    deals_dir: Some(root),
                    contracts: std::path::Path::new("manifest/deployed.manifest.json"),
                    note_addr: &note_addr,
                    frame_model,
                    gateway_advertise: &gateway,
                    advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::default(),
                    recover_publication: None,
                },
                &pool_test_policy(3),
                &mut provision,
                shutdown.as_mut(),
                &mut shutdown_requested,
            ),
        )
        .await
        .expect("the pool must stop on the shutdown it consumed, not on the test timeout")
        .expect_err("a deal startup interrupted by the operator is still reported");
        seller.server_task.abort();
        let _ = (&mut seller.server_task).await;

        assert!(
            later.inspection_fail_once.load(Ordering::Relaxed),
            "the pool started another deal after it had already consumed the operator's stop"
        );
        assert_eq!(
            later.post_calls.load(Ordering::Relaxed),
            0,
            "no SELL may be posted after the operator's stop was consumed"
        );
        assert!(
            shutdown_requested,
            "consuming the stop must leave the disposition on the flag the seller already keeps"
        );
        let reported = format!("{error:#}", error = outcome);
        assert!(
            reported.contains("interrupted by shutdown"),
            "the operator must be told about their own stop: {reported}"
        );
        assert!(
            !reported.contains("gateway stopped while pool deals were active"),
            "a consumed stop must not be reported as some later symptom: {reported}"
        );
    }

    #[test]
    fn upstream_failure_jsonl_has_stable_safe_schema() {
        let event = upstream_failure_event(7, "0:deal", "auth", false, "Unavailable", Some(401));
        let line = serde_json::to_string(&event).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["schema"], SELLER_EVENT_SCHEMA);
        assert_eq!(parsed["seq"], 7);
        assert_eq!(parsed["event"], "upstream_failed");
        assert_eq!(parsed["role"], "seller");
        assert_eq!(parsed["token_contract"], "0:deal");
        assert_eq!(parsed["error_class"], "auth");
        assert_eq!(parsed["retryable"], false);
        assert_eq!(parsed["grpc_status"], "Unavailable");
        assert_eq!(parsed["http_status"], 401);
        for forbidden in ["Authorization", "prompt", "provider response", "/secret/"] {
            assert!(!line.contains(forbidden), "{line}");
        }
    }

    #[tokio::test]
    async fn seller_pool_recursively_relists_exact_residuals_on_one_gateway() {
        fn context<'a>(
            root: &'a std::path::Path,
            note_addr: &'a str,
            frame_model: &'a str,
            gateway: &'a str,
        ) -> SellerPoolContext<'a> {
            SellerPoolContext {
                deals_dir: Some(root),
                contracts: std::path::Path::new("manifest/deployed.manifest.json"),
                note_addr,
                frame_model,
                gateway_advertise: gateway,
                advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::default(),
                recover_publication: None,
            }
        }

        let root = tempfile::tempdir().expect("seller pool test directory");
        let root = root.path();
        let note = Arc::new(LocalNote::generate());
        let note_addr = format!("0:{}", "a".repeat(64));
        let frame_model = "openai/gpt-oss-20b";
        let owner_fills = Arc::new(Mutex::new(Vec::new()));
        let initial = Arc::new(PoolTestBackend::new(
            owner_fills.clone(),
            format!("0:{}", "1".repeat(64)),
            8,
            3,
            true,
            i64::MAX - 4,
        ));
        let independent = Arc::new(PoolTestBackend::new(
            owner_fills.clone(),
            format!("0:{}", "4".repeat(64)),
            4,
            4,
            true,
            i64::MAX - 3,
        ));
        let residual_five = Arc::new(
            PoolTestBackend::new(
                owner_fills.clone(),
                format!("0:{}", "2".repeat(64)),
                5,
                2,
                false,
                i64::MAX - 2,
            )
            .with_inspection_failure()
            .with_open_failure(),
        );
        let residual_three = Arc::new(PoolTestBackend::new(
            owner_fills.clone(),
            format!("0:{}", "3".repeat(64)),
            3,
            2,
            false,
            i64::MAX - 1,
        ));
        let market_for = |backend: &PoolTestBackend, nonce| dexdo_core::MarketManifest {
            network: "net-a".to_string(),
            frame_model: frame_model.to_string(),
            model_hash: dexdo_core::model_hash_for(frame_model),
            inference_order_book: format!("0:{}", "d".repeat(64)),
            root_model: format!("0:{}", "e".repeat(64)),
            token_contract: backend.token_contract.clone(),
            seller_note: note_addr.clone(),
            nonce,
            price_per_tick: u128::from(backend.price_per_tick),
            max_ticks: u128::from(backend.offered_ticks),
        };
        let bind_addr = "127.0.0.1:0".parse().unwrap();
        let mut seller = dexdo::seller::start_gateway_with_note(
            bind_addr,
            dexdo::seller::UpstreamConfig::Mock,
            note.clone(),
        )
        .await
        .unwrap();
        let gateway = seller.listen_addr.to_string();
        let initial_config = |gateway: &str| SellerConfig {
            token_contract: initial.token_contract.clone(),
            price_per_tick: initial.price_per_tick,
            max_ticks: initial.offered_ticks,
            subscription: false,
            gateway_advertise: gateway.to_string(),
            mock_token_count: 8,
        };
        let initial_watch = dexdo::seller::SellerMatchWatchConfig {
            cursor_path: root.join("initial.cursor.json"),
            poll_interval: std::time::Duration::from_millis(1),
        };
        let initial_deal = |gateway: &str| SellerPoolDeal {
            chain: initial.clone(),
            cfg: initial_config(gateway),
            watch: initial_watch.clone(),
            upstream: dexdo::seller::UpstreamConfig::Mock,
            nonce: 10,
            market: Some(market_for(initial.as_ref(), 10)),
        };
        let independent_deal = |gateway: &str| SellerPoolDeal {
            chain: independent.clone(),
            cfg: SellerConfig {
                token_contract: independent.token_contract.clone(),
                price_per_tick: independent.price_per_tick,
                max_ticks: independent.offered_ticks,
                subscription: false,
                gateway_advertise: gateway.to_string(),
                mock_token_count: 8,
            },
            watch: dexdo::seller::SellerMatchWatchConfig {
                cursor_path: root.join("independent.cursor.json"),
                poll_interval: std::time::Duration::from_millis(1),
            },
            upstream: dexdo::seller::UpstreamConfig::Mock,
            nonce: 20,
            market: Some(market_for(independent.as_ref(), 20)),
        };
        let boundary_writes = Arc::new(AtomicU64::new(0));
        let mut boundary_provision = {
            let boundary_writes = boundary_writes.clone();
            move |_: String, _: u64, _: u64, _: u64| {
                boundary_writes.fetch_add(1, Ordering::Relaxed);
                futures::future::ready(Err(anyhow::anyhow!(
                    "max_open_deals must reject before provision"
                )))
            }
        };
        let boundary_shutdown = futures::future::pending::<()>().fuse();
        tokio::pin!(boundary_shutdown);
        let boundary_error = run_seller_pool(
            &seller,
            vec![initial_deal(&gateway), independent_deal(&gateway)],
            context(root, &note_addr, frame_model, &gateway),
            &pool_test_policy(1),
            &mut boundary_provision,
            boundary_shutdown.as_mut(),
            &mut false,
        )
        .await
        .expect_err("two deals must exceed seller.max_open_deals=1");
        assert!(boundary_error.to_string().contains("max_open_deals=1"));
        assert_eq!(boundary_writes.load(Ordering::Relaxed), 0);
        assert_eq!(initial.post_calls.load(Ordering::Relaxed), 0);
        assert_eq!(initial.open_calls.load(Ordering::Relaxed), 0);
        assert_eq!(independent.post_calls.load(Ordering::Relaxed), 0);
        assert_eq!(independent.open_calls.load(Ordering::Relaxed), 0);

        dexdo::seller::poll_match_and_maybe_open(
            &seller,
            initial.as_ref(),
            &initial_config(&gateway),
            &initial_watch.cursor_path,
        )
        .await
        .unwrap()
        .expect("initial partial match");

        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut provisioner = PoolTestProvisioner {
            note_addr: note_addr.clone(),
            frame_model: frame_model.to_string(),
            backends: VecDeque::from([residual_five.clone(), residual_three.clone()]),
            calls: calls.clone(),
        };
        let unknown_tc = format!("0:{}", "9".repeat(64));
        owner_fills.lock().unwrap().push((
            i64::MAX - 5,
            MatchedFill {
                order_id: 999,
                token_contract: unknown_tc.clone(),
                ticks: 2,
                price_per_tick: 1_000_000_000,
            },
        ));
        let independent_for_shutdown = independent.clone();
        let unknown_shutdown = async move {
            while independent_for_shutdown.open_calls.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        }
        .fuse();
        tokio::pin!(unknown_shutdown);
        let unknown_policy = pool_test_policy(2);
        let unknown_error = {
            let mut provision = |model: String, nonce, price, ticks| {
                assert_eq!(model, frame_model);
                futures::future::ready(provisioner.provision(nonce, price, ticks))
            };
            run_seller_pool(
                &seller,
                vec![initial_deal(&gateway), independent_deal(&gateway)],
                context(root, &note_addr, frame_model, &gateway),
                &unknown_policy,
                &mut provision,
                unknown_shutdown.as_mut(),
                &mut false,
            )
            .await
            .expect_err("an owner fill without a handle must fail visibly")
        };
        // the failure names the TokenContract canonically, and a TokenContract is a self-DApp
        // account, so its DApp half is its own account id.
        let unknown_tc_account = unknown_tc
            .strip_prefix("0:")
            .expect("the fixture TokenContract is in the chain form");
        assert!(
            unknown_error
                .to_string()
                .contains(&format!("{unknown_tc_account}::{unknown_tc_account}")),
            "{unknown_error:#}"
        );
        assert!(
            calls.lock().unwrap().is_empty(),
            "unknown owner capacity must fail before residual provision"
        );
        owner_fills
            .lock()
            .unwrap()
            .retain(|(_, fill)| fill.token_contract != unknown_tc);
        let _ = (&mut seller.server_task).await;
        let mut seller = dexdo::seller::start_gateway_with_note(
            bind_addr,
            dexdo::seller::UpstreamConfig::Mock,
            note.clone(),
        )
        .await
        .unwrap();
        let gateway = seller.listen_addr.to_string();
        dexdo::seller::poll_match_and_maybe_open(
            &seller,
            initial.as_ref(),
            &initial_config(&gateway),
            &initial_watch.cursor_path,
        )
        .await
        .unwrap()
        .expect("unknown-fill restart restores the initial match");

        let seller_policy = pool_test_policy(4);
        let calls_for_shutdown = calls.clone();
        let interrupted = async move {
            while calls_for_shutdown.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        }
        .fuse();
        tokio::pin!(interrupted);
        let error = {
            let mut provision = |model: String, nonce, price, ticks| {
                assert_eq!(model, frame_model);
                futures::future::ready(provisioner.provision(nonce, price, ticks))
            };
            run_seller_pool(
                &seller,
                vec![initial_deal(&gateway), independent_deal(&gateway)],
                context(root, &note_addr, frame_model, &gateway),
                &seller_policy,
                &mut provision,
                interrupted.as_mut(),
                &mut false,
            )
            .await
            .expect_err("restart window is injected after fill persistence and before POST")
        };
        assert!(error
            .to_string()
            .contains("after provision and before POST"));
        let _ = (&mut seller.server_task).await;
        let mut seller = dexdo::seller::start_gateway_with_note(
            bind_addr,
            dexdo::seller::UpstreamConfig::Mock,
            note.clone(),
        )
        .await
        .unwrap();
        let gateway = seller.listen_addr.to_string();
        let valid_cursor = std::fs::read(&initial_watch.cursor_path).unwrap();
        let mut corrupt_cursor: serde_json::Value = serde_json::from_slice(&valid_cursor).unwrap();
        corrupt_cursor["fill"]["residual_ticks"] = serde_json::json!(4);
        std::fs::write(
            &initial_watch.cursor_path,
            serde_json::to_vec_pretty(&corrupt_cursor).unwrap(),
        )
        .unwrap();
        let calls_before_corrupt_restart = calls.lock().unwrap().len();
        let corrupt_shutdown = std::future::ready(()).fuse();
        tokio::pin!(corrupt_shutdown);
        let corrupt_error = {
            let mut provision = |model: String, nonce, price, ticks| {
                assert_eq!(model, frame_model);
                futures::future::ready(provisioner.provision(nonce, price, ticks))
            };
            run_seller_pool(
                &seller,
                vec![initial_deal(&gateway), independent_deal(&gateway)],
                context(root, &note_addr, frame_model, &gateway),
                &seller_policy,
                &mut provision,
                corrupt_shutdown.as_mut(),
                &mut false,
            )
            .await
            .expect_err("corrupt residual lineage must fail before replacement provision")
        };
        assert!(
            corrupt_error
                .to_string()
                .contains("invalid seller fill lineage"),
            "{corrupt_error:#}"
        );
        assert_eq!(
            calls.lock().unwrap().len(),
            calls_before_corrupt_restart,
            "corrupt residual lineage must produce zero provision POSTs"
        );
        std::fs::write(&initial_watch.cursor_path, valid_cursor).unwrap();
        seller.server_task.abort();
        let _ = (&mut seller.server_task).await;

        initial.exists.store(false, Ordering::Relaxed);
        let seller = dexdo::seller::start_gateway_with_note(
            bind_addr,
            dexdo::seller::UpstreamConfig::Mock,
            note,
        )
        .await
        .unwrap();
        let gateway = seller.listen_addr.to_string();
        let restored_pool = load_seller_pool_deals(
            &context(root, &note_addr, frame_model, &gateway),
            initial_deal(&gateway),
            8,
            |market| {
                let chain: Arc<dyn ChainBackend> =
                    if market.token_contract == independent.token_contract {
                        independent.clone()
                    } else if market.token_contract == residual_five.token_contract {
                        residual_five.clone()
                    } else {
                        return Err(anyhow::anyhow!(
                            "unexpected restored test TokenContract {}",
                            market.token_contract
                        ));
                    };
                Ok((chain, dexdo::seller::UpstreamConfig::Mock))
            },
        )
        .await
        .expect("restart must reconstruct the residual descendant from its existing handle");
        let final_backend = residual_three.clone();
        let final_token_contract = residual_three.token_contract.clone();
        let shutdown = async move {
            loop {
                if final_backend
                    .deal_state(&final_token_contract)
                    .await
                    .expect("successor state remains readable")
                    .is_some_and(|state| state.opened)
                {
                    return;
                }
                tokio::task::yield_now().await;
            }
        }
        .fuse();
        tokio::pin!(shutdown);
        let mut provision = |model: String, nonce, price, ticks| {
            assert_eq!(model, frame_model);
            futures::future::ready(provisioner.provision(nonce, price, ticks))
        };
        let isolated_error = run_seller_pool(
            &seller,
            restored_pool,
            context(root, &note_addr, frame_model, &gateway),
            &seller_policy,
            &mut provision,
            shutdown.as_mut(),
            &mut false,
        )
        .await
        .expect_err("one failed open is reported after unrelated residual service continues");
        assert!(isolated_error
            .to_string()
            .contains("injected per-deal open failure"));

        assert_eq!(
            *calls.lock().unwrap(),
            vec![(11, 1_000_000_000, 5), (12, 1_000_000_000, 3)]
        );
        assert_eq!(residual_five.post_calls.load(Ordering::Relaxed), 1);
        assert_eq!(residual_three.post_calls.load(Ordering::Relaxed), 1);
        assert_eq!(initial.open_calls.load(Ordering::Relaxed), 1);
        assert_eq!(independent.open_calls.load(Ordering::Relaxed), 1);
        assert_eq!(residual_five.open_calls.load(Ordering::Relaxed), 1);
        assert_eq!(residual_three.open_calls.load(Ordering::Relaxed), 1);
        assert!(!residual_five.opened.load(Ordering::Relaxed));
        assert!(residual_three.opened.load(Ordering::Relaxed));

        let initial_delivery = seller.state.delivery(&initial.token_contract);
        let independent_delivery = seller.state.delivery(&independent.token_contract);
        let five_delivery = seller.state.delivery(&residual_five.token_contract);
        let three_delivery = seller.state.delivery(&residual_three.token_contract);
        initial_delivery.count.store(7, Ordering::Relaxed);
        assert_eq!(five_delivery.count.load(Ordering::Relaxed), 0);
        assert_eq!(three_delivery.count.load(Ordering::Relaxed), 0);
        assert_eq!(independent_delivery.count.load(Ordering::Relaxed), 0);
        assert!(!Arc::ptr_eq(
            &initial_delivery.count,
            &independent_delivery.count
        ));
        assert!(!Arc::ptr_eq(&initial_delivery.count, &five_delivery.count));
        assert!(!Arc::ptr_eq(&five_delivery.count, &three_delivery.count));

        let initial_lineage = dexdo::seller::read_seller_fill_lineage(
            &initial_watch.cursor_path,
            &initial.token_contract,
        )
        .unwrap()
        .unwrap();
        assert_eq!(initial_lineage.replacement_nonce, Some(11));
        assert_eq!(
            initial_lineage.replacement_token_contract.as_deref(),
            Some(residual_five.token_contract.as_str())
        );
        let five_cursor =
            super::seller_watch_cursor_path(Some(root), &residual_five.token_contract).unwrap();
        let five_lineage =
            dexdo::seller::read_seller_fill_lineage(&five_cursor, &residual_five.token_contract)
                .unwrap()
                .unwrap();
        assert_eq!(five_lineage.replacement_nonce, Some(12));
        assert_eq!(
            five_lineage.replacement_token_contract.as_deref(),
            Some(residual_three.token_contract.as_str())
        );
        let final_cursor =
            super::seller_watch_cursor_path(Some(root), &residual_three.token_contract).unwrap();
        let final_fill =
            dexdo::seller::read_seller_fill_lineage(&final_cursor, &residual_three.token_contract)
                .unwrap()
                .unwrap();
        assert_eq!(
            (
                final_fill.offered_ticks,
                final_fill.matched_ticks,
                final_fill.residual_ticks
            ),
            (3, 2, 1)
        );
    }

    /// (issue example 2), driven through the real `run_seller_pool` entry: a deal that cannot
    /// start AND an owner fill the pool cannot
    /// account for, in the same run.

    /// Before this, the owner-fill audit ran first and won the `first_error` race, so the process
    /// printed `Error: seller owner fill... refusing to discard unknown capacity` while the real
    /// root cause was only logged -- which is what produced the wrong "the note is permanently
    /// wedged" conclusion in. The primary must now be on the headline and the owner-fill
    /// finding attached under `secondary`.
    #[tokio::test]
    async fn pool_reports_the_startup_failure_and_attaches_the_owner_fill_finding() {
        let root = tempfile::tempdir().expect("seller pool test directory");
        let root = root.path();
        let note = Arc::new(LocalNote::generate());
        let note_addr = format!("0:{}", "a".repeat(64));
        let frame_model = "openai/gpt-oss-20b";
        let owner_fills = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(PoolTestBackend::new(
            owner_fills.clone(),
            format!("0:{}", "1".repeat(64)),
            8,
            3,
            false,
            i64::MAX - 4,
        ));
        // An address nothing listens on: the pinned-TLS self-probe fails, exactly as in. The
        // reservation is HELD for the whole assertion -- a bind-and-drop hands the port
        // straight back to the kernel and something else can answer on it.
        let (_unreachable_hold, unreachable) = crate::test_refusing_endpoint::refusing_endpoint();
        let seller = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            note.clone(),
        )
        .await
        .unwrap();
        let deal = SellerPoolDeal {
            chain: backend.clone(),
            cfg: SellerConfig {
                token_contract: backend.token_contract.clone(),
                price_per_tick: backend.price_per_tick,
                max_ticks: backend.offered_ticks,
                subscription: false,
                gateway_advertise: unreachable.clone(),
                mock_token_count: 8,
            },
            watch: dexdo::seller::SellerMatchWatchConfig {
                cursor_path: root.join("cascade.cursor.json"),
                poll_interval: std::time::Duration::from_millis(1),
            },
            upstream: dexdo::seller::UpstreamConfig::Mock,
            nonce: 10,
            market: Some(dexdo_core::MarketManifest {
                network: "net-a".to_string(),
                frame_model: frame_model.to_string(),
                model_hash: dexdo_core::model_hash_for(frame_model),
                inference_order_book: format!("0:{}", "d".repeat(64)),
                root_model: format!("0:{}", "e".repeat(64)),
                token_contract: backend.token_contract.clone(),
                seller_note: note_addr.clone(),
                nonce: 10,
                price_per_tick: u128::from(backend.price_per_tick),
                max_ticks: u128::from(backend.offered_ticks),
            }),
        };
        // An owner fill for a TokenContract this pool has no handle for.
        let unknown_tc = format!("0:{}", "9".repeat(64));
        owner_fills.lock().unwrap().push((
            i64::MAX - 5,
            MatchedFill {
                order_id: 999,
                token_contract: unknown_tc.clone(),
                ticks: 2,
                price_per_tick: 1_000_000_000,
            },
        ));
        let mut provision = |_: String, _: u64, _: u64, _: u64| {
            futures::future::ready(Err(anyhow::anyhow!("no residual provision in this test")))
        };
        let shutdown = futures::future::pending::<()>().fuse();
        tokio::pin!(shutdown);
        let error = run_seller_pool(
            &seller,
            vec![deal],
            SellerPoolContext {
                deals_dir: Some(root),
                contracts: std::path::Path::new("manifest/deployed.manifest.json"),
                note_addr: &note_addr,
                frame_model,
                gateway_advertise: &unreachable,
                // `unreachable` is a closed loopback port, so it is not public and the
                // production default is still fatal -- the cascade under test is reached.
                advertise_probe: dexdo::seller::liveness::AdvertiseProbePolicy::default(),
                recover_publication: None,
            },
            &pool_test_policy(2),
            &mut provision,
            shutdown.as_mut(),
            &mut false,
        )
        .await
        .expect_err("a readiness failure plus an unaccounted owner fill must fail visibly");
        let rendered = error.to_string();

        // The PRIMARY is the concrete readiness error itself; the pool never wraps it in another
        // structured error just to attach the cascade note.
        let first_line = rendered.lines().next().unwrap();
        assert!(
            first_line.starts_with("error[E_ADVERTISE_UNREACHABLE] (network): advertised gateway"),
            "{rendered}"
        );
        assert_eq!(
            rendered.matches("error[E_ADVERTISE_UNREACHABLE]").count(),
            1
        );
        assert!(rendered.contains(&unreachable), "{rendered}");
        // The SECONDARY is attached, named, and never on the first line.
        assert!(
            !first_line.contains("unknown capacity"),
            "the cascade masqueraded as the primary: {rendered}"
        );
        assert!(
            rendered.contains(
                "secondary (pool owner-fill audit): error[E_POOL_UNKNOWN_OWNER_FILL] (pool):"
            ),
            "{rendered}"
        );
        // the cascade note names the TokenContract canonically, and a TokenContract is a
        // self-DApp account, so its DApp half is its own account id.
        let unknown_tc_account = unknown_tc
            .strip_prefix("0:")
            .expect("the fixture TokenContract is in the chain form");
        assert!(
            rendered.contains(&format!("{unknown_tc_account}::{unknown_tc_account}")),
            "{rendered}"
        );
        assert!(
            rendered.contains("CONSEQUENCE of the primary error"),
            "{rendered}"
        );
        assert_eq!(backend.post_calls.load(Ordering::Relaxed), 0);
        seller.server_task.abort();
    }

    /// E2E-ADV-16/L0, through the real `run_seller` boundary. The exact CLI shape the live campaign
    /// ran -- `--gateway-listen 127.0.0.1:0` with no
    /// `--gateway-advertise`. The advertise is inherited from the listen address at the command
    /// boundary -- before the gateway binds -- so it still carries the placeholder port `0`, which is
    /// not an address anyone can dial. Driven through the real `run_seller` entry: the endpoint a
    /// BUYER decrypts out of the handover must be the port the gateway actually bound, and a client
    /// must be able to connect to it. Without the resolution the buyer is handed `127.0.0.1:0`.
    #[tokio::test]
    async fn inherited_ephemeral_advertise_reaches_the_buyer_as_the_bound_port() {
        let root = tempfile::tempdir().unwrap();
        let seller_seed = [0x63; 32];
        crate::cli::support::write_owner_only_key_fixture(
            &root.path().join("seller.key"),
            &hex::encode(seller_seed),
        );
        let token_contract = format!("0:{}", "7".repeat(64));
        let chain = MockChainBackend::new(
            root.path().join("endpoints.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let buyer = Arc::new(LocalNote::generate());

        let args = crate::cli::args::SellerArgs {
            mock: crate::cli::args::MockFlags {
                mock_model: true,
                mock_chain: true,
            },
            identity: crate::cli::args::IdentityArgs {
                note_key: Some(root.path().join("seller.key")),
                note_index: 0,
                note_addr: None,
            },
            registry: crate::cli::args::ModelRegistryValidationArgs::default(),
            // The live shape: an ephemeral listen port, and no explicit advertise to inherit from.
            gateway_listen: "127.0.0.1:0".parse().unwrap(),
            gateway_advertise: None,
            allow_private_advertise: true,
            require_advertise_probe: false,
            endpoints_file: Some(root.path().join("endpoints.json")),
            deals_dir: Some(root.path().join("deals")),
            token_contract: Some(token_contract.clone()),
            market: None,
            nonce: Some(7),
            subscription: false,
            price_per_tick: dexdo_core::PRICE_STEP as u64,
            mock_token_count: 4,
            model: None,
            allow_unverified_model: false,
            models: root.path().join("unused-models.json"),
            policy: None,
            recover_publication: None,
            confirm_recover_publication: false,
        };
        assert_eq!(
            args.checked_gateway_advertise_addr().unwrap(),
            "127.0.0.1:0",
            "the boundary can only inherit the placeholder; the port is not chosen yet"
        );

        let seller = super::run_seller(args);
        tokio::pin!(seller);
        let scenario = async {
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    if matches!(
                        chain.confirm_offer_outcome(&token_contract).await,
                        Ok(Some(SellOfferOutcome::Rested { .. }))
                    ) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("run_seller must rest the offer");
            chain
                .place_buy(&token_contract, buyer.as_ref())
                .await
                .unwrap();
            let buyer_client = dexdo::buyer::Buyer::from_note(buyer.clone());
            // Same bound, same reason as the sibling row below: sized for the slowest runner.
            tokio::time::timeout(std::time::Duration::from_secs(120), async {
                loop {
                    if let Ok(handover) =
                        buyer_client.resolve_endpoint(&chain, &token_contract).await
                    {
                        break handover;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("buyer handover")
        };
        tokio::pin!(scenario);
        let handover = tokio::select! {
            result = &mut seller => panic!("seller exited before the buyer resolved the handover: {result:?}"),
            handover = &mut scenario => handover,
        };

        // By fact: the finalized rewrite goes through the same classifier/permission decision as
        // an explicit address, names a real port, and answers with the exact TLS identity handed to
        // the buyer. A plain TCP connect would not prove that the port belongs to this gateway.
        let advertised = handover
            .endpoint
            .strip_prefix("https://")
            .expect("gateway endpoint")
            .to_string();
        assert!(
            !advertised.ends_with(":0"),
            "the buyer was handed the unusable placeholder port: {}",
            handover.endpoint
        );
        assert_eq!(
            dexdo::seller::advertise::classify_advertise(&advertised),
            dexdo::seller::advertise::classify_advertise("127.0.0.1:0"),
            "ADV-16 finalized rewrite changed the inherited classifier verdict"
        );
        dexdo::seller::advertise::validate_advertise(&advertised, false, true)
            .expect("ADV-16 finalized private advertise retains the explicit opt-in");
        let socket: std::net::SocketAddr = advertised.parse().expect("host:port");
        assert_ne!(socket.port(), 0);
        dexdo::buyer::tls::connect_pinned(&handover.endpoint, &handover.tls_fingerprint)
            .await
            .expect("the finalized advertised address must present the buyer's pinned gateway");
    }

    /// run the seller twice in one working directory and the second run could not start at
    /// all. The first run leaves a seller deal handle behind; that deal then reaches its ordinary
    /// end and its `TokenContract` self-destructs, so the address in the handle answers no
    /// `getDeal`. The pool loader treated that as fatal and aborted -- before posting any offer --
    /// which turned the ordinary residue of a finished deal into an outage for the deal this run
    /// was actually invoked for.

    /// Driven through the real `run_seller` entry. The precondition is the residue itself, written
    /// with the same `deals::save_deal_handle` the production writer
    /// (`save_runtime_deal_handle` -> `persist_runtime_deal_handle`) ends in, and carrying the
    /// `network` that writer stamps (`"net-a"`) -- the mock network has its own tolerated-missing
    /// branch, so a handle recorded as mock can never reach the guard this pins. What is asserted
    /// is not the precondition: it is that the seller starts, rests its own offer, and serves a
    /// buyer, and that nothing is posted for the spent address.
    #[tokio::test]
    async fn a_spent_deal_handle_whose_token_contract_is_gone_does_not_stop_the_seller() {
        let root = tempfile::tempdir().unwrap();
        let seller_seed = [0x64; 32];
        crate::cli::support::write_owner_only_key_fixture(
            &root.path().join("seller.key"),
            &hex::encode(seller_seed),
        );
        let seller_note = dexdo_core::NoteTree::from_secret_hex(&hex::encode(seller_seed))
            .unwrap()
            .node(0)
            .unwrap();
        let seller_owner = format!(
            "0:{}",
            seller_note
                .pubkey()
                .ed
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        // The deal this run was invoked for, and the one a previous run finished with.
        let token_contract = format!("0:{}", "8".repeat(64));
        let spent_token_contract = format!("0:{}", "9".repeat(64));
        let chain = MockChainBackend::new(
            root.path().join("endpoints.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let buyer = Arc::new(LocalNote::generate());

        let deals_dir = root.path().join("deals");
        let spent_market = dexdo_core::MarketManifest {
            network: "net-a".to_string(),
            frame_model: "mock".to_string(),
            model_hash: dexdo_core::model_hash_for("mock"),
            inference_order_book: "mock".to_string(),
            root_model: "mock".to_string(),
            token_contract: spent_token_contract.clone(),
            seller_note: seller_owner.clone(),
            nonce: 4,
            price_per_tick: dexdo_core::PRICE_STEP,
            max_ticks: 4,
        };
        deals::save_deal_handle(
            &deals_dir,
            &deals::DealHandle {
                version: deals::DEAL_HANDLE_VERSION,
                handle: deals::make_handle_id(&spent_token_contract, deals::DealHandleRole::Seller),
                role: deals::DealHandleRole::Seller,
                network: "net-a".to_string(),
                token_contract: spent_token_contract.clone(),
                note_addr: seller_owner.clone(),
                frame_model: spent_market.frame_model.clone(),
                model_hash: Some(spent_market.model_hash.clone()),
                order_book: Some(spent_market.inference_order_book.clone()),
                root_model: Some(spent_market.root_model.clone()),
                market: Some(spent_market),
                contracts: root
                    .path()
                    .join("unused-contracts.json")
                    .display()
                    .to_string(),
                // The previous run's gateway, on a port this run does not own.
                endpoint: Some(deals::DealEndpointInfo {
                    kind: "gateway".to_string(),
                    value: "127.0.0.1:1".to_string(),
                }),
                created_order_ids: Vec::new(),
                created_at_unix: deals::now_unix().unwrap(),
            },
        )
        .unwrap();
        assert_eq!(
            chain.sell_offer_terms(&spent_token_contract).await.unwrap(),
            None,
            "the premise: the handle outlived its TokenContract, which answers no getDeal"
        );

        let args = crate::cli::args::SellerArgs {
            mock: crate::cli::args::MockFlags {
                mock_model: true,
                mock_chain: true,
            },
            identity: crate::cli::args::IdentityArgs {
                note_key: Some(root.path().join("seller.key")),
                note_index: 0,
                note_addr: None,
            },
            registry: crate::cli::args::ModelRegistryValidationArgs::default(),
            gateway_listen: "127.0.0.1:0".parse().unwrap(),
            gateway_advertise: None,
            allow_private_advertise: true,
            require_advertise_probe: false,
            endpoints_file: Some(root.path().join("endpoints.json")),
            deals_dir: Some(deals_dir),
            token_contract: Some(token_contract.clone()),
            market: None,
            nonce: Some(8),
            subscription: false,
            price_per_tick: dexdo_core::PRICE_STEP as u64,
            mock_token_count: 4,
            model: None,
            allow_unverified_model: false,
            models: root.path().join("unused-models.json"),
            policy: None,
            recover_publication: None,
            confirm_recover_publication: false,
        };

        let seller = super::run_seller(args);
        tokio::pin!(seller);
        let scenario = async {
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                loop {
                    if matches!(
                        chain.confirm_offer_outcome(&token_contract).await,
                        Ok(Some(SellOfferOutcome::Rested { .. }))
                    ) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the spent handle must not stop this run's offer from being posted");
            chain
                .place_buy(&token_contract, buyer.as_ref())
                .await
                .unwrap();
            let buyer_client = dexdo::buyer::Buyer::from_note(buyer.clone());
            // Reached only from the serving pool, i.e. strictly after the loader that used to abort.
            // The bound has to hold on the SLOWEST runner this suite runs on, not on the fastest.
            // `macos-latest` timed this loop out at 30 s while Linux completes it in well under a
            // second; the loop itself polls every 10 ms and returns the moment the endpoint
            // resolves, so a larger bound costs nothing when the handover works and only changes
            // how long a genuine hang is allowed to look like a hang.
            let handover = tokio::time::timeout(std::time::Duration::from_secs(120), async {
                loop {
                    if let Ok(handover) =
                        buyer_client.resolve_endpoint(&chain, &token_contract).await
                    {
                        break handover;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("buyer handover");
            assert_eq!(
                buyer_client
                    .connect_and_stream(&handover, &token_contract, 2)
                    .await
                    .unwrap()
                    .received,
                2
            );
            assert_eq!(
                chain
                    .confirm_offer_outcome(&spent_token_contract)
                    .await
                    .unwrap(),
                None,
                "the spent TokenContract must be skipped, not reposted"
            );
        };
        tokio::pin!(scenario);
        tokio::select! {
            result = &mut seller => panic!(
                "the seller refused to start over a deal handle whose TokenContract is gone: {result:?}"
            ),
            () = &mut scenario => {}
        }
    }

    /// Real `dexdo seller` argv, parsed by the real CLI parser -- so a regression proves what the
    /// OPERATOR typed reaches the lifecycle, not what a struct literal asserted.
    fn parsed_seller_args(extra: &[&str]) -> crate::cli::args::SellerArgs {
        use clap::Parser as _;
        let mut argv = vec![
            "dexdo",
            "seller",
            "--note-addr",
            "0:note",
            "--token-contract",
            "0:tc",
            "--model",
            "qwen",
        ];
        argv.extend_from_slice(extra);
        let crate::Command::Seller(args) = crate::Cli::try_parse_from(argv)
            .expect("seller argv parses")
            .command
        else {
            panic!("expected Command::Seller");
        };
        args
    }

    fn advertise_pool_deal(
        backend: Arc<PoolTestBackend>,
        gateway_advertise: &str,
        cursor_path: std::path::PathBuf,
    ) -> SellerPoolDeal {
        SellerPoolDeal {
            cfg: SellerConfig {
                token_contract: backend.token_contract.clone(),
                price_per_tick: backend.price_per_tick,
                max_ticks: backend.offered_ticks,
                subscription: false,
                gateway_advertise: gateway_advertise.to_string(),
                mock_token_count: 4,
            },
            chain: backend,
            watch: dexdo::seller::SellerMatchWatchConfig {
                cursor_path,
                poll_interval: std::time::Duration::from_millis(1),
            },
            upstream: dexdo::seller::UpstreamConfig::Mock,
            nonce: 11,
            market: None,
        }
    }

    /// E2E-ROW: E2E-ADV-16/L0
    #[tokio::test]
    async fn pr795_edge_explicit_zero_gateway_advertise_fails_as_config_and_posts_nothing() {
        let root = tempfile::tempdir().unwrap();
        let endpoints = root.path().join("endpoints.json");
        let endpoints_arg = endpoints.to_string_lossy().into_owned();
        let args = parsed_seller_args(&[
            "--endpoints-file",
            &endpoints_arg,
            "--gateway-advertise",
            "seller.example.net:0",
        ]);

        let error = super::run_seller(args)
            .await
            .expect_err("an explicit advertise port 0 must fail before seller setup")
            .to_string();
        assert_eq!(
            error,
            "error[E_ADVERTISE_NOT_PUBLIC] (config): --gateway-advertise \
             seller.example.net:0 uses port 0, which no remote buyer can dial\n  \
             hint: pass a public host:port reachable from the internet, or run on a public host; \
             for local/LAN testing only, use --allow-private-advertise"
        );
        assert!(
            !endpoints.exists() && !endpoints.with_extension("chainstate.json").exists(),
            "the invalid explicit advertise must fail before any state or SELL can be written"
        );
    }

    /// The real pool boundary must retain the canonical structured error and the concrete I/O
    /// source from the fatal readiness probe; an ask behind that address must never be posted.
    #[tokio::test]
    async fn fatal_private_advertise_reaches_the_pool_boundary_with_its_typed_source() {
        let root = tempfile::tempdir().unwrap();
        let (_reserved, advertise) = crate::test_refusing_endpoint::refusing_endpoint();
        let args = parsed_seller_args(&["--gateway-advertise", &advertise]);
        let boundary = args
            .checked_gateway_advertise_addr()
            .expect_err("without the opt-in a private advertise is refused at the boundary")
            .to_string();
        assert!(
            boundary.contains("error[E_ADVERTISE_NOT_PUBLIC] (config)")
                && boundary.contains(&advertise),
            "{boundary}"
        );
        let seller = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let note_addr = format!("0:{}", "b".repeat(64));
        let backend = Arc::new(PoolTestBackend::new(
            Arc::new(Mutex::new(Vec::new())),
            format!("0:{}", "2".repeat(64)),
            8,
            8,
            false,
            i64::MAX - 4,
        ));
        let deal = advertise_pool_deal(
            backend.clone(),
            &advertise,
            root.path().join("no-optin.cursor.json"),
        );
        let mut provision = |_: String, _: u64, _: u64, _: u64| {
            futures::future::ready(Err(anyhow::anyhow!("no residual provision in this test")))
        };
        let shutdown = futures::future::pending::<()>().fuse();
        tokio::pin!(shutdown);
        let error = run_seller_pool(
            &seller,
            vec![deal],
            SellerPoolContext {
                deals_dir: Some(root.path()),
                contracts: std::path::Path::new("manifest/deployed.manifest.json"),
                note_addr: &note_addr,
                frame_model: "mock",
                gateway_advertise: &advertise,
                advertise_probe: args.advertise_probe_policy(),
                recover_publication: None,
            },
            &pool_test_policy(1),
            &mut provision,
            shutdown.as_mut(),
            &mut false,
        )
        .await
        .expect_err("a private advertise nobody opted into must still fail closed");
        let structured = error
            .downcast_ref::<dexdo_core::DexdoError>()
            .expect("the pool boundary must expose the concrete DexdoError to main");
        assert_eq!(
            structured.code(),
            dexdo_core::error_codes::E_ADVERTISE_UNREACHABLE.code()
        );
        let first_source = std::error::Error::source(structured)
            .expect("the structured error must own the health failure");
        assert!(
            first_source
                .downcast_ref::<dexdo::seller::liveness::HealthFailure>()
                .is_some(),
            "unexpected first source: {first_source:?}"
        );
        let mut source = Some(first_source);
        let mut saw_io = false;
        while let Some(current) = source {
            saw_io |= current.downcast_ref::<std::io::Error>().is_some();
            source = current.source();
        }
        assert!(saw_io, "the original typed I/O source was flattened away");
        let rendered = structured.to_string();
        assert!(
            rendered.contains("error[E_ADVERTISE_UNREACHABLE] (network)")
                && rendered.contains(&advertise),
            "{rendered}"
        );
        assert_eq!(
            rendered.matches("error[E_ADVERTISE_UNREACHABLE]").count(),
            1
        );
        assert_eq!(rendered.matches("\n  hint: ").count(), 1);
        assert_eq!(
            backend.post_calls.load(Ordering::Relaxed),
            0,
            "nothing may be posted behind a fatal readiness failure"
        );
        seller.server_task.abort();
    }

    /// `--allow-private-advertise` admits a private address CLASS; it never vouches for what is
    /// listening there. An address that ANSWERS with a foreign certificate is proof of the WRONG
    /// endpoint, never a tolerable transport artifact -- so through the real lifecycle, with that
    /// flag passed, readiness stays fatal and the backend records nothing.
    #[tokio::test]
    async fn the_opt_in_still_fails_closed_on_a_wrong_gateway() {
        let root = tempfile::tempdir().unwrap();
        let seller = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let foreign = dexdo::seller::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            dexdo::seller::UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        assert_ne!(seller.tls_fingerprint, foreign.tls_fingerprint);
        let advertise = foreign.listen_addr.to_string();
        let args = parsed_seller_args(&[
            "--allow-private-advertise",
            "--gateway-advertise",
            &advertise,
        ]);
        let note_addr = format!("0:{}", "c".repeat(64));
        let backend = Arc::new(PoolTestBackend::new(
            Arc::new(Mutex::new(Vec::new())),
            format!("0:{}", "3".repeat(64)),
            8,
            8,
            false,
            i64::MAX - 4,
        ));
        let deal = advertise_pool_deal(
            backend.clone(),
            &advertise,
            root.path().join("foreign.cursor.json"),
        );
        let mut provision = |_: String, _: u64, _: u64, _: u64| {
            futures::future::ready(Err(anyhow::anyhow!("no residual provision in this test")))
        };
        let shutdown = futures::future::pending::<()>().fuse();
        tokio::pin!(shutdown);
        let error = run_seller_pool(
            &seller,
            vec![deal],
            SellerPoolContext {
                deals_dir: Some(root.path()),
                contracts: std::path::Path::new("manifest/deployed.manifest.json"),
                note_addr: &note_addr,
                frame_model: "mock",
                gateway_advertise: &advertise,
                advertise_probe: args.advertise_probe_policy(),
                recover_publication: None,
            },
            &pool_test_policy(1),
            &mut provision,
            shutdown.as_mut(),
            &mut false,
        )
        .await
        .expect_err("a foreign gateway on the advertised address must stay fatal")
        .to_string();
        assert!(
            error.contains("error[E_ADVERTISE_WRONG_GATEWAY] (tls)"),
            "{error}"
        );
        assert_eq!(
            backend.post_calls.load(Ordering::Relaxed),
            0,
            "a wrong-endpoint proof must never post"
        );
        seller.server_task.abort();
        foreign.server_task.abort();
    }

    fn mock_pool_seller_args(
        root: &std::path::Path,
        token_contract: String,
        nonce: u64,
        gateway_listen: std::net::SocketAddr,
    ) -> crate::cli::args::SellerArgs {
        crate::cli::args::SellerArgs {
            mock: crate::cli::args::MockFlags {
                mock_model: true,
                mock_chain: true,
            },
            identity: crate::cli::args::IdentityArgs {
                note_key: Some(root.join("seller.key")),
                note_index: 0,
                note_addr: None,
            },
            registry: crate::cli::args::ModelRegistryValidationArgs::default(),
            gateway_listen,
            gateway_advertise: None,
            allow_private_advertise: false,
            require_advertise_probe: false,
            endpoints_file: Some(root.join("endpoints.json")),
            deals_dir: Some(root.join("deals")),
            token_contract: Some(token_contract),
            market: None,
            nonce: Some(nonce),
            subscription: false,
            price_per_tick: dexdo_core::PRICE_STEP as u64,
            mock_token_count: 4,
            model: None,
            allow_unverified_model: false,
            models: root.join("unused-models.json"),
            policy: None,
            recover_publication: None,
            confirm_recover_publication: false,
        }
    }

    #[tokio::test]
    async fn run_seller_raw_mock_partial_fill_relists_and_serves_two_buyers() {
        let root = tempfile::tempdir().unwrap();
        let seller_seed = [0x61; 32];
        crate::cli::support::write_owner_only_key_fixture(
            &root.path().join("seller.key"),
            &hex::encode(seller_seed),
        );
        let seller_note = Arc::new(
            dexdo_core::NoteTree::from_secret_hex(&hex::encode(seller_seed))
                .unwrap()
                .node(0)
                .unwrap(),
        );
        let seller_owner = format!(
            "0:{}",
            seller_note
                .pubkey()
                .ed
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let initial_tc = format!("0:{}", "1".repeat(64));
        let chain = MockChainBackend::new(
            root.path().join("endpoints.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        // shape B: `run_seller` makes the ONE bind and resolves the inherited `:0` advertise to
        // the port it actually got. Reserving a port here and releasing it before `run_seller`
        // binds hands it back to the kernel for the whole of `prepare_seller_offer` below.
        let gateway: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let cfg = SellerConfig {
            token_contract: initial_tc.clone(),
            price_per_tick: dexdo_core::PRICE_STEP as u64,
            max_ticks: 1024,
            subscription: false,
            gateway_advertise: gateway.to_string(),
            mock_token_count: 4,
        };
        prepare_seller_offer(seller_note.as_ref(), &chain, &cfg, Some(&seller_owner))
            .await
            .unwrap();
        let buyer_a = Arc::new(LocalNote::generate());
        let buyer_b = Arc::new(LocalNote::generate());
        chain
            .place_buy_ticks(&initial_tc, buyer_a.as_ref(), 2)
            .await
            .unwrap();

        let seller = super::run_seller(mock_pool_seller_args(
            root.path(),
            initial_tc.clone(),
            7,
            gateway,
        ));
        tokio::pin!(seller);
        let residual_tc = format!(
            "0:{}",
            dexdo_core::model_hash_for(&format!("{seller_owner}:mock:8")).trim_start_matches("0x")
        );
        let scenario = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if matches!(
                        chain.confirm_offer_outcome(&residual_tc).await,
                        Ok(Some(SellOfferOutcome::Rested { .. }))
                    ) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("raw startup must reach the pool and POST exact residual");
            chain
                .place_buy(&residual_tc, buyer_b.as_ref())
                .await
                .unwrap();

            let buyer_a_client = dexdo::buyer::Buyer::from_note(buyer_a.clone());
            let buyer_b_client = dexdo::buyer::Buyer::from_note(buyer_b.clone());
            let handover_a = tokio::time::timeout(std::time::Duration::from_secs(35), async {
                loop {
                    if let Ok(handover) = buyer_a_client.resolve_endpoint(&chain, &initial_tc).await
                    {
                        break handover;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("buyer A handover");
            let handover_b = tokio::time::timeout(std::time::Duration::from_secs(35), async {
                loop {
                    if let Ok(handover) =
                        buyer_b_client.resolve_endpoint(&chain, &residual_tc).await
                    {
                        break handover;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("buyer B handover");
            assert_eq!(
                buyer_a_client
                    .connect_and_stream(&handover_a, &initial_tc, 2)
                    .await
                    .unwrap()
                    .received,
                2
            );
            chain.stop(&initial_tc, buyer_a.as_ref()).await.unwrap();
            assert_eq!(
                buyer_b_client
                    .connect_and_stream(&handover_b, &residual_tc, 2)
                    .await
                    .unwrap()
                    .received,
                2
            );
        };
        tokio::pin!(scenario);
        tokio::select! {
            result = &mut seller => panic!("seller exited before buyer B continued: {result:?}"),
            () = &mut scenario => {}
        }
    }

    #[tokio::test]
    async fn run_seller_terminal_raw_ancestor_resumes_linked_descendant() {
        let root = tempfile::tempdir().unwrap();
        let seller_seed = [0x62; 32];
        crate::cli::support::write_owner_only_key_fixture(
            &root.path().join("seller.key"),
            &hex::encode(seller_seed),
        );
        let seller_note = Arc::new(
            dexdo_core::NoteTree::from_secret_hex(&hex::encode(seller_seed))
                .unwrap()
                .node(0)
                .unwrap(),
        );
        let seller_owner = format!(
            "0:{}",
            seller_note
                .pubkey()
                .ed
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        let ancestor_tc = format!("0:{}", "2".repeat(64));
        let descendant_tc = format!("0:{}", "3".repeat(64));
        let chain = MockChainBackend::new(
            root.path().join("endpoints.json"),
            ProtocolConsts::canonical(),
            DobParams::canonical(),
        );
        let gateway: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let descendant_cfg = SellerConfig {
            token_contract: descendant_tc.clone(),
            price_per_tick: dexdo_core::PRICE_STEP as u64,
            max_ticks: 2,
            subscription: false,
            gateway_advertise: gateway.to_string(),
            mock_token_count: 4,
        };
        prepare_seller_offer(
            seller_note.as_ref(),
            &chain,
            &descendant_cfg,
            Some(&seller_owner),
        )
        .await
        .unwrap();
        let buyer = Arc::new(LocalNote::generate());
        chain
            .place_buy(&descendant_tc, buyer.as_ref())
            .await
            .unwrap();

        let market = dexdo_core::MarketManifest {
            network: "mock".to_string(),
            frame_model: "mock".to_string(),
            model_hash: dexdo_core::model_hash_for("mock"),
            inference_order_book: "mock".to_string(),
            root_model: "mock".to_string(),
            token_contract: descendant_tc.clone(),
            seller_note: seller_owner.clone(),
            nonce: 8,
            price_per_tick: u128::from(descendant_cfg.price_per_tick),
            max_ticks: u128::from(descendant_cfg.max_ticks),
        };
        let deals_dir = root.path().join("deals");
        deals::save_deal_handle(
            &deals_dir,
            &deals::DealHandle {
                version: deals::DEAL_HANDLE_VERSION,
                handle: deals::make_handle_id(&descendant_tc, deals::DealHandleRole::Seller),
                role: deals::DealHandleRole::Seller,
                network: "mock".to_string(),
                token_contract: descendant_tc.clone(),
                note_addr: seller_owner.clone(),
                frame_model: market.frame_model.clone(),
                model_hash: Some(market.model_hash.clone()),
                order_book: Some(market.inference_order_book.clone()),
                root_model: Some(market.root_model.clone()),
                market: Some(market),
                contracts: root
                    .path()
                    .join("unused-contracts.json")
                    .display()
                    .to_string(),
                endpoint: Some(deals::DealEndpointInfo {
                    kind: "gateway".to_string(),
                    value: gateway.to_string(),
                }),
                created_order_ids: Vec::new(),
                created_at_unix: deals::now_unix().unwrap(),
            },
        )
        .unwrap();
        let cursor_path = super::seller_watch_cursor_path(Some(&deals_dir), &ancestor_tc).unwrap();
        std::fs::create_dir_all(cursor_path.parent().unwrap()).unwrap();
        std::fs::write(
            &cursor_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 1,
                "token_contract": ancestor_tc,
                "source": MatchWatchCursor::new(0),
                "last_polled_unix": null,
                "opened_at_unix": null,
                "fill": dexdo::seller::SellerFillLineage {
                    order_id: 1,
                    offered_ticks: 4,
                    matched_ticks: 2,
                    residual_ticks: 2,
                    price_per_tick: dexdo_core::PRICE_STEP as u64,
                    replacement_nonce: Some(8),
                    replacement_token_contract: Some(descendant_tc.clone()),
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let seller = super::run_seller(mock_pool_seller_args(
            root.path(),
            ancestor_tc.clone(),
            7,
            gateway,
        ));
        tokio::pin!(seller);
        let scenario = async {
            let buyer_client = dexdo::buyer::Buyer::from_note(buyer);
            let handover = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    if let Ok(handover) =
                        buyer_client.resolve_endpoint(&chain, &descendant_tc).await
                    {
                        break handover;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("production entry must resume linked descendant");
            assert_eq!(
                buyer_client
                    .connect_and_stream(&handover, &descendant_tc, 2)
                    .await
                    .unwrap()
                    .received,
                2
            );
            assert_eq!(
                chain.confirm_offer_outcome(&ancestor_tc).await.unwrap(),
                None,
                "terminal ancestor must not be reposted"
            );
        };
        tokio::pin!(scenario);
        tokio::select! {
            result = &mut seller => panic!("seller exited before descendant service: {result:?}"),
            () = &mut scenario => {}
        }
    }

    #[test]
    fn policy_seller_fields_dispatch_or_fail_closed_explicitly() {
        let source = include_str!("seller.rs");
        let policy_source = include_str!("policy.rs");
        let seller_policy_source = include_str!("seller_policy.rs");
        let run = source
            .find("pub(crate) async fn run_seller")
            .expect("run_seller present");
        assert!(
            policy_source.contains("fn validate_seller_runtime_capabilities"),
            "seller runtime policy capability validation must remain explicit"
        );
        assert!(
            seller_policy_source.contains("chain.release_dispute(token_contract)"),
            "seller dispute_against_me=release_if_clean must invoke release_dispute"
        );
        assert!(
            seller_policy_source.contains("policy_action_unsupported"),
            "seller unsupported republish/cleanup surfaces must fail closed explicitly"
        );
        assert!(
            seller_policy_source.contains("action=retire_gateway"),
            "seller buyer_no_show=retire_gateway must have an explicit runtime terminal action"
        );

        let end = source[run..]
            .find("#[cfg(test)]\nmod tests")
            .map(|offset| run + offset)
            .expect("run_seller end marker present");
        let body = &source[run..end];
        let validate = body
            .find("load_seller_runtime_policy")
            .expect("shared seller policy validation present");
        let doctor = body
            .find("chain_doctor_preflight")
            .expect("a real chain preflight present");
        let startup = body
            .find("run_seller_pool(")
            .expect("shared seller pool startup seam present");
        assert!(validate < doctor);
        assert!(validate < startup);

        let advance_start = source
            .find("async fn record_advance_result")
            .expect("per-deal result handler present");
        let advance_end = source[advance_start..]
            .find("struct SellerPoolContext")
            .map(|offset| advance_start + offset)
            .expect("per-deal result handler end present");
        let advance = &source[advance_start..advance_end];
        assert!(
            advance.contains("apply_seller_terminal_policy")
                && advance.contains("is_err_not_open(&error)")
                && advance.contains("classify_by_fact_advance_failure")
                && advance.contains("apply_seller_dispute_policy"),
            "ERR_NOT_OPEN must be classified before the seller turns it into a process fault"
        );
        let classify = advance
            .find("classify_by_fact_advance_failure")
            .expect("ERR_NOT_OPEN classifier present");
        let policy = advance
            .find("apply_seller_dispute_policy")
            .expect("non-ERR_NOT_OPEN dispute policy fallback present");
        assert!(
            classify < policy,
            "unsafe ERR_NOT_OPEN must return a money-path fault before generic dispute policy can consume it"
        );
        assert!(body.contains("run_seller_pool"));
    }

    include!("seller_1057_shutdown_tests.rs");

    include!("seller_1773_shutdown_drain_tests.rs");

    /// The observed parent skips shutdown polling during startup. Its watcher then opens the
    /// match and queues the residual in `fill_rx` together. On the next pool turn the biased
    /// `select!` must poll this newly-ready shutdown before the queued fill, so the pool's own
    /// shutdown arm consumes it while `pending` is still empty and the guard cannot run.
    #[tokio::test]
    async fn issue_1150_pool_select_shutdown_records_the_callers_disposition() {
        let pool = issue_1057_pool(2).await;
        let provision_calls = Arc::new(AtomicU64::new(0));
        let mut provision = {
            let provision_calls = provision_calls.clone();
            move |_: String, _: u64, _: u64, _: u64| {
                provision_calls.fetch_add(1, Ordering::Relaxed);
                futures::future::ready(Err::<
                    (dexdo_core::MarketManifest, Arc<dyn ChainBackend>),
                    anyhow::Error,
                >(anyhow::anyhow!(
                    "the pool select must stop before provisioning"
                )))
            }
        };
        let shutdown = Issue1057Shutdown::after_parent_open(pool.parent.clone(), 0);
        tokio::pin!(shutdown);
        let mut shutdown_requested = false;

        run_seller_pool(
            &pool.seller,
            vec![pool.deal.clone()],
            pool.context(),
            &pool_test_policy(2),
            &mut provision,
            shutdown.as_mut(),
            &mut shutdown_requested,
        )
        .await
        .expect("the pool's own shutdown arm is a normal operator stop");

        assert_eq!(
            pool.parent.open_calls.load(Ordering::Relaxed),
            1,
            "the real watched match must arm shutdown only after startup"
        );
        assert_eq!(
            shutdown.as_ref().get_ref().polls_after_trigger,
            1,
            "the first shutdown poll after the watched match opens must consume it"
        );
        assert!(
            futures::future::FusedFuture::is_terminated(shutdown.as_ref().get_ref()),
            "the pool's own select must consume the fused shutdown"
        );
        assert_eq!(
            provision_calls.load(Ordering::Relaxed),
            0,
            "the queued fill must not reach the pending/provision path before the biased shutdown arm"
        );
        assert!(
            shutdown_requested,
            "the caller must observe the stop consumed by the pool's own select"
        );
    }
}
