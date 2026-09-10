use super::{
    clear_offer_submission, inspect_seller_offer, pending_offer_submission,
    persist_offer_submission, prepare_seller_offer, reconcile_pending_offer_submission,
    validate_resting_offer, wait_for_match, RunningSeller, SellerConfig, SellerMatchWatchConfig,
    SellerOfferInspection, SellerOfferStartup,
};
use anyhow::{anyhow, Result};
use dexdo_core::{
    market::{RestingSellCancelStartError, RestingSellCancelWatch},
    params::{
        SellerLivenessParams, SELLER_UPSTREAM_HEALTH_TIMEOUT_MAX_ATTEMPTS,
        TRANSIENT_READ_TOTAL_BUDGET,
    },
    ChainBackend, Match,
};
use dexdo_proto::{ChallengeRequest, GatewayClient};
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use crate::seller::auth::HEALTH_CHALLENGE_TC;

fn display_token_contract(token_contract: &str) -> String {
    dexdo_core::address::display_self_dapp(token_contract)
}

fn display_dexdo_address(address: &str) -> String {
    dexdo_core::address::display(address)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthComponent {
    GatewayTask,
    AdvertisedGateway,
    UpstreamModel,
}

impl HealthComponent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GatewayTask => "gateway_task",
            Self::AdvertisedGateway => "advertised_gateway",
            Self::UpstreamModel => "upstream_authentication_and_model",
        }
    }
}

#[derive(Debug)]
pub struct HealthFailure {
    pub component: HealthComponent,
    pub timed_out: bool,
    pub detail: String,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl HealthFailure {
    pub fn new(component: HealthComponent, timed_out: bool, detail: impl Into<String>) -> Self {
        Self {
            component,
            timed_out,
            detail: detail.into(),
            source: None,
        }
    }

    fn with_source(
        mut self,
        source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        self.source = Some(source.into());
        self
    }

    fn into_startup_error(self, advertised: &str) -> anyhow::Error {
        let probe = self
            .source
            .as_deref()
            .and_then(|source| source.downcast_ref::<ProbeFault>())
            .map(|fault| (fault.stage, fault.wrong_endpoint));
        match probe {
            Some((stage, wrong_endpoint)) => anyhow::Error::new(
                advertise_probe_fault(advertised, stage, wrong_endpoint).with_source(self),
            ),
            None => {
                let context = if self.component == HealthComponent::UpstreamModel
                    && self.detail.contains("startup capability probe")
                {
                    format!("seller readiness failed before SELL: {}", self.detail)
                } else {
                    "seller readiness failed before SELL".to_string()
                };
                anyhow::Error::new(self).context(context)
            }
        }
    }
}

impl std::fmt::Display for HealthFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} {}: {}",
            self.component.as_str(),
            if self.timed_out {
                "timed out"
            } else {
                "failed"
            },
            self.detail
        )
    }
}

impl std::error::Error for HealthFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestingOfferIdentity {
    pub owner_note: String,
    pub token_contract: String,
    pub order_id: u128,
}

#[derive(Debug)]
pub enum CancellationDisposition {
    Cancelled,
    AlreadyAbsent,
    AlreadyMatched(Match),
    UnknownFailure {
        known_result: String,
    },
    /// The chain terminally rejected the accepted cancel request and the exact order still rests.
    RejectedStillResting {
        known_result: String,
    },
    /// no cancellation was attempted, because the order died of its own deadline.

    /// Distinct from `AlreadyAbsent`: on-chain expiry removal is lazy, so the row may well still be
    /// sitting in the book. It is simply unmatchable, and claiming it is gone would be a fact this
    /// client never read.
    NotAttemptedExpired,
    /// the expired order was reaped off the book, and no successor was posted for `reason`.

    /// The residual capacity is idle and stays idle until an operator acts, so the reason travels
    /// with the outcome instead of only into a log line.
    ReapedNotRelisted {
        reason: String,
    },
}

impl CancellationDisposition {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::AlreadyAbsent => "already_absent",
            Self::AlreadyMatched(_) => "already_matched",
            Self::UnknownFailure { .. } => "unknown_failure",
            Self::RejectedStillResting { .. } => "rejected_still_resting",
            Self::NotAttemptedExpired => "not_attempted_expired",
            Self::ReapedNotRelisted { .. } => "reaped_not_relisted",
        }
    }

    pub fn known_result(&self) -> Option<&str> {
        match self {
            Self::UnknownFailure { known_result } => Some(known_result),
            Self::RejectedStillResting { known_result } => Some(known_result),
            Self::ReapedNotRelisted { reason } => Some(reason),
            _ => None,
        }
    }

    /// has the chain PROVEN that no buyer can still match this ask?

    /// This is the one question that decides whether the seller may stop serving a deal, and it is
    /// answered here so that every caller asks it rather than re-deriving it. Three places already
    /// spelled the same `matches!` out by hand -- the gateway abort in this file, and the startup and
    /// shutdown guards in `cli/seller.rs` -- and a fourth place, the running retire path, did not
    /// spell it at all. That is how happened: `liveness` deliberately kept the gateway alive
    /// for the two unproven outcomes, and the pool retired the deal anyway, cancelling the decision
    /// one layer down. A predicate copied into N places is a policy that can disagree with itself,
    /// so it lives on the type that carries the outcome.

    /// Proven, and why:

    /// * `Cancelled` / `AlreadyAbsent` / `ReapedNotRelisted` -- the order is off the book. The book
    /// has ONE removal point (`InferenceOrderBook._removeFromBook`), so off means unmatchable.
    /// * `AlreadyMatched` -- it matched; it cannot match again.
    /// * `NotAttemptedExpired` -- the row may well still be sitting there, but the matcher drops an
    /// expired maker inline on every crossing (`_isExpired(mk.deadline)`, three sites in the match
    /// walk), so it is unmatchable without anyone writing anything.

    /// NOT proven:

    /// * `RejectedStillResting` -- the chain refused the cancel and the exact order still rests.
    /// * `UnknownFailure` -- nothing established either way, which is not the same as "gone".
    pub fn proven_unmatchable(&self) -> bool {
        match self {
            Self::Cancelled
            | Self::AlreadyAbsent
            | Self::AlreadyMatched(_)
            | Self::NotAttemptedExpired
            | Self::ReapedNotRelisted { .. } => true,
            Self::UnknownFailure { .. } | Self::RejectedStillResting { .. } => false,
        }
    }
}

impl std::fmt::Display for CancellationDisposition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())?;
        if let Some(known_result) = self.known_result() {
            write!(formatter, " ({known_result})")?;
        }
        Ok(())
    }
}

/// The authoritative expiry of the exact supervised SELL, and the moment the seller observed it
/// .

/// `deadline` is read back out of the order book, never reconstructed as `post time + MAX_SELL_TTL`:
/// the chain anchors it at `block.timestamp` inside `PrivateNote.postSellOffer`
/// (`contracts/dex/PrivateNote.sol:793`), and a client clock that differs from the node's would put
/// the reconstruction on the wrong side of the boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestingOfferExpiry {
    /// The absolute deadline the book holds for this order.
    pub deadline: u64,
    /// Unix seconds at the read that proved the deadline had passed.
    pub observed_at: u64,
}

#[derive(Debug)]
pub enum RestingStopReason {
    Health(HealthFailure),
    Shutdown,
    Watcher(String),
    /// the supervised order reached its own on-chain deadline.

    /// Terminal, and deliberately NOT a health failure -- nothing is wrong with this seller. Its offer
    /// simply stopped being executable, which is the ordinary end of every SELL: the deadline is
    /// mandatory and capped at `MAX_SELL_TTL = 3600` (`contracts/dex/PrivateNote.sol:41,792`), so a
    /// seller that runs longer than an hour reaches this outcome by design, not by fault.
    Expired(RestingOfferExpiry),
}

#[derive(Debug)]
pub enum RestingSellerOutcome {
    Matched(Match),
    Stopped {
        reason: RestingStopReason,
        disposition: CancellationDisposition,
    },
}

#[derive(Debug)]
pub enum SellerStartupOutcome {
    Ready(SellerOfferStartup),
    Stopped {
        identity: Option<RestingOfferIdentity>,
        reason: RestingStopReason,
        disposition: CancellationDisposition,
    },
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn trace_health(
    identity: Option<&RestingOfferIdentity>,
    token_contract: &str,
    component: HealthComponent,
    status: &str,
) {
    let token_contract = display_token_contract(token_contract);
    let owner_note = identity
        .map(|value| display_dexdo_address(&value.owner_note))
        .unwrap_or_else(|| "pending".to_string());
    tracing::info!(
        event = "seller_health",
        timestamp = unix_timestamp(),
        token_contract,
        owner_note,
        order_id = identity.map(|value| value.order_id),
        component = component.as_str(),
        status,
        "seller readiness component checked"
    );
}

/// What the operator demands of the `advertised_gateway` self-probe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AdvertiseProbePolicy {
    /// Default. A TRANSPORT-level self-probe failure against a **public** advertised address
    /// degrades to a loud warning and the offer still posts: from the seller host the advertised
    /// address is a known-limited observation point. A probe
    /// that proves the address is the WRONG endpoint (pinned-certificate mismatch, foreign gateway)
    /// stays fatal, and so does any failure against a non-public advertised address.
    #[default]
    TolerateTunneledTransportFailure,
    /// `--require-advertise-probe`: every self-probe failure is fatal, as before.
    Required,
}

/// A failed stage of the shared pinned-TLS (h2) gateway probe, with its source chain.
#[derive(Debug)]
pub struct ProbeFault {
    /// `endpoint_parse` / `dns_resolve` / `tcp_connect` / `tls_handshake` /
    /// `http2_handshake` / `grpc_challenge` / `challenge_response` / `handshake_timeout`.
    stage: &'static str,
    /// `true` when the address answered but is provably not this gateway -- never a tunnel artifact.
    wrong_endpoint: bool,
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
}

impl ProbeFault {
    fn transport(
        stage: &'static str,
        source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        Self {
            stage,
            wrong_endpoint: false,
            source: source.into(),
        }
    }

    fn wrong_endpoint(
        stage: &'static str,
        source: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        Self {
            stage,
            wrong_endpoint: true,
            source: source.into(),
        }
    }

    /// The exact probe boundary that failed.
    pub const fn stage(&self) -> &'static str {
        self.stage
    }

    /// Whether the peer answered but proved it was not the gateway identified by the handover.
    pub const fn is_wrong_endpoint(&self) -> bool {
        self.wrong_endpoint
    }

    /// The preserved underlying error chain, without replacing it with an opaque transport error.
    pub fn cause_detail(&self) -> String {
        let mut causes = vec![self.source.to_string()];
        let mut source = self.source.source();
        while let Some(cause) = source {
            causes.push(cause.to_string());
            source = cause.source();
        }
        causes.join("; caused by: ")
    }
}

impl std::fmt::Display for ProbeFault {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "advertised gateway self-probe {} at {}",
            if self.wrong_endpoint {
                "reached the wrong endpoint"
            } else {
                "failed"
            },
            self.stage
        )
    }
}

impl std::error::Error for ProbeFault {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// The structured shell shared by every `advertised_gateway` fault, so the timeout path and the
/// error path render identically apart from their stage and cause lines.

/// Before this, the probe's failure reached the operator as `advertised_gateway failed: transport
/// error` -- `tonic::transport::Error`'s `Display` is that literal string, and `error.to_string()`
/// at the boundary discarded everything under it. lost hours to exactly that.
fn advertise_probe_fault(
    advertised: &str,
    stage: &'static str,
    wrong_endpoint: bool,
) -> dexdo_core::DexdoError {
    if wrong_endpoint {
        dexdo_core::DexdoError::new(
            dexdo_core::error_codes::E_ADVERTISE_WRONG_GATEWAY,
            format!(
                "advertised gateway {advertised} answered the pinned-TLS (h2) self-probe, but it \
                 is not this gateway"
            ),
        )
        .with_stage(stage)
        .with_hint(format!(
            "point --gateway-advertise at this gateway's own address, or free {advertised} from \
             the other service; the certificate pin is never relaxed"
        ))
    } else {
        let error = dexdo_core::DexdoError::new(
            dexdo_core::error_codes::E_ADVERTISE_UNREACHABLE,
            format!(
                "advertised gateway {advertised} did not complete the pinned-TLS (h2) self-probe"
            ),
        )
        .with_stage(stage);
        if crate::seller::advertise::advertise_is_public(advertised) {
            error.with_hint(format!(
                "the advertised address must be reachable from THIS host and forward back to this \
                 gateway; verify externally with `curl -k https://{advertised}/`, and note that a \
                 NAT/VPN/reverse-tunnel hairpin can fail this in-process self-probe while a remote \
                 buyer connects fine ()"
            ))
        } else {
            error.with_hint(
                "the advertised address is not public, so the self-probe is authoritative here: \
                 make it reachable from this host, or advertise the address a remote buyer must \
                 dial",
            )
        }
    }
}

/// Probe a decrypted handover endpoint through DNS, TCP, pinned TLS/h2, and gateway identity.
pub async fn probe_gateway(
    endpoint: &str,
    tls_fingerprint: &str,
) -> std::result::Result<(), ProbeFault> {
    let uri: http::Uri = endpoint
        .parse()
        .map_err(|error| ProbeFault::transport("endpoint_parse", error))?;
    if uri.scheme_str() != Some("https") {
        return Err(ProbeFault::transport(
            "endpoint_parse",
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seller gateway endpoint must use https",
            ),
        ));
    }
    let host = uri.host().ok_or_else(|| {
        ProbeFault::transport(
            "endpoint_parse",
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seller gateway endpoint has no host",
            ),
        )
    })?;
    let port = uri.port_u16().unwrap_or(443);

    // Stage 1 -- resolve separately from TCP so a bad name is not collapsed into the same
    // `tcp_connect` bucket as a closed port.
    let resolved = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| ProbeFault::transport("dns_resolve", error))?
        .collect::<Vec<_>>();
    if resolved.is_empty() {
        return Err(ProbeFault::transport(
            "dns_resolve",
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "seller gateway name resolved to no addresses",
            ),
        ));
    }

    // Stage 2 -- plain TCP reachability, so "refused/unroutable" is never reported as an opaque
    // `transport error` from the TLS/h2 stack above it.
    if let Err(error) = tokio::net::TcpStream::connect(resolved.as_slice()).await {
        return Err(ProbeFault::transport("tcp_connect", error));
    }
    // Stage 3 -- pinned TLS + h2. Pinning is NOT relaxed: a fingerprint mismatch is a wrong-endpoint
    // proof and stays fatal.
    let channel = match crate::buyer::tls::connect_pinned(endpoint, tls_fingerprint).await {
        Ok(channel) => channel,
        Err(error) => {
            // The same typed pin-mismatch check the buyer dial uses, and the same typed stage:
            // one definition each, so the two sides of the same connection can never disagree
            // about what a wrong endpoint is or about which step of the dial failed. The stage
            // used to be inferred here from "is there an `io::Error` anywhere in the chain",
            // which cannot tell a refused TCP connect from a failed TLS handshake and called
            // both `tls_handshake`; `connect_pinned` tags the step it was actually on
            // (`DialStageError`), which is the whole point of's staging.
            let wrong_endpoint = crate::buyer::tls::dial_reached_wrong_endpoint(&error);
            return Err(if wrong_endpoint {
                ProbeFault::wrong_endpoint("tls_certificate_pin", error)
            } else {
                let stage = crate::buyer::tls::dial_stage(&error);
                ProbeFault::transport(stage, error)
            });
        }
    };
    // Stage 4 -- the gateway's own gRPC surface. A non-gRPC HTTP response is preserved in the
    // tonic status (including its mapped HTTP status code), while a valid but foreign gRPC service
    // is still classified as the wrong endpoint at this same `grpc_challenge` boundary.
    let mut client = GatewayClient::new(channel);
    let challenge = match client
        .get_challenge(ChallengeRequest {
            token_contract: HEALTH_CHALLENGE_TC.to_string(),
        })
        .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => {
            // A server-returned gRPC status proves the connection completed. No application status
            // is a transport failure that a NAT/VPN/reverse-tunnel hairpin can explain away.
            return Err(ProbeFault::wrong_endpoint("grpc_challenge", status));
        }
    };
    if challenge.token_contract != HEALTH_CHALLENGE_TC || challenge.nonce.len() != 32 {
        return Err(ProbeFault::wrong_endpoint(
            "challenge_response",
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "gateway readiness challenge returned an invalid response",
            ),
        ));
    }
    Ok(())
}

/// The same probe bounded by the caller's canonical health-check duration.
pub async fn probe_gateway_with_timeout(
    endpoint: &str,
    tls_fingerprint: &str,
    timeout: Duration,
) -> std::result::Result<(), ProbeFault> {
    match tokio::time::timeout(timeout, probe_gateway(endpoint, tls_fingerprint)).await {
        Ok(result) => result,
        Err(_) => Err(ProbeFault::transport(
            "handshake_timeout",
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "bounded gateway probe expired",
            ),
        )),
    }
}

async fn probe_advertised_gateway(
    seller: &RunningSeller,
    advertised: &str,
) -> std::result::Result<(), ProbeFault> {
    probe_gateway(&format!("https://{advertised}"), &seller.tls_fingerprint).await
}

pub async fn check_readiness(
    seller: &RunningSeller,
    advertised: &str,
    timeout: Duration,
    identity: Option<&RestingOfferIdentity>,
    token_contract: &str,
    advertise_probe: AdvertiseProbePolicy,
) -> std::result::Result<(), HealthFailure> {
    check_readiness_with_probe(
        seller,
        advertised,
        timeout,
        identity,
        token_contract,
        advertise_probe,
        probe_advertised_gateway(seller, advertised),
    )
    .await
}

async fn check_startup_readiness(
    seller: &RunningSeller,
    advertised: &str,
    timeout: Duration,
    identity: Option<&RestingOfferIdentity>,
    token_contract: &str,
    _advertise_probe: AdvertiseProbePolicy,
) -> std::result::Result<(), HealthFailure> {
    let upstream = seller.state.upstream(token_contract);
    let upstream_timeout_detail = upstream.startup_capability_timeout_detail();
    check_readiness_with_probes(
        ReadinessTarget {
            seller,
            advertised,
            identity,
            token_contract,
        },
        timeout,
        probe_advertised_gateway(seller, advertised),
        upstream.check_startup_market_readiness(),
        upstream_timeout_detail.as_deref(),
    )
    .await
}

async fn check_readiness_with_probe(
    seller: &RunningSeller,
    advertised: &str,
    timeout: Duration,
    identity: Option<&RestingOfferIdentity>,
    token_contract: &str,
    // no longer consulted. Deleting the degrade arm made every self-probe failure fatal, so
    // both `AdvertiseProbePolicy` variants now behave identically and `--require-advertise-probe`
    // selects between two identical behaviours. The plumbing is kept rather than ripped out because
    // removing the type would edit tests; retiring the flag is a CLI-surface decision.
    _advertise_probe: AdvertiseProbePolicy,
    probe: impl Future<Output = std::result::Result<(), ProbeFault>>,
) -> std::result::Result<(), HealthFailure> {
    let upstream = seller.state.upstream(token_contract);
    check_readiness_with_probes(
        ReadinessTarget {
            seller,
            advertised,
            identity,
            token_contract,
        },
        timeout,
        probe,
        upstream.check_market_readiness(),
        None,
    )
    .await
}

struct ReadinessTarget<'a> {
    seller: &'a RunningSeller,
    advertised: &'a str,
    identity: Option<&'a RestingOfferIdentity>,
    token_contract: &'a str,
}

async fn check_readiness_with_probes(
    target: ReadinessTarget<'_>,
    timeout: Duration,
    probe: impl Future<Output = std::result::Result<(), ProbeFault>>,
    upstream_probe: impl Future<Output = Result<()>>,
    upstream_timeout_detail: Option<&str>,
) -> std::result::Result<(), HealthFailure> {
    let ReadinessTarget {
        seller,
        advertised,
        identity,
        token_contract,
    } = target;
    let deadline = tokio::time::Instant::now() + timeout;
    if seller.server_task.is_finished() {
        trace_health(
            identity,
            token_contract,
            HealthComponent::GatewayTask,
            "fail",
        );
        return Err(HealthFailure::new(
            HealthComponent::GatewayTask,
            false,
            "gateway server task stopped",
        ));
    }
    trace_health(
        identity,
        token_contract,
        HealthComponent::GatewayTask,
        "pass",
    );

    // Both readiness components share the canonical per-cycle deadline. Poll them concurrently so
    // a tolerated stalled self-probe cannot starve an already-healthy exact-model check.

    // E2E-ADV-02: readiness asks a strictly LARGER question than provider health -- "may I sell on
    // this market?" -- so this component is `check_market_readiness`, not `check_health`. Provider health is
    // one half of it; the other half is that the model which actually answered is the model this market
    // sells. `check_health` cannot make that call: it is not told which market it is being asked about, and
    // the seller's own outbound slug (which an OpenAI-compatible provider echoes) certified itself there.
    let (probe_result, upstream_result) = tokio::join!(
        tokio::time::timeout_at(deadline, probe),
        tokio::time::timeout_at(deadline, upstream_probe),
    );
    let probe = match probe_result {
        Ok(Ok(())) => None,
        Ok(Err(fault)) => Some((fault, false)),
        Err(_) => Some((
            ProbeFault::transport(
                "handshake_timeout",
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "bounded gateway probe expired",
                ),
            ),
            true,
        )),
    };
    match probe {
        None => {
            trace_health(
                identity,
                token_contract,
                HealthComponent::AdvertisedGateway,
                "pass",
            );
        }
        // a failed self-probe is fatal, with no tolerated arm. The offer is not posted.

        // The arm deleted here degraded a TRANSPORT-level failure against a public advertised
        // address to a warning and posted anyway, on the ground that a NAT/VPN/reverse-tunnel path
        // hairpins back to this process and can fail from the seller host while a remote buyer
        // connects fine. That is true, and it is the wrong trade: the seller cannot tell that case
        // apart from an address no buyer can reach, so tolerating it published offers against dead
        // endpoints. A buyer who matches one pays to reach nothing. If the probe is wrong about a
        // reachable address, that is a defect in the probe to fix, not a reason to publish blind.
        Some((fault, timed_out)) => {
            trace_health(
                identity,
                token_contract,
                HealthComponent::AdvertisedGateway,
                if timed_out { "timeout" } else { "fail" },
            );
            return Err(HealthFailure::new(
                HealthComponent::AdvertisedGateway,
                timed_out,
                format!("pinned-TLS (h2) self-probe of {advertised}"),
            )
            .with_source(fault));
        }
    }

    match upstream_result {
        Ok(Ok(())) => trace_health(
            identity,
            token_contract,
            HealthComponent::UpstreamModel,
            "pass",
        ),
        Ok(Err(error)) => {
            trace_health(
                identity,
                token_contract,
                HealthComponent::UpstreamModel,
                "fail",
            );
            return Err(HealthFailure::new(
                HealthComponent::UpstreamModel,
                false,
                error.to_string(),
            ));
        }
        Err(_) => {
            trace_health(
                identity,
                token_contract,
                HealthComponent::UpstreamModel,
                "timeout",
            );
            return Err(HealthFailure::new(
                HealthComponent::UpstreamModel,
                true,
                upstream_timeout_detail.unwrap_or("bounded upstream model probe expired"),
            ));
        }
    }

    if seller.server_task.is_finished() {
        trace_health(
            identity,
            token_contract,
            HealthComponent::GatewayTask,
            "fail",
        );
        return Err(HealthFailure::new(
            HealthComponent::GatewayTask,
            false,
            "gateway server task stopped during readiness",
        ));
    }
    Ok(())
}

enum TargetState {
    Present,
    Absent,
    Matched(Match),
    /// the exact order is still in the book but past its own deadline.
    Expired(RestingOfferExpiry),
}

async fn target_state(
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    identity: &RestingOfferIdentity,
) -> Result<TargetState> {
    // Sample before the awaited read: crossing the deadline in transit can only keep an offer until
    // the next poll (the matcher already skips it), while sampling after could retire an offer that
    // was live at observation merely because the network read was slow.
    let observed_at = unix_timestamp();
    let orders = chain
        .raw_resting_sell_orders_for_tc(&identity.token_contract)
        .await?;
    if let Some(order) = orders
        .iter()
        .find(|order| order.order_id == identity.order_id)
    {
        validate_resting_offer(order, Some(&identity.owner_note), cfg)?;
        // present in the book is not the same as usable. A row past its own deadline is skipped
        // by the matcher (`_isExpired`, `contracts/airegistry/InferenceOrderBook.sol:1115-1117`), so
        // resuming onto it would report readiness for an offer no buyer can reach.
        if !dexdo_core::order_deadline_is_live(order.is_buy, order.deadline, observed_at) {
            return Ok(TargetState::Expired(RestingOfferExpiry {
                deadline: order.deadline,
                observed_at,
            }));
        }
        return Ok(TargetState::Present);
    }
    if let Some(matched) = chain
        .read_openable_match_now(&identity.token_contract)
        .await?
    {
        return Ok(TargetState::Matched(matched));
    }
    Ok(TargetState::Absent)
}

/// One authoritative look at the supervised order.
enum SupervisedOfferPoll {
    /// Still in the book, with a deadline that has not passed.
    Live { deadline: u64 },
    /// Terminal: past its own deadline in the book, or already removed from it without a fill.
    Expired(RestingOfferExpiry),
    /// The deal is matched. Not this branch's business -- the match path owns it.
    Matched,
}

/// Is the exact supervised order past its own authoritative deadline right now?

/// The deadline is re-read from the book on every call rather than cached at post time, so a wall-clock
/// jump, a delayed task wake or a stale local estimate cannot extend on-chain order validity: whatever
/// happened while this task was asleep, the answer comes from the same read that proves the order is
/// still there.

/// an order that has left the book WITHOUT a fill is the same fact arriving early. Expiry removal
/// is permissionless, so a matcher crossing the book or another keeper may sweep this seller's ask
/// seconds after its deadline and before this poll ever sees it expired
/// (`contracts/airegistry/InferenceOrderBook.sol:1015-1019,1679-1691`). The removal counts as an expiry
/// only once the deal proves it was not a fill -- an unfunded deal has sold nothing, and a fill and its
/// funding land in the same match (`contracts/airegistry/InferenceOrderBook.sol:1082-1092`). Without
/// this, a swept ask left the seller supervising an order that no longer exists, healthy and
/// unreachable, which is the very state exists to end.
async fn resting_offer_expiry(
    chain: &dyn ChainBackend,
    identity: &RestingOfferIdentity,
    last_observed_deadline: Option<u64>,
) -> Result<SupervisedOfferPoll> {
    // Sample before the awaited read: crossing the deadline in transit can only keep an offer until
    // the next poll (the matcher already skips it), while sampling after could retire an offer that
    // was live at observation merely because the network read was slow.
    let observed_at = unix_timestamp();
    let orders = chain
        .raw_resting_sell_orders_for_tc(&identity.token_contract)
        .await?;
    let Some(order) = orders
        .iter()
        .find(|order| order.order_id == identity.order_id)
    else {
        if chain
            .read_openable_match_now(&identity.token_contract)
            .await?
            .is_some()
        {
            return Ok(SupervisedOfferPoll::Matched);
        }
        return Ok(SupervisedOfferPoll::Expired(RestingOfferExpiry {
            // The book's own figure, from while it still held the row. A zero says the row was gone
            // before this supervision read it even once: the removal of an unfunded deal's ask is the
            // authoritative fact, and no locally reconstructed `post time + TTL` stands in for it.
            deadline: last_observed_deadline.unwrap_or(0),
            observed_at,
        }));
    };
    if dexdo_core::order_deadline_is_live(order.is_buy, order.deadline, observed_at) {
        return Ok(SupervisedOfferPoll::Live {
            deadline: order.deadline,
        });
    }
    Ok(SupervisedOfferPoll::Expired(RestingOfferExpiry {
        deadline: order.deadline,
        observed_at,
    }))
}

/// The terminal expiry line an operator and a log scraper both read.

/// It carries the order id, the TokenContract, the absolute deadline and the observed time, so the
/// outcome can be checked against the chain without trusting the process that emitted it.
fn trace_offer_expired(identity: &RestingOfferIdentity, expiry: &RestingOfferExpiry) {
    tracing::info!(
        event = "seller_offer_outcome",
        timestamp = expiry.observed_at,
        owner_note = %display_dexdo_address(&identity.owner_note),
        token_contract = %display_token_contract(&identity.token_contract),
        order_id = identity.order_id,
        deadline = expiry.deadline,
        observed_at = expiry.observed_at,
        disposition = "expired",
        "resting SELL reached its authoritative deadline and is no longer executable"
    );
}

fn trace_offer_expiry_read_failure(
    identity: &RestingOfferIdentity,
    error: impl std::fmt::Display,
    consecutive_failures: u64,
    elapsed_since_last_successful_read: Duration,
    attempt_total: u64,
) {
    let elapsed_since_last_successful_read_ms =
        u64::try_from(elapsed_since_last_successful_read.as_millis()).unwrap_or(u64::MAX);
    if consecutive_failures == 1 {
        tracing::warn!(
            event = "seller_offer_expiry_read_failed",
            timestamp = unix_timestamp(),
            owner_note = %display_dexdo_address(&identity.owner_note),
            token_contract = %display_token_contract(&identity.token_contract),
            order_id = identity.order_id,
            error = %error,
            consecutive_failures,
            elapsed_since_last_successful_read_ms,
            attempt_total,
            "authoritative deadline re-read failed; the seller still owns the offer but its current expiry is unverified"
        );
    } else {
        tracing::error!(
            event = "seller_offer_expiry_read_blind",
            timestamp = unix_timestamp(),
            owner_note = %display_dexdo_address(&identity.owner_note),
            token_contract = %display_token_contract(&identity.token_contract),
            order_id = identity.order_id,
            error = %error,
            consecutive_failures,
            elapsed_since_last_successful_read_ms,
            attempt_total,
            "consecutive authoritative deadline re-reads failed; the seller still owns the offer but its current expiry is unverified"
        );
    }
}

/// What the seller proved about one expired generation before deciding whether to relist.
#[derive(Debug)]
enum RelistDecision {
    /// The exact order is off the book, the deal's offer latch is released, and the deal still holds
    /// this much unsold capacity at its own constructor-bound price.
    Relist {
        remaining_ticks: u64,
        price_per_tick: u64,
    },
    /// A match owns this deal after all. The expiry never was the terminal outcome, so the deal is
    /// handed back to the match path rather than reaped and relisted.
    Matched(Match),
    /// Authoritatively not this seller's to relist. Deterministic: retrying cannot change it.
    Refused { reason: String },
    /// Neither provable nor disprovable inside the budget. Fail closed: an unconfirmed expiry or an
    /// unread latch is exactly the state in which posting again risks a second live offer.
    Unproven { known_result: String },
}

/// Reap the exact expired ask and prove the deal may carry a successor.

/// Submits the permissionless `expireOrder(orderId)` once, then confirms the authoritative
/// consequences rather than the submit: the exact order absent from the book, no OTHER live SELL for
/// the deal, the `_offerPosted` latch released by `onSellClosed`
/// (`contracts/airegistry/TokenContract.sol:729-736`), the deal still unsold, and its
/// constructor-bound capacity readable.

/// The submit's own result is deliberately not authority in either direction. `expireOrder` is
/// permissionless and idempotent -- a gone or still-live order is a silent no-op
/// (`contracts/airegistry/InferenceOrderBook.sol:1679-1691`) -- so a matcher, another keeper or a
/// second seller process may have already done the work, and a submit that failed may still have
/// landed. Both reconcile through the same read-back, which is why this function has no separate
/// "somebody else expired it" branch.
async fn reap_expired_offer(
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    identity: &RestingOfferIdentity,
    deadline: tokio::time::Instant,
    poll_interval: Duration,
) -> RelistDecision {
    // Reconcile BEFORE writing. A matcher, another keeper or a previous run of this seller may have
    // already reaped the order, and in that case there is nothing to submit -- the successor's
    // precondition is simply already true. This is also why "somebody else expired it first" needs no
    // branch of its own: it is the ordinary first pass, minus the submit.
    let mut expiry_submit = "not_needed".to_string();
    let mut submitted = false;
    let mut last;
    loop {
        // Every read is inside the budget, not merely checked against it afterwards: a chain that
        // never answers must not hold the seller in a cleanup it can neither finish nor abandon.
        match tokio::time::timeout_at(deadline, reap_state(chain, cfg, identity)).await {
            Ok(Ok(Ok(decision))) => return decision,
            Ok(Ok(Err(pending))) => last = format!("expiry_submit={expiry_submit}; {pending}"),
            Ok(Err(error)) => last = format!("expiry_submit={expiry_submit}; read_failed: {error}"),
            Err(_) => last = format!("expiry_submit={expiry_submit}; read_failed: timeout"),
        }
        if !submitted {
            submitted = true;
            expiry_submit = match tokio::time::timeout_at(
                deadline,
                chain.expire_resting_sell_order(&identity.token_contract, identity.order_id),
            )
            .await
            {
                Ok(Ok(())) => "submitted".to_string(),
                Ok(Err(error)) => format!("failed: {error}"),
                Err(_) => "timeout".to_string(),
            };
            tracing::info!(
                event = "seller_offer_reap",
                timestamp = unix_timestamp(),
                owner_note = %display_dexdo_address(&identity.owner_note),
                token_contract = %display_token_contract(&identity.token_contract),
                order_id = identity.order_id,
                expiry_submit = %expiry_submit,
                "permissionless expiry submitted for the seller's own expired ask"
            );
            last = format!("expiry_submit={expiry_submit}; awaiting authoritative removal");
        }
        if tokio::time::Instant::now() >= deadline {
            return RelistDecision::Unproven {
                known_result: format!(
                    "{last}; operator_action=run `dexdo orders list` with the same `--note-addr` \
                     and `--market` or `--model` this seller was started with, and confirm \
                     TokenContract {} carries no live SELL before restarting the seller",
                    display_token_contract(&identity.token_contract)
                ),
            };
        }
        let wake_at = std::cmp::min(tokio::time::Instant::now() + poll_interval, deadline);
        tokio::time::sleep_until(wake_at).await;
    }
}

/// One authoritative pass of the reap gate.

/// `Ok(Ok(decision))` is terminal, `Ok(Err(pending))` describes a state that a later pass may still
/// resolve, and `Err` is a failed read. Only the middle one is worth polling for: everything else is
/// either proven or deterministic.
#[allow(clippy::type_complexity)]
async fn reap_state(
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    identity: &RestingOfferIdentity,
) -> Result<std::result::Result<RelistDecision, String>> {
    // A match that landed before the deadline outranks the expiry: the deal is sold, and serving it
    // is worth more than any successor. The book cannot create a NEW one -- `_match` skips a row past
    // its deadline (`contracts/airegistry/InferenceOrderBook.sol:1115-1117`) -- so this is the earlier
    // fill this seller had not observed yet, not a fill against the expired order.
    if let Some(matched) = chain
        .read_openable_match_now(&identity.token_contract)
        .await?
    {
        return Ok(Ok(RelistDecision::Matched(matched)));
    }

    let orders = chain
        .raw_resting_sell_orders_for_tc(&identity.token_contract)
        .await?;
    if let Some(other) = orders
        .iter()
        .find(|order| order.order_id != identity.order_id)
    {
        return Ok(Ok(RelistDecision::Refused {
            reason: format!(
                "TokenContract {} already carries live SELL {} besides the expired {}; refusing to \
                 create a second offer",
                display_token_contract(&identity.token_contract), other.order_id, identity.order_id
            ),
        }));
    }
    if !orders.is_empty() {
        return Ok(Err(format!(
            "authoritative_state=expired_order_{}_still_in_book",
            identity.order_id
        )));
    }

    let Some(latch) = chain
        .token_contract_offer_latch(&identity.token_contract)
        .await?
    else {
        return Ok(Ok(RelistDecision::Refused {
            reason: format!(
                "TokenContract {} offer latch is unreadable on this backend; a successor \
                 `postFromNote` would be dropped without a trace if `_offerPosted` were still set",
                display_token_contract(&identity.token_contract)
            ),
        }));
    };
    // Contracts 4.0.35 deleted `_closing`: a wind-down intent is no longer representable, because
    // `close()` refuses outright while an offer is live instead of latching and reporting
    // success. `offerPosted` is the whole latch, and it is the only question this decision asked of
    // the second flag anyway -- a closing TC always had its offer posted.
    if latch.offer_posted {
        return Ok(Err(
            "authoritative_state=offer_latch_still_posted".to_string()
        ));
    }

    let Some(state) = chain.deal_state(&identity.token_contract).await? else {
        return Ok(Err("authoritative_state=deal_state_absent".to_string()));
    };
    if let Some(reason) = state.used_reason() {
        return Ok(Ok(RelistDecision::Refused {
            reason: format!(
                "TokenContract {} is already used ({reason}); its capacity belongs to that deal",
                display_token_contract(&identity.token_contract)
            ),
        }));
    }

    let Some((price_per_tick, remaining_ticks)) =
        chain.sell_offer_terms(&identity.token_contract).await?
    else {
        return Ok(Ok(RelistDecision::Refused {
            reason: format!(
                "TokenContract {} has no readable getDeal terms; the successor's size would be a \
                 guess rather than authoritative remaining capacity",
                display_token_contract(&identity.token_contract)
            ),
        }));
    };
    // Below the contract's own minimum a successor could never become a deal: `_match` refuses a
    // trade under two ticks because `fundFromOrderBook` rejects a sub-2 fund
    // (`contracts/airegistry/InferenceOrderBook.sol:1051`).
    if u128::from(remaining_ticks) < dexdo_core::MIN_STREAM_BUY_TICKS {
        return Ok(Ok(RelistDecision::Refused {
            reason: format!(
                "TokenContract {} has {remaining_ticks} tick(s) of remaining capacity, below the \
                 contract's {} minimum fill",
                display_token_contract(&identity.token_contract),
                dexdo_core::MIN_STREAM_BUY_TICKS
            ),
        }));
    }
    if (price_per_tick, remaining_ticks) != (cfg.price_per_tick, cfg.max_ticks) {
        tracing::warn!(
            event = "seller_offer_reap",
            timestamp = unix_timestamp(),
            token_contract = %display_token_contract(&identity.token_contract),
            order_id = identity.order_id,
            configured_price_per_tick = cfg.price_per_tick,
            configured_max_ticks = cfg.max_ticks,
            price_per_tick,
            remaining_ticks,
            "successor sized from the deal's authoritative terms, not from the expired offer"
        );
    }
    Ok(Ok(RelistDecision::Relist {
        remaining_ticks,
        price_per_tick,
    }))
}

/// The line that correlates a reaped generation with its successor.
fn trace_offer_relisted(
    identity: &RestingOfferIdentity,
    expired: &RestingOfferExpiry,
    successor_order_id: u128,
    successor_deadline: u64,
    remaining_ticks: u64,
) {
    tracing::info!(
        event = "seller_offer_relisted",
        timestamp = unix_timestamp(),
        owner_note = %display_dexdo_address(&identity.owner_note),
        token_contract = %display_token_contract(&identity.token_contract),
        previous_order_id = identity.order_id,
        previous_deadline = expired.deadline,
        order_id = successor_order_id,
        deadline = successor_deadline,
        remaining_ticks,
        disposition = "relisted",
        "expired ask reaped and exactly one successor accepted for the remaining capacity"
    );
}

/// The seller cannot print a runnable `orders` line here: `orders` needs this note's identity and
/// either `--market` or `--model`, and cancelling needs `--note-key` to sign, none of which this
/// module holds. So it names the command and states the inputs the operator must supply,
/// rather than printing a line that looks runnable and is not.
fn manual_cancel_action(order_id: u128) -> String {
    format!(
        "cancel resting order {order_id} by hand: run `dexdo orders cancel` with the same \
         `--note-addr` and `--market` or `--model` this seller was started with, plus `--note-key` \
         to sign, then verify the book"
    )
}

/// The same, where the order id is not known yet because the submit never resolved.
fn manual_cancel_action_for_unknown_order() -> String {
    "cancel the exact resting order by hand: run `dexdo orders cancel` with its order id, the same \
     `--note-addr` and `--market` or `--model` this seller was started with, plus `--note-key` to \
     sign, then verify the book"
        .to_string()
}

fn unknown_cancellation(
    identity: &RestingOfferIdentity,
    cycle_timeout: Duration,
    known_result: impl Into<String>,
) -> CancellationDisposition {
    let known_result = format!(
        "{}; budget_ms={}; operator_action={}",
        known_result.into(),
        cycle_timeout.as_millis(),
        manual_cancel_action(identity.order_id)
    );
    tracing::error!(
        event = "seller_cancel_terminal",
        timestamp = unix_timestamp(),
        owner_note = %display_dexdo_address(&identity.owner_note),
        token_contract = %display_token_contract(&identity.token_contract),
        order_id = identity.order_id,
        disposition = "unknown_failure",
        known_result = %known_result,
        "exact resting SELL cancellation has no terminal authoritative fact"
    );
    CancellationDisposition::UnknownFailure { known_result }
}

fn rejected_cancel_still_resting(
    identity: &RestingOfferIdentity,
    authoritative_result: &str,
    reason: u8,
) -> CancellationDisposition {
    let known_result = format!(
        "cancel_submit=accepted; {authoritative_result}; terminal_chain_fact=\
         InferenceOrderCancelRejected(order_id={}, reason={reason}, owner_note={}); \
         operator_action={}",
        identity.order_id,
        display_dexdo_address(&identity.owner_note),
        manual_cancel_action(identity.order_id)
    );
    tracing::error!(
        event = "seller_cancel_terminal",
        timestamp = unix_timestamp(),
        owner_note = %display_dexdo_address(&identity.owner_note),
        token_contract = %display_token_contract(&identity.token_contract),
        order_id = identity.order_id,
        disposition = "rejected_still_resting",
        cancel_rejection_reason = reason,
        known_result = %known_result,
        "the chain terminally rejected the accepted cancel request and the exact SELL still rests"
    );
    CancellationDisposition::RejectedStillResting { known_result }
}

/// Await one in-flight chain read without cancelling or duplicating it, while keeping the accepted
/// cancel watch visibly alive at the canonical reconciliation cadence.
async fn await_visible_cancel_watch<F, T>(
    identity: &RestingOfferIdentity,
    poll_interval: Duration,
    read: &'static str,
    future: F,
) -> T
where
    F: Future<Output = T>,
{
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = tokio::time::sleep(poll_interval) => {
                tracing::info!(
                    event = "seller_cancel_watch",
                    timestamp = unix_timestamp(),
                    owner_note = %display_dexdo_address(&identity.owner_note),
                    token_contract = %display_token_contract(&identity.token_contract),
                    order_id = identity.order_id,
                    cancel_submit = "accepted",
                    read,
                    status = "waiting",
                    "accepted cancellation is still watching for an authoritative chain fact"
                );
            }
        }
    }
}

async fn watch_accepted_cancel(
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    identity: &RestingOfferIdentity,
    watch: &RestingSellCancelWatch,
    poll_interval: Duration,
) -> CancellationDisposition {
    loop {
        let state = await_visible_cancel_watch(
            identity,
            poll_interval,
            "exact_order_state",
            target_state(chain, cfg, identity),
        )
        .await;
        let authoritative_result = match state {
            Ok(TargetState::Absent) => {
                tracing::info!(
                    event = "seller_cancel_terminal",
                    timestamp = unix_timestamp(),
                    owner_note = %display_dexdo_address(&identity.owner_note),
                    token_contract = %display_token_contract(&identity.token_contract),
                    order_id = identity.order_id,
                    disposition = "cancelled",
                    submit_result = "cancel_submit=accepted",
                    "accepted cancellation reached authoritative exact-order absence"
                );
                return CancellationDisposition::Cancelled;
            }
            Ok(TargetState::Matched(matched)) => {
                tracing::info!(
                    event = "seller_cancel_terminal",
                    timestamp = unix_timestamp(),
                    owner_note = %display_dexdo_address(&identity.owner_note),
                    token_contract = %display_token_contract(&identity.token_contract),
                    order_id = identity.order_id,
                    disposition = "already_matched",
                    submit_result = "cancel_submit=accepted",
                    "match won the cancel race; no other order was touched"
                );
                return CancellationDisposition::AlreadyMatched(matched);
            }
            Ok(TargetState::Present) => "authoritative_state=present".to_string(),
            Ok(TargetState::Expired(expired)) => format!(
                "authoritative_state=present_expired deadline={} observed_at={}",
                expired.deadline, expired.observed_at
            ),
            Err(error) => {
                tracing::warn!(
                    event = "seller_cancel_watch",
                    timestamp = unix_timestamp(),
                    owner_note = %display_dexdo_address(&identity.owner_note),
                    token_contract = %display_token_contract(&identity.token_contract),
                    order_id = identity.order_id,
                    cancel_submit = "accepted",
                    status = "authoritative_read_failed",
                    error = %error,
                    "accepted cancellation remains under authoritative watch"
                );
                tokio::time::sleep(poll_interval).await;
                continue;
            }
        };

        let rejection = await_visible_cancel_watch(
            identity,
            poll_interval,
            "cancel_terminal_event",
            chain.resting_sell_cancel_rejection_after(
                &identity.token_contract,
                identity.order_id,
                &identity.owner_note,
                watch,
            ),
        )
        .await;
        match rejection {
            Ok(Some(reason)) => {
                return rejected_cancel_still_resting(identity, &authoritative_result, reason);
            }
            Ok(None) => tracing::info!(
                event = "seller_cancel_watch",
                timestamp = unix_timestamp(),
                owner_note = %display_dexdo_address(&identity.owner_note),
                token_contract = %display_token_contract(&identity.token_contract),
                order_id = identity.order_id,
                cancel_submit = "accepted",
                authoritative_result = %authoritative_result,
                status = "watching",
                "accepted cancellation has no terminal chain fact yet"
            ),
            Err(error) => tracing::warn!(
                event = "seller_cancel_watch",
                timestamp = unix_timestamp(),
                owner_note = %display_dexdo_address(&identity.owner_note),
                token_contract = %display_token_contract(&identity.token_contract),
                order_id = identity.order_id,
                cancel_submit = "accepted",
                authoritative_result = %authoritative_result,
                status = "terminal_event_read_failed",
                error = %error,
                "accepted cancellation remains under authoritative watch"
            ),
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// where the one cycle deadline is USED, so that "one deadline, read twice" can be asserted
/// as itself.

/// A supervision cycle computes its deadline once, bounds the readiness check with it, and hands
/// the SAME instant to the cancellation. Nothing about that is visible from outside the function:
/// both a shared deadline and two independent ones end in the same `unknown_failure` carrying the
/// same `budget_ms`, and the only externally different thing about two deadlines is that the cycle
/// takes about twice as long. That is why this property used to be asserted with a stopwatch, and
/// why the stopwatch kept failing under scheduling pressure while the property itself held.

/// This is observation and nothing else. Both the recorder and its call sites are `#[cfg(test)]`,
/// so a non-test build does not contain them, and in either build the deadline is computed at
/// exactly the same point from exactly the same inputs.

/// Thread-local rather than global: libtest gives each test its own thread and these tests drive
/// `supervise_with_timing` on it directly, so recordings cannot bleed between tests running
/// concurrently in the same binary -- the very condition that made the old stopwatch flaky.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CycleDeadlineSite {
    HealthCheck,
    Cancel,
}

#[cfg(test)]
thread_local! {
    static OBSERVED_CYCLE_DEADLINES: std::cell::RefCell<
        Vec<(CycleDeadlineSite, tokio::time::Instant)>,
    > = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn observe_cycle_deadline(site: CycleDeadlineSite, deadline: tokio::time::Instant) {
    OBSERVED_CYCLE_DEADLINES.with(|observed| observed.borrow_mut().push((site, deadline)));
}

/// Drain what this thread observed. Draining rather than reading keeps a test that runs several
/// cycles from inheriting an earlier one's recordings.
#[cfg(test)]
fn take_observed_cycle_deadlines() -> Vec<(CycleDeadlineSite, tokio::time::Instant)> {
    OBSERVED_CYCLE_DEADLINES.with(|observed| std::mem::take(&mut *observed.borrow_mut()))
}

async fn cancel_and_confirm_before(
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    identity: &RestingOfferIdentity,
    deadline: tokio::time::Instant,
    cycle_timeout: Duration,
    poll_interval: Duration,
) -> CancellationDisposition {
    #[cfg(test)]
    observe_cycle_deadline(CycleDeadlineSite::Cancel, deadline);
    loop {
        match tokio::time::timeout_at(deadline, target_state(chain, cfg, identity)).await {
            Err(_) => {
                return unknown_cancellation(
                    identity,
                    cycle_timeout,
                    "initial_authoritative_read=timeout",
                );
            }
            Ok(Err(error)) => {
                return unknown_cancellation(
                    identity,
                    cycle_timeout,
                    format!("initial_authoritative_read=failed: {error}"),
                );
            }
            // `Expired` is the same fact as `Present` for a cancellation - the row is still in
            // the book, and `_doCancel` has no expiry guard, so the owner can still remove it and free
            // the TokenContract's offer latch. Only a supervision outcome distinguishes the two.
            Ok(Ok(TargetState::Present | TargetState::Expired(_))) => break,
            Ok(Ok(TargetState::Matched(matched))) => {
                tracing::info!(
                    event = "seller_cancel_terminal",
                    timestamp = unix_timestamp(),
                    owner_note = %display_dexdo_address(&identity.owner_note),
                    token_contract = %display_token_contract(&identity.token_contract),
                    order_id = identity.order_id,
                    disposition = "already_matched",
                    "match won before cancellation submit; no order was touched"
                );
                return CancellationDisposition::AlreadyMatched(matched);
            }
            Ok(Ok(TargetState::Absent)) if tokio::time::Instant::now() < deadline => {
                let wake_at = std::cmp::min(tokio::time::Instant::now() + poll_interval, deadline);
                tokio::time::sleep_until(wake_at).await;
                if tokio::time::Instant::now() < deadline {
                    continue;
                }
            }
            Ok(Ok(TargetState::Absent)) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::info!(
                event = "seller_cancel_terminal",
                timestamp = unix_timestamp(),
                owner_note = %display_dexdo_address(&identity.owner_note),
                token_contract = %display_token_contract(&identity.token_contract),
                order_id = identity.order_id,
                disposition = "already_absent",
                "resting SELL was already authoritatively absent"
            );
            return CancellationDisposition::AlreadyAbsent;
        }
    }

    tracing::info!(
        event = "seller_cancel_submit",
        timestamp = unix_timestamp(),
        owner_note = %display_dexdo_address(&identity.owner_note),
        token_contract = %display_token_contract(&identity.token_contract),
        order_id = identity.order_id,
        "submitting exact resting SELL cancellation"
    );
    let submit_result = match tokio::time::timeout_at(
        deadline,
        chain.begin_resting_sell_cancel(&identity.token_contract, identity.order_id),
    )
    .await
    {
        Ok(Ok(watch)) => {
            return watch_accepted_cancel(chain, cfg, identity, &watch, poll_interval).await;
        }
        Ok(Err(RestingSellCancelStartError::Preparation(error))) => {
            return unknown_cancellation(
                identity,
                cycle_timeout,
                format!("cancel_preparation=failed: {error}"),
            );
        }
        Ok(Err(RestingSellCancelStartError::Submit(error))) => {
            format!("cancel_submit=rejected: {error}")
        }
        Err(_) => {
            return unknown_cancellation(
                identity,
                cycle_timeout,
                "cancel_preparation_or_submit=timeout",
            );
        }
    };

    let mut authoritative_result = "authoritative_state=present".to_string();
    let mut authoritatively_absent = false;
    loop {
        if tokio::time::Instant::now() >= deadline {
            if authoritatively_absent {
                let disposition = CancellationDisposition::AlreadyAbsent;
                tracing::info!(
                    event = "seller_cancel_terminal",
                    timestamp = unix_timestamp(),
                    owner_note = %display_dexdo_address(&identity.owner_note),
                    token_contract = %display_token_contract(&identity.token_contract),
                    order_id = identity.order_id,
                    disposition = disposition.as_str(),
                    submit_result = %submit_result,
                    "rejected cancel was followed by authoritative exact-order absence"
                );
                return disposition;
            }
            return unknown_cancellation(
                identity,
                cycle_timeout,
                format!("{submit_result}; {authoritative_result}"),
            );
        }
        match tokio::time::timeout_at(deadline, target_state(chain, cfg, identity)).await {
            Ok(Ok(TargetState::Absent)) => {
                authoritatively_absent = true;
                authoritative_result = "authoritative_state=absent".to_string();
            }
            Ok(Ok(TargetState::Matched(matched))) => {
                tracing::info!(
                    event = "seller_cancel_terminal",
                    timestamp = unix_timestamp(),
                    owner_note = %display_dexdo_address(&identity.owner_note),
                    token_contract = %display_token_contract(&identity.token_contract),
                    order_id = identity.order_id,
                    disposition = "already_matched",
                    submit_result = %submit_result,
                    "match won the cancel race; no other order was touched"
                );
                return CancellationDisposition::AlreadyMatched(matched);
            }
            Ok(Ok(TargetState::Present)) => {
                authoritatively_absent = false;
                authoritative_result = "authoritative_state=present".to_string();
            }
            Ok(Ok(TargetState::Expired(expired))) => {
                // Still there, so the cancellation has not landed yet; the deadline is reported
                // rather than swallowed, because it explains why nothing will ever match it.
                authoritatively_absent = false;
                authoritative_result = format!(
                    "authoritative_state=present_expired deadline={} observed_at={}",
                    expired.deadline, expired.observed_at
                );
            }
            Ok(Err(error)) => {
                authoritatively_absent = false;
                authoritative_result = format!("authoritative_read=failed: {error}");
            }
            Err(_) => {
                return unknown_cancellation(
                    identity,
                    cycle_timeout,
                    format!("{submit_result}; authoritative_read=timeout"),
                );
            }
        }

        let wake_at = std::cmp::min(tokio::time::Instant::now() + poll_interval, deadline);
        tokio::time::sleep_until(wake_at).await;
    }
}

async fn cancel_and_confirm_with_timing(
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    identity: &RestingOfferIdentity,
    confirm_timeout: Duration,
    poll_interval: Duration,
) -> CancellationDisposition {
    cancel_and_confirm_before(
        chain,
        cfg,
        identity,
        tokio::time::Instant::now() + confirm_timeout,
        confirm_timeout,
        poll_interval,
    )
    .await
}

pub async fn cancel_and_confirm(
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    identity: &RestingOfferIdentity,
) -> CancellationDisposition {
    let params = SellerLivenessParams::canonical();
    cancel_and_confirm_with_timing(
        chain,
        cfg,
        identity,
        params.cancel_confirmation_timeout,
        params.cancel_confirmation_poll,
    )
    .await
}

async fn wait_for_gateway_task_stop(seller: &RunningSeller) {
    let poll_interval = SellerLivenessParams::canonical().gateway_task_poll;
    while !seller.server_task.is_finished() {
        tokio::time::sleep(poll_interval).await;
    }
}

fn gateway_task_failure(detail: &str) -> HealthFailure {
    HealthFailure::new(HealthComponent::GatewayTask, false, detail)
}

async fn stop_exact_offer(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    identity: &RestingOfferIdentity,
    reason: RestingStopReason,
    deadline: tokio::time::Instant,
    timing: SupervisionTiming,
) -> Result<SellerStartupOutcome> {
    match cancel_and_confirm_before(
        chain,
        cfg,
        identity,
        deadline,
        timing.cycle_timeout,
        timing.cancel_poll,
    )
    .await
    {
        CancellationDisposition::AlreadyMatched(_) => Ok(SellerStartupOutcome::Ready(
            SellerOfferStartup::ResumedFunded,
        )),
        disposition if !disposition.proven_unmatchable() => Ok(SellerStartupOutcome::Stopped {
            identity: Some(identity.clone()),
            reason,
            disposition,
        }),
        disposition => {
            seller.server_task.abort();
            Ok(SellerStartupOutcome::Stopped {
                identity: Some(identity.clone()),
                reason,
                disposition,
            })
        }
    }
}

async fn reconcile_unresolved_startup(
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    expected_owner: &str,
    deadline: tokio::time::Instant,
    poll_interval: Duration,
) -> std::result::Result<SellerOfferInspection, String> {
    let mut known_result = "authoritative_state=vacant".to_string();
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(known_result);
        }
        match tokio::time::timeout_at(
            deadline,
            inspect_seller_offer(chain, cfg, Some(expected_owner)),
        )
        .await
        {
            Ok(Ok(SellerOfferInspection::Vacant)) => {
                known_result = "authoritative_state=vacant".to_string();
            }
            Ok(Ok(inspection)) => return Ok(inspection),
            Ok(Err(error)) => {
                known_result = format!("authoritative_read=failed: {error}");
            }
            Err(_) => return Err("authoritative_read=timeout".to_string()),
        }
        let wake_at = std::cmp::min(tokio::time::Instant::now() + poll_interval, deadline);
        tokio::time::sleep_until(wake_at).await;
    }
}

async fn resolve_interrupted_startup(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    expected_owner: &str,
    startup: Option<SellerOfferStartup>,
    stop: (RestingStopReason, tokio::time::Instant),
    timing: SupervisionTiming,
) -> Result<SellerStartupOutcome> {
    let (reason, deadline) = stop;
    let inspection = match startup {
        Some(SellerOfferStartup::ResumedFunded)
        | Some(SellerOfferStartup::Posted {
            outcome: Some(dexdo_core::SellOfferOutcome::Matched),
        }) => {
            return Ok(SellerStartupOutcome::Ready(
                SellerOfferStartup::ResumedFunded,
            ))
        }
        Some(SellerOfferStartup::ResumedResting { order_id })
        | Some(SellerOfferStartup::Posted {
            outcome: Some(dexdo_core::SellOfferOutcome::Rested { order_id }),
        }) => SellerOfferInspection::Resting { order_id },
        Some(SellerOfferStartup::Posted { outcome: None }) | None => {
            match reconcile_unresolved_startup(
                chain,
                cfg,
                expected_owner,
                deadline,
                timing.cancel_poll,
            )
            .await
            {
                Ok(inspection) => inspection,
                Err(result) => {
                    let known_result = format!(
                        "fresh_sell_submit=unresolved; {result}; budget_ms={}; \
                         operator_action=list this note's resting orders with `dexdo orders list`, \
                         then {}",
                        timing.cycle_timeout.as_millis(),
                        manual_cancel_action_for_unknown_order()
                    );
                    tracing::error!(
                        event = "seller_cancel_terminal",
                        timestamp = unix_timestamp(),
                        owner_note = %display_dexdo_address(expected_owner),
                        token_contract = %display_token_contract(&cfg.token_contract),
                        order_id = Option::<u128>::None,
                        disposition = "unknown_failure",
                        known_result = %known_result,
                        "interrupted fresh SELL has no terminal authoritative fact"
                    );
                    return Ok(SellerStartupOutcome::Stopped {
                        identity: None,
                        reason,
                        disposition: CancellationDisposition::UnknownFailure { known_result },
                    });
                }
            }
        }
    };

    match inspection {
        SellerOfferInspection::Funded => Ok(SellerStartupOutcome::Ready(
            SellerOfferStartup::ResumedFunded,
        )),
        SellerOfferInspection::Resting { order_id } => {
            let identity = RestingOfferIdentity {
                owner_note: expected_owner.to_string(),
                token_contract: cfg.token_contract.clone(),
                order_id,
            };
            stop_exact_offer(seller, chain, cfg, &identity, reason, deadline, timing).await
        }
        SellerOfferInspection::Vacant => {
            seller.server_task.abort();
            Ok(SellerStartupOutcome::Stopped {
                identity: None,
                reason,
                disposition: CancellationDisposition::AlreadyAbsent,
            })
        }
    }
}

/// Startup readiness, with the same bounded tolerance for an isolated stall the periodic path has.

/// The periodic check retries a TIMED-OUT probe once
/// (`SELLER_UPSTREAM_HEALTH_TIMEOUT_MAX_ATTEMPTS`, "two attempts absorb one isolated stall"). Startup
/// had no retry at all, and it is where a stall costs the most: no ask is posted, the seller exits,
/// and the market stays empty until an operator notices. Observed against a provider answering a
/// plain completion in two seconds: `upstream_authentication_and_model timed out: bounded upstream
/// model probe expired`, and the offer was never placed.

/// Only a timeout is retried. A readiness FAILURE -- a dead gateway, a model that is not the model
/// this market sells -- stays fatal on the first answer, as before: it is evidence of unfitness, and
/// asking twice cannot change it.
async fn startup_readiness_with_timeout_retries(
    seller: &RunningSeller,
    cfg: &SellerConfig,
    existing_identity: Option<&RestingOfferIdentity>,
    timing: SupervisionTiming,
) -> std::result::Result<(), HealthFailure> {
    let mut attempt = 1;
    loop {
        let outcome = check_startup_readiness(
            seller,
            &cfg.gateway_advertise,
            timing.health_timeout,
            existing_identity,
            &cfg.token_contract,
            timing.advertise_probe,
        )
        .await;
        match outcome {
            Ok(()) => return Ok(()),
            Err(failure)
                if failure.timed_out && attempt < SELLER_UPSTREAM_HEALTH_TIMEOUT_MAX_ATTEMPTS =>
            {
                tracing::warn!(
                    event = "seller_startup_readiness_timeout_retry",
                    component = failure.component.as_str(),
                    timestamp = unix_timestamp(),
                    token_contract = %cfg.token_contract,
                    failed_attempt = attempt,
                    next_attempt = attempt + 1,
                    max_attempts = SELLER_UPSTREAM_HEALTH_TIMEOUT_MAX_ATTEMPTS,
                    reason = %failure,
                    "timed-out startup readiness probe; retrying before refusing to post the SELL"
                );
                attempt += 1;
            }
            Err(failure) => return Err(failure),
        }
    }
}

async fn prepare_seller_offer_with_timing<S>(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    expected_owner: &str,
    existing_identity: Option<&RestingOfferIdentity>,
    shutdown: S,
    timing: SupervisionTiming,
) -> Result<SellerStartupOutcome>
where
    S: Future<Output = ()>,
{
    // E2E-ADV-14 -- "the note covers the 2P security deposit before the offer is posted". This runs
    // FIRST, ahead of the readiness probe and ahead of any post: a note whose record cannot pay the
    // deal's exact mirror bond must neither emit a readiness success nor rest an ask. It is also the
    // last point where the refusal is unambiguous -- once a post has been attempted the outcome has to
    // be reconciled against the real book, whereas here nothing was submitted and there is nothing to
    // reconcile, so the shortfall reaches the operator as the error itself.
    chain
        .assert_note_covers_seller_bond(&cfg.token_contract)
        .await?;
    let readiness_deadline = tokio::time::Instant::now() + timing.cycle_timeout;
    let readiness = startup_readiness_with_timeout_retries(seller, cfg, existing_identity, timing);
    let gateway_stopped = wait_for_gateway_task_stop(seller);
    tokio::pin!(readiness);
    tokio::pin!(gateway_stopped);
    tokio::pin!(shutdown);

    let readiness_stop = tokio::select! {
        biased;
        _ = &mut shutdown => Some(RestingStopReason::Shutdown),
        _ = &mut gateway_stopped => Some(RestingStopReason::Health(
            gateway_task_failure("gateway server task stopped during startup readiness")
        )),
        result = &mut readiness => result.err().map(RestingStopReason::Health),
    };
    if let Some(reason) = readiness_stop {
        if let Some(identity) = existing_identity {
            return stop_exact_offer(
                seller,
                chain,
                cfg,
                identity,
                reason,
                readiness_deadline,
                timing,
            )
            .await;
        }
        seller.server_task.abort();
        return match reason {
            RestingStopReason::Shutdown => Ok(SellerStartupOutcome::Stopped {
                identity: None,
                reason,
                disposition: CancellationDisposition::AlreadyAbsent,
            }),
            RestingStopReason::Health(failure) => {
                println!(
                    "seller offer NOT placed for TokenContract {}: readiness failed before SELL",
                    display_token_contract(&cfg.token_contract)
                );
                Err(failure.into_startup_error(&cfg.gateway_advertise))
            }
            RestingStopReason::Watcher(_) => unreachable!("no watcher exists before SELL"),
            RestingStopReason::Expired(_) => {
                unreachable!("no supervised order exists before SELL")
            }
        };
    }

    let startup = prepare_seller_offer(seller.note.as_ref(), chain, cfg, Some(expected_owner));
    tokio::pin!(startup);
    let interruption = tokio::select! {
        biased;
        _ = &mut shutdown => Some(RestingStopReason::Shutdown),
        _ = &mut gateway_stopped => Some(RestingStopReason::Health(
            gateway_task_failure("gateway server task stopped during SELL post or confirmation")
        )),
        result = &mut startup => {
            return match result {
                Ok(SellerOfferStartup::Posted { outcome: None }) => {
                    resolve_interrupted_startup(
                        seller,
                        chain,
                        cfg,
                        expected_owner,
                        None,
                        (
                            RestingStopReason::Watcher(
                                "SELL post returned without an exact resting or matched outcome"
                                    .to_string(),
                            ),
                            tokio::time::Instant::now() + timing.cycle_timeout,
                        ),
                        timing,
                    ).await
                }
                Ok(startup @ SellerOfferStartup::Posted {
                    outcome: Some(dexdo_core::SellOfferOutcome::Rested { order_id }),
                }) => {
                    let identity = RestingOfferIdentity {
                        owner_note: expected_owner.to_string(),
                        token_contract: cfg.token_contract.clone(),
                        order_id,
                    };
                    let deadline = tokio::time::Instant::now() + timing.cycle_timeout;
                    let remaining =
                        deadline.saturating_duration_since(tokio::time::Instant::now());
                    let readiness = check_readiness(
                        seller,
                        &cfg.gateway_advertise,
                        std::cmp::min(timing.health_timeout, remaining),
                        Some(&identity),
                        &cfg.token_contract,
                        timing.advertise_probe,
                    );
                    tokio::pin!(readiness);
                    let stop = tokio::select! {
                        biased;
                        _ = &mut shutdown => Some(RestingStopReason::Shutdown),
                        _ = &mut gateway_stopped => Some(RestingStopReason::Health(
                            gateway_task_failure(
                                "gateway server task stopped after fresh SELL became resting"
                            )
                        )),
                        result = &mut readiness => result.err().map(RestingStopReason::Health),
                    };
                    match stop {
                        Some(reason) => stop_exact_offer(
                            seller,
                            chain,
                            cfg,
                            &identity,
                            reason,
                            deadline,
                            timing,
                        ).await,
                        None => Ok(SellerStartupOutcome::Ready(startup)),
                    }
                }
                Ok(startup) => Ok(SellerStartupOutcome::Ready(startup)),
                Err(error) => {
                    // A post/confirmation error is not proof that the fresh
                    // write failed.  In particular, cancelling a row which
                    // becomes visible during this error path turns a
                    // transport timeout into an unauthorized lifecycle
                    // action.  Leave it fail-closed for the durable marker
                    // reconciler instead; it is the only path allowed to
                    // adopt a later exact-TC fact or permit a future retry.
                    let reason = RestingStopReason::Watcher(format!(
                        "publication_unconfirmed: seller offer post or confirmation failed: {error}"
                    ));
                    Ok(SellerStartupOutcome::Stopped {
                        identity: None,
                        reason,
                        disposition: CancellationDisposition::UnknownFailure {
                            known_result: format!(
                                "publication_unconfirmed; no cancellation or repost was submitted after seller post/confirmation error: {error}"
                            ),
                        },
                    })
                }
            };
        }
    };

    let reason = interruption.expect("startup select only returns after an interruption");
    let deadline = tokio::time::Instant::now() + timing.cycle_timeout;
    let startup_deadline = std::cmp::min(
        deadline,
        tokio::time::Instant::now() + timing.health_timeout,
    );
    let startup = match tokio::time::timeout_at(startup_deadline, &mut startup).await {
        Ok(Ok(startup)) => Some(startup),
        Ok(Err(error)) => {
            tracing::warn!(
                token_contract = %display_token_contract(&cfg.token_contract),
                error = %error,
                "seller startup failed after lifecycle interruption; reconciling authoritative state"
            );
            None
        }
        Err(_) => None,
    };
    resolve_interrupted_startup(
        seller,
        chain,
        cfg,
        expected_owner,
        startup,
        (reason, deadline),
        timing,
    )
    .await
}

pub async fn prepare_seller_offer_with_liveness<S>(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    expected_owner: &str,
    existing_identity: Option<&RestingOfferIdentity>,
    shutdown: S,
    advertise_probe: AdvertiseProbePolicy,
) -> Result<SellerStartupOutcome>
where
    S: Future<Output = ()>,
{
    prepare_seller_offer_with_timing(
        seller,
        chain,
        cfg,
        expected_owner,
        existing_identity,
        shutdown,
        canonical_timing(true, advertise_probe),
    )
    .await
}

/// Production-only write-ahead wrapper around the normal liveness startup.
///
/// The existing liveness path owns readiness, cancellation and gateway
/// supervision.  This wrapper adds one invariant at its outer money boundary:
/// after a marker exists, the next start first reconciles the exact deal and
/// never falls through to another post merely because a read timed out.
pub async fn prepare_seller_offer_with_persisted_liveness<S>(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    expected_owner: &str,
    existing_identity: Option<&RestingOfferIdentity>,
    cursor_path: &Path,
    shutdown: S,
    advertise_probe: AdvertiseProbePolicy,
) -> Result<SellerStartupOutcome>
where
    S: Future<Output = ()>,
{
    if pending_offer_submission(cursor_path, &cfg.token_contract)?.is_some() {
        match reconcile_pending_offer_submission(chain, cfg, Some(expected_owner)).await {
            Ok(
                super::PendingPublicationResolution::Resting { .. }
                | super::PendingPublicationResolution::Funded,
            ) => {
                clear_offer_submission(cursor_path, &cfg.token_contract)?;
            }
            Ok(super::PendingPublicationResolution::Negative) => {
                clear_offer_submission(cursor_path, &cfg.token_contract)?;
                return Err(anyhow!(
                    "publication_unconfirmed token_contract={}: the prior marked post has an exact negative proof; a fresh explicit seller start may retry, but this start will not repost automatically",
                    display_token_contract(&cfg.token_contract)
                ));
            }
            Err(error) => return Err(error),
        }
    } else {
        // This write is intentionally before liveness enters the only path
        // which can call postSellOffer.  If readiness aborts before the write,
        // the next explicit start obtains a harmless exact negative proof; the
        // conservative false-positive is preferable to a duplicate SELL.
        persist_offer_submission(cursor_path, &cfg.token_contract)?;
    }

    let started = prepare_seller_offer_with_liveness(
        seller,
        chain,
        cfg,
        expected_owner,
        existing_identity,
        shutdown,
        advertise_probe,
    )
    .await;

    match reconcile_pending_offer_submission(chain, cfg, Some(expected_owner)).await {
        Ok(super::PendingPublicationResolution::Resting { order_id }) => {
            clear_offer_submission(cursor_path, &cfg.token_contract)?;
            match started {
                Ok(SellerStartupOutcome::Stopped { .. }) => Ok(SellerStartupOutcome::Ready(
                    SellerOfferStartup::ResumedResting { order_id },
                )),
                other => other,
            }
        }
        Ok(super::PendingPublicationResolution::Funded) => {
            clear_offer_submission(cursor_path, &cfg.token_contract)?;
            match started {
                Ok(SellerStartupOutcome::Stopped { .. }) => Ok(SellerStartupOutcome::Ready(
                    SellerOfferStartup::ResumedFunded,
                )),
                other => other,
            }
        }
        Ok(super::PendingPublicationResolution::Negative) => {
            clear_offer_submission(cursor_path, &cfg.token_contract)?;
            Err(anyhow!(
                "publication_unconfirmed token_contract={}: the marked post has an exact negative proof; a fresh explicit seller start may retry, but this start will not repost automatically",
                display_token_contract(&cfg.token_contract)
            ))
        }
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy)]
struct SupervisionTiming {
    health_interval: Duration,
    health_timeout: Duration,
    cycle_timeout: Duration,
    cancel_poll: Duration,
    /// how often the supervised order's own deadline is re-read from the book.
    expiry_poll: Duration,
    /// whole budget for proving one expired ask was reaped before any successor is posted.
    reap_timeout: Duration,
    /// poll interval while confirming that reap.
    reap_poll: Duration,
    abort_gateway_on_stop: bool,
    /// how a failed `advertised_gateway` self-probe is treated.
    advertise_probe: AdvertiseProbePolicy,
}

async fn supervise_with_timing<S>(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    watch: &SellerMatchWatchConfig,
    identity: &RestingOfferIdentity,
    shutdown: S,
    timing: SupervisionTiming,
) -> Result<RestingSellerOutcome>
where
    S: Future<Output = ()>,
{
    enum Trigger {
        Health(HealthFailure),
        Shutdown,
        Watcher(String),
        Expired(RestingOfferExpiry),
    }

    let decision = {
        let matched = wait_for_match(seller, chain, cfg, watch);
        tokio::pin!(matched);
        tokio::pin!(shutdown);
        let mut last_healthy = tokio::time::Instant::now();
        let start = tokio::time::Instant::now() + timing.health_interval;
        let mut health = tokio::time::interval_at(start, timing.health_interval);
        health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // expiry gets its own timer. `wait_for_match` never returns for an order that simply
        // died, and health checks only ask about the gateway and the upstream -- without this branch a
        // seller whose offer expired keeps logging healthy cycles and waiting for a match that can no
        // longer happen, which is exactly what the live incident showed.
        let mut expiry = tokio::time::interval_at(
            tokio::time::Instant::now() + timing.expiry_poll,
            timing.expiry_poll,
        );
        expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // the last deadline the BOOK held for this exact order, so a row swept between two
        // polls is still reported with the figure this client actually read rather than a
        // reconstruction.
        let mut last_deadline: Option<u64> = None;
        let mut expiry_read_attempt_total = 0_u64;
        let mut consecutive_expiry_read_failures = 0_u64;
        let mut last_successful_expiry_read = tokio::time::Instant::now();

        'supervision: loop {
            tokio::select! {
                match_result = &mut matched => {
                    break match match_result {
                        Ok(matched) => Ok(matched),
                        Err(error) => Err((
                            Trigger::Watcher(error.to_string()),
                            tokio::time::Instant::now() + timing.cycle_timeout,
                        )),
                    };
                }
                _ = &mut shutdown => break Err((
                    Trigger::Shutdown,
                    tokio::time::Instant::now() + timing.cycle_timeout,
                )),
                _ = health.tick() => {
                    // The cycle budget can be spent before this arm is ever entered. The expiry
                    // read in the same `select!` holds the loop for up to
                    // `TRANSIENT_READ_TOTAL_BUDGET` (45s) while the health cadence is
                    // `health_interval` (20s), so ONE slow book read is enough for the health tick
                    // to arrive later than `last_healthy + cycle_timeout`. Sizing the check from
                    // what is left of that window then hands it ZERO: the `timeout_at` in
                    // `check_readiness_with_probe` fires before a socket is opened, the seller
                    // reports `handshake_timeout` against a gateway nobody contacted, cancels a
                    // healthy resting SELL, and tells the operator to clean up an order that was
                    // never sick. Observed on a long-lived seller: last healthy 12:37:01, next
                    // cycle 12:38:41, deal retired on a probe that never ran.

                    // A starved cycle is evidence about this loop, not about the gateway. Give the
                    // check its own budget and measure the cancellation headroom from the instant
                    // it actually starts.
                    let cycle_start = tokio::time::Instant::now();
                    let starved = last_healthy + timing.cycle_timeout <= cycle_start;
                    if starved {
                        tracing::warn!(
                            event = "seller_health_cycle_starved",
                            timestamp = unix_timestamp(),
                            owner_note = %identity.owner_note,
                            token_contract = %identity.token_contract,
                            order_id = identity.order_id,
                            since_last_healthy_ms = last_healthy.elapsed().as_millis() as u64,
                            cycle_timeout_ms = timing.cycle_timeout.as_millis() as u64,
                            "supervision reached the readiness check after its own cycle budget had \
                             expired; rebasing the cycle rather than failing a check that never ran"
                        );
                    }
                    let cycle_base = if starved { cycle_start } else { last_healthy };
                    let deadline = cycle_base + timing.cycle_timeout;
                    #[cfg(test)]
                    observe_cycle_deadline(CycleDeadlineSite::HealthCheck, deadline);
                    let retry_allowance = timing.health_timeout.saturating_mul(
                        SELLER_UPSTREAM_HEALTH_TIMEOUT_MAX_ATTEMPTS.saturating_sub(1),
                    );
                    let timeout_deadline = deadline + retry_allowance;
                    let mut attempt = 1;
                    let mut check_deadline = deadline;
                    loop {
                        let remaining = check_deadline
                            .saturating_duration_since(tokio::time::Instant::now());
                        let check_timeout = std::cmp::min(timing.health_timeout, remaining);
                        match check_readiness(
                            seller,
                            &cfg.gateway_advertise,
                            check_timeout,
                            Some(identity),
                            &identity.token_contract,
                            timing.advertise_probe,
                        ).await {
                            Ok(()) => {
                                last_healthy = tokio::time::Instant::now();
                                break;
                            }
                            // Both readiness components are bounded network probes sharing one
                            // deadline, so both can lose it to an isolated stall -- which is what
                            // `SELLER_UPSTREAM_HEALTH_TIMEOUT_MAX_ATTEMPTS` exists to absorb. The
                            // self-probe was excluded, and since it is matched first, a stalled
                            // gateway probe retired the deal even when the retry would have found
                            // both components healthy. A timeout is not a failed probe: makes
                            // a self-probe FAILURE fatal, and that stays.
                            Err(failure)
                                if failure.timed_out
                                    && matches!(
                                        failure.component,
                                        HealthComponent::UpstreamModel
                                            | HealthComponent::AdvertisedGateway
                                    )
                                    && attempt
                                        < SELLER_UPSTREAM_HEALTH_TIMEOUT_MAX_ATTEMPTS =>
                            {
                                tracing::warn!(
                                    event = "seller_health_timeout_retry",
                                    component = failure.component.as_str(),
                                    timestamp = unix_timestamp(),
                                    owner_note = %display_dexdo_address(&identity.owner_note),
                                    token_contract = %display_token_contract(&identity.token_contract),
                                    order_id = identity.order_id,
                                    failed_attempt = attempt,
                                    next_attempt = attempt + 1,
                                    max_attempts = SELLER_UPSTREAM_HEALTH_TIMEOUT_MAX_ATTEMPTS,
                                    reason = %failure,
                                    "timed-out readiness probe; retrying before retiring exact SELL"
                                );
                                attempt += 1;
                                check_deadline = timeout_deadline;
                            }
                            Err(failure) => {
                                // A timeout-authorized retry may consume the original cycle even if
                                // it returns a different terminal class; keep cancellation headroom.
                                let cancellation_deadline = if attempt > 1 {
                                    timeout_deadline
                                } else {
                                    deadline
                                };
                                break 'supervision Err((
                                    Trigger::Health(failure),
                                    cancellation_deadline,
                                ));
                            }
                        }
                    }
                }
                _ = expiry.tick() => {
                    // The read gets the total budget of the retry it wraps, not the shorter cadence
                    // that decides when the next read starts. A book that never answers still cannot
                    // wedge supervision: the nested select keeps shutdown and the match watcher live
                    // while this arm waits for the bounded read.

                    // A transport blip is not proof of expiry either, so an unreadable book leaves the
                    // seller owning an offer whose current expiry is unverified, and the next tick asks
                    // again. Only an authoritative row that is present AND past its deadline ends
                    // supervision here.
                    expiry_read_attempt_total = expiry_read_attempt_total.saturating_add(1);
                    let read = tokio::time::timeout(
                        TRANSIENT_READ_TOTAL_BUDGET,
                        resting_offer_expiry(chain, identity, last_deadline),
                    );
                    tokio::pin!(read);
                    let read_result = tokio::select! {
                        match_result = &mut matched => {
                            break match match_result {
                                Ok(matched) => Ok(matched),
                                Err(error) => Err((
                                    Trigger::Watcher(error.to_string()),
                                    tokio::time::Instant::now() + timing.cycle_timeout,
                                )),
                            };
                        }
                        _ = &mut shutdown => break Err((
                            Trigger::Shutdown,
                            tokio::time::Instant::now() + timing.cycle_timeout,
                        )),
                        read_result = &mut read => read_result,
                    };
                    match read_result {
                        Ok(Ok(poll)) => {
                            consecutive_expiry_read_failures = 0;
                            last_successful_expiry_read = tokio::time::Instant::now();
                            match poll {
                                SupervisedOfferPoll::Expired(expired) => break Err((
                                    Trigger::Expired(expired),
                                    tokio::time::Instant::now() + timing.cycle_timeout,
                                )),
                                SupervisedOfferPoll::Live { deadline } => {
                                    last_deadline = Some(deadline);
                                }
                                SupervisedOfferPoll::Matched => {}
                            }
                        }
                        Ok(Err(error)) => {
                            consecutive_expiry_read_failures =
                                consecutive_expiry_read_failures.saturating_add(1);
                            trace_offer_expiry_read_failure(
                                identity,
                                error,
                                consecutive_expiry_read_failures,
                                last_successful_expiry_read.elapsed(),
                                expiry_read_attempt_total,
                            );
                        }
                        Err(_) => {
                            consecutive_expiry_read_failures =
                                consecutive_expiry_read_failures.saturating_add(1);
                            trace_offer_expiry_read_failure(
                                identity,
                                "timeout",
                                consecutive_expiry_read_failures,
                                last_successful_expiry_read.elapsed(),
                                expiry_read_attempt_total,
                            );
                        }
                    }
                }
            }
        }
    };

    let (trigger, deadline) = match decision {
        Ok(matched) => return Ok(RestingSellerOutcome::Matched(matched)),
        Err(trigger) => trigger,
    };

    // expiry is terminal on its own and needs no write. The order is already unmatchable on
    // chain, so cancelling it would spend gas to remove something the matcher already skips, and the
    // cancel would race the permissionless sweep for no gain. Readiness ends here, before any cleanup
    // or relist work -- that is's job, and it starts from this outcome.

    // The gateway is deliberately left running: an expired offer is not an unhealthy seller, and the
    // successor offer posts needs the same gateway still answering.
    if let Trigger::Expired(expired) = trigger {
        trace_offer_expired(identity, &expired);
        return Ok(RestingSellerOutcome::Stopped {
            reason: RestingStopReason::Expired(expired),
            disposition: CancellationDisposition::NotAttemptedExpired,
        });
    }

    let disposition = cancel_and_confirm_before(
        chain,
        cfg,
        identity,
        deadline,
        timing.cycle_timeout,
        timing.cancel_poll,
    )
    .await;
    let disposition = match disposition {
        CancellationDisposition::AlreadyMatched(matched) => {
            return Ok(RestingSellerOutcome::Matched(matched));
        }
        disposition => disposition,
    };
    if timing.abort_gateway_on_stop && disposition.proven_unmatchable() {
        seller.server_task.abort();
    }
    Ok(RestingSellerOutcome::Stopped {
        reason: match trigger {
            Trigger::Health(failure) => RestingStopReason::Health(failure),
            Trigger::Shutdown => RestingStopReason::Shutdown,
            Trigger::Watcher(error) => RestingStopReason::Watcher(error),
            // Handled above: expiry returns before any cancellation is attempted.
            Trigger::Expired(expired) => RestingStopReason::Expired(expired),
        },
        disposition,
    })
}

/// Supervise one resting ask and, when it dies of its own deadline, reap it and carry the deal's
/// remaining capacity into exactly one successor.

/// A SELL's deadline is mandatory and capped at `MAX_SELL_TTL = 3600`
/// (`contracts/dex/PrivateNote.sol:41,792`), so an alive seller meets it by design rather than by
/// fault. Every iteration of this loop supervises ONE generation: the loop only turns after the
/// previous generation is authoritatively off the book, and it stops for good at the first outcome
/// that is not an expiry.

/// The successor rests on the SAME deal, and that is not a shortcut. A `TokenContract`'s
/// `_maxTicks` and `_pricePerTick` are constructor statics -- `postFromNote` re-posts exactly them
/// (`contracts/airegistry/TokenContract.sol:713-718`) -- and an ask leaving the book WITHOUT a fill
/// frees the deal's latch precisely so the same live TC can carry the next one
/// (`contracts/airegistry/TokenContract.sol:700-703`). An unfunded deal has therefore sold nothing:
/// any fill would have removed the ask and funded the TC in the same match
/// (`contracts/airegistry/InferenceOrderBook.sol:1088-1092`), so `getDeal().maxTicks` IS the
/// remaining capacity, read here from the deal itself rather than copied out of the expired row.
#[allow(clippy::too_many_arguments)]
/// the relist loop, REPORTING the generation it ends on.

/// Split from `supervise_and_relist_with_timing` rather than replacing it: that entry point has
/// callers whose identity is not theirs to lend mutably, and they do not care which generation the
/// loop finished on. Only the pool does -- it holds the entry the shutdown sweep acts upon.
async fn supervise_and_relist_reporting_identity<S>(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    watch: &SellerMatchWatchConfig,
    identity: &mut RestingOfferIdentity,
    shutdown: S,
    timing: SupervisionTiming,
) -> Result<RestingSellerOutcome>
where
    S: Future<Output = ()>,
{
    tokio::pin!(shutdown);
    // NOT a clone. The relist loop below advances this to the successor, and the pool
    // holds the entry the shutdown sweep will act on -- so advancing a private copy left the
    // pool pointing at a consumed predecessor and the successor resting unserved.
    let mut cfg = cfg.clone();
    loop {
        let expired = match supervise_with_timing(
            seller,
            chain,
            &cfg,
            watch,
            &*identity,
            shutdown.as_mut(),
            timing,
        )
        .await?
        {
            RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Expired(expired),
                ..
            } => expired,
            settled => return Ok(settled),
        };

        let reap_deadline = tokio::time::Instant::now() + timing.reap_timeout;
        let (remaining_ticks, price_per_tick) = match reap_expired_offer(
            chain,
            &cfg,
            &*identity,
            reap_deadline,
            timing.reap_poll,
        )
        .await
        {
            RelistDecision::Relist {
                remaining_ticks,
                price_per_tick,
            } => (remaining_ticks, price_per_tick),
            RelistDecision::Matched(matched) => return Ok(RestingSellerOutcome::Matched(matched)),
            RelistDecision::Refused { reason } => {
                tracing::warn!(
                    event = "seller_offer_relist_refused",
                    timestamp = unix_timestamp(),
                    owner_note = %display_dexdo_address(&identity.owner_note),
                    token_contract = %display_token_contract(&identity.token_contract),
                    order_id = identity.order_id,
                    disposition = "reaped_not_relisted",
                    reason = %reason,
                    "expired ask reaped; the deal is not this seller's to re-offer"
                );
                return Ok(RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Expired(expired),
                    disposition: CancellationDisposition::ReapedNotRelisted { reason },
                });
            }
            RelistDecision::Unproven { known_result } => {
                tracing::error!(
                    event = "seller_offer_relist_terminal",
                    timestamp = unix_timestamp(),
                    owner_note = %display_dexdo_address(&identity.owner_note),
                    token_contract = %display_token_contract(&identity.token_contract),
                    order_id = identity.order_id,
                    disposition = "unknown_failure",
                    known_result = %known_result,
                    "expiry cleanup has no terminal authoritative fact; refusing to relist"
                );
                return Ok(RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Expired(expired),
                    disposition: CancellationDisposition::UnknownFailure { known_result },
                });
            }
        };

        cfg.price_per_tick = price_per_tick;
        cfg.max_ticks = remaining_ticks;
        let successor = match prepare_seller_offer_with_timing(
            seller,
            chain,
            &cfg,
            &identity.owner_note,
            None,
            shutdown.as_mut(),
            timing,
        )
        .await?
        {
            SellerStartupOutcome::Ready(SellerOfferStartup::ResumedFunded)
            | SellerStartupOutcome::Ready(SellerOfferStartup::Posted {
                outcome: Some(dexdo_core::SellOfferOutcome::Matched),
            }) => {
                return Ok(RestingSellerOutcome::Matched(
                    chain.read_match(&identity.token_contract).await?,
                ))
            }
            SellerStartupOutcome::Ready(SellerOfferStartup::Posted {
                outcome: Some(dexdo_core::SellOfferOutcome::Rested { order_id }),
            })
            | SellerStartupOutcome::Ready(SellerOfferStartup::ResumedResting { order_id }) => {
                order_id
            }
            SellerStartupOutcome::Ready(SellerOfferStartup::Posted { outcome: None }) => {
                // `prepare_seller_offer_with_timing` only returns `Ready` after an exact outcome, so
                // this arm cannot be reached; treating it as an unconfirmed write keeps the
                // fail-closed rule even if that ever changes.
                return Ok(RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Expired(expired),
                    disposition: CancellationDisposition::UnknownFailure {
                        known_result: format!(
                            "successor_post=unconfirmed for TokenContract {}",
                            display_token_contract(&identity.token_contract)
                        ),
                    },
                });
            }
            SellerStartupOutcome::Stopped {
                reason,
                disposition,
                ..
            } => {
                return Ok(RestingSellerOutcome::Stopped {
                    reason,
                    disposition,
                })
            }
        };

        // The reaped id can never come back: `_removeFromBook` deletes it and the book allocates the
        // next one. Reading it here again would mean the successor is the corpse, so supervising it
        // would spin this loop against an order that can never be live.
        if successor == identity.order_id {
            return Ok(RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Expired(expired),
                disposition: CancellationDisposition::UnknownFailure {
                    known_result: format!(
                        "successor_post=returned the reaped order id {} for TokenContract {}",
                        successor,
                        display_token_contract(&identity.token_contract)
                    ),
                },
            });
        }
        let successor_identity = RestingOfferIdentity {
            order_id: successor,
            ..(*identity).clone()
        };
        let successor_deadline = match successor_absolute_deadline(
            chain,
            &successor_identity,
            tokio::time::Instant::now() + timing.reap_timeout,
            timing.reap_poll,
        )
        .await
        {
            Ok(deadline) if deadline > expired.deadline => deadline,
            Ok(deadline) => {
                return Ok(RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Expired(expired),
                    disposition: CancellationDisposition::UnknownFailure {
                        known_result: format!(
                            "successor order {successor} carries deadline {deadline}, which does not \
                             advance past the reaped {}; TokenContract {} now rests an offer this \
                             seller will not supervise",
                            expired.deadline, display_token_contract(&identity.token_contract)
                        ),
                    },
                });
            }
            Err(known_result) => {
                return Ok(RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Expired(expired),
                    disposition: CancellationDisposition::UnknownFailure { known_result },
                });
            }
        };
        trace_offer_relisted(
            &*identity,
            &expired,
            successor,
            successor_deadline,
            remaining_ticks,
        );
        *identity = successor_identity;
    }
}

/// The relist loop for callers that do not need the generation it ended on. Behaviour is unchanged:
/// it supervises a private copy, exactly as it always did.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn supervise_and_relist_with_timing<S>(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    watch: &SellerMatchWatchConfig,
    identity: &RestingOfferIdentity,
    shutdown: S,
    timing: SupervisionTiming,
) -> Result<RestingSellerOutcome>
where
    S: Future<Output = ()>,
{
    let mut owned = identity.clone();
    supervise_and_relist_reporting_identity(seller, chain, cfg, watch, &mut owned, shutdown, timing)
        .await
}

/// The successor's own absolute deadline, straight out of the book.

/// Read rather than reconstructed, for the same reason `RestingOfferExpiry` carries the book's
/// figure: the chain anchors it at `block.timestamp + ttl` inside `PrivateNote.postSellOffer`
/// (`contracts/dex/PrivateNote.sol:793`). It also bounds the relist loop -- a successor must outlive
/// the generation it replaced, and one that does not is reported instead of supervised.
async fn successor_absolute_deadline(
    chain: &dyn ChainBackend,
    identity: &RestingOfferIdentity,
    deadline: tokio::time::Instant,
    poll_interval: Duration,
) -> std::result::Result<u64, String> {
    let mut last;
    loop {
        match tokio::time::timeout_at(
            deadline,
            chain.raw_resting_sell_orders_for_tc(&identity.token_contract),
        )
        .await
        {
            Ok(Ok(orders)) => match orders
                .iter()
                .find(|order| order.order_id == identity.order_id)
            {
                Some(order) => return Ok(order.deadline),
                None => {
                    last = format!(
                        "authoritative_state=accepted successor {} is absent from the book",
                        identity.order_id
                    )
                }
            },
            Ok(Err(error)) => last = format!("authoritative_read=failed: {error}"),
            Err(_) => last = "authoritative_read=timeout".to_string(),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "{last}; operator_action=run `dexdo orders list` with the same `--note-addr` and \
                 `--market` or `--model` this seller was started with, and supervise or cancel \
                 order {} on TokenContract {} by hand",
                identity.order_id,
                display_token_contract(&identity.token_contract)
            ));
        }
        let wake_at = std::cmp::min(tokio::time::Instant::now() + poll_interval, deadline);
        tokio::time::sleep_until(wake_at).await;
    }
}

/// Supervise exactly ONE generation of a resting ask and return its terminal outcome.

/// the seller process itself uses [`supervise_resting_offer_with_relist`], because a deadline is
/// not the end of an alive seller. This entry stays for the callers that want a single generation and
/// the outcome that ended it -- an acceptance run asserting one exact terminal fact, not a daemon.
#[allow(clippy::too_many_arguments)]
pub async fn supervise_resting_offer<S>(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    watch: &SellerMatchWatchConfig,
    identity: &RestingOfferIdentity,
    shutdown: S,
    abort_gateway_on_stop: bool,
    advertise_probe: AdvertiseProbePolicy,
) -> Result<RestingSellerOutcome>
where
    S: Future<Output = ()>,
{
    supervise_with_timing(
        seller,
        chain,
        cfg,
        watch,
        identity,
        shutdown,
        canonical_timing(abort_gateway_on_stop, advertise_probe),
    )
    .await
}

/// supervise the resting ask and keep the deal available across its mandatory deadlines.
#[allow(clippy::too_many_arguments)]
pub async fn supervise_resting_offer_with_relist<S>(
    seller: &RunningSeller,
    chain: &dyn ChainBackend,
    cfg: &SellerConfig,
    watch: &SellerMatchWatchConfig,
    identity: &mut RestingOfferIdentity,
    shutdown: S,
    abort_gateway_on_stop: bool,
    advertise_probe: AdvertiseProbePolicy,
) -> Result<RestingSellerOutcome>
where
    S: Future<Output = ()>,
{
    supervise_and_relist_reporting_identity(
        seller,
        chain,
        cfg,
        watch,
        identity,
        shutdown,
        canonical_timing(abort_gateway_on_stop, advertise_probe),
    )
    .await
}

fn canonical_timing(
    abort_gateway_on_stop: bool,
    advertise_probe: AdvertiseProbePolicy,
) -> SupervisionTiming {
    let params = SellerLivenessParams::canonical();
    SupervisionTiming {
        health_interval: params.health_interval,
        health_timeout: params.health_check_timeout,
        cycle_timeout: params.health_cycle_timeout,
        cancel_poll: params.cancel_confirmation_poll,
        expiry_poll: params.offer_expiry_poll,
        reap_timeout: params.offer_reap_timeout,
        reap_poll: params.offer_reap_poll,
        abort_gateway_on_stop,
        advertise_probe,
    }
}

#[cfg(test)]
mod tests {

    /// the seller's manual-cancellation guidance is the one output path here that names a
    /// command, and it must not pretend to be a runnable line: `orders` needs an identity and a
    /// market or model this module does not hold, and cancelling needs a key to sign. Every
    /// command span it prints is therefore a command *name* -- nothing follows it inside the
    /// backticks -- with the inputs stated in prose. (This module compiles into the library, which
    /// has no clap parser; the parser-level check on these same literals is the source lint in the
    /// binary.)
    #[test]
    fn manual_cancellation_guidance_names_commands_it_cannot_complete() {
        for action in [
            super::manual_cancel_action(7),
            super::manual_cancel_action_for_unknown_order(),
        ] {
            assert!(action.contains("`dexdo orders cancel`"), "{action}");
            assert!(
                !action.contains("`dexdo orders cancel "),
                "the span reads as a line to run, but this guidance cannot complete one: {action}"
            );
            assert!(
                !action.contains("`dexdo orders list "),
                "the span reads as a line to run, but this guidance cannot complete one: {action}"
            );
            for stated in ["--note-addr", "--note-key", "--market", "--model"] {
                assert!(action.contains(stated), "{stated} is not stated: {action}");
            }
        }
    }

    use super::*;
    use crate::seller::{Capabilities, OpenAiConfig, SampleAlgorithm, UpstreamConfig};
    use dexdo_core::{
        ChainError, DealBuyerBond, DealChainSnapshot, DealChainState, DealOfferLatch,
        DealSellerBond, DealSubscription, LocalNote, Note, NotePubkey, OfferListing,
        OrderBookOrder, SellOffer, SellOfferOutcome, Settlement, StreamSnapshot, TokenContract,
    };
    use futures::{future::FusedFuture as _, FutureExt as _};
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Clone, Copy)]
    enum CancelBehavior {
        Remove,
        Reject,
        AmbiguousRemove,
        Keep,
        RemoveAfterReads(u64),
        TerminalReject(u8),
        Hang,
    }

    #[derive(Clone, Copy)]
    enum PostVisibility {
        Immediate,
        AfterVacantReads(u64),
        Never,
    }

    struct CancelBackend {
        orders: Arc<Mutex<Vec<OrderBookOrder>>>,
        matched: Arc<Mutex<Option<Match>>>,
        calls: Mutex<Vec<(TokenContract, u128)>>,
        posts: AtomicU64,
        behavior: CancelBehavior,
        owner: String,
        posted_order_id: u128,
        hang_reads: bool,
        watch_fails: bool,
        confirm_delay: Duration,
        confirm_ready: Option<Arc<tokio::sync::Notify>>,
        post_visibility: PostVisibility,
        pending_post: Mutex<Option<OrderBookOrder>>,
        post_visibility_reads: AtomicU64,
        post_submitted: tokio::sync::watch::Sender<bool>,
        post_release: tokio::sync::watch::Sender<bool>,
        open_delay: Duration,
        opens: AtomicU64,
        post_submit_reads: AtomicU64,
        /// E2E-ADV-14: when set, `assert_note_covers_seller_bond` refuses with this message, standing
        /// in for a seller note whose record cannot pay the deal's `2P` mirror bond.
        bond_cover_error: Option<String>,
    }

    impl CancelBackend {
        fn new(
            orders: Vec<OrderBookOrder>,
            owner: String,
            posted_order_id: u128,
            behavior: CancelBehavior,
        ) -> Self {
            let (post_submitted, _) = tokio::sync::watch::channel(false);
            let (post_release, _) = tokio::sync::watch::channel(false);
            Self {
                orders: Arc::new(Mutex::new(orders)),
                matched: Arc::new(Mutex::new(None)),
                calls: Mutex::new(Vec::new()),
                posts: AtomicU64::new(0),
                behavior,
                owner,
                posted_order_id,
                hang_reads: false,
                watch_fails: false,
                confirm_delay: Duration::ZERO,
                confirm_ready: None,
                post_visibility: PostVisibility::Immediate,
                pending_post: Mutex::new(None),
                post_visibility_reads: AtomicU64::new(0),
                post_submitted,
                post_release,
                open_delay: Duration::ZERO,
                opens: AtomicU64::new(0),
                post_submit_reads: AtomicU64::new(0),
                bond_cover_error: None,
            }
        }

        fn with_bond_cover_error(mut self, message: &str) -> Self {
            self.bond_cover_error = Some(message.to_string());
            self
        }

        fn with_hanging_reads(mut self) -> Self {
            self.hang_reads = true;
            self
        }

        fn with_watcher_error(mut self) -> Self {
            self.watch_fails = true;
            self
        }

        fn with_confirm_delay(mut self, delay: Duration) -> Self {
            self.confirm_delay = delay;
            self
        }

        fn with_confirmation_ready(mut self, ready: Arc<tokio::sync::Notify>) -> Self {
            self.confirm_ready = Some(ready);
            self
        }

        fn with_post_visibility_after_vacant_reads(mut self, reads: u64) -> Self {
            self.post_visibility = PostVisibility::AfterVacantReads(reads);
            self
        }

        fn with_post_never_visible(mut self) -> Self {
            self.post_visibility = PostVisibility::Never;
            self
        }

        fn with_open_delay(mut self, delay: Duration) -> Self {
            self.open_delay = delay;
            self
        }

        fn calls(&self) -> Vec<(TokenContract, u128)> {
            self.calls.lock().unwrap().clone()
        }

        fn order_ids(&self) -> Vec<u128> {
            self.orders
                .lock()
                .unwrap()
                .iter()
                .map(|order| order.order_id)
                .collect()
        }

        async fn wait_for_post_submission(&self) {
            let mut submitted = self.post_submitted.subscribe();
            if !*submitted.borrow_and_update() {
                submitted
                    .changed()
                    .await
                    .expect("test POST submission signal stays alive");
            }
        }

        fn release_interrupted_post(&self) {
            self.post_release.send_replace(true);
        }
    }

    #[async_trait::async_trait]
    impl ChainBackend for CancelBackend {
        async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
            Ok(Vec::new())
        }

        async fn assert_note_covers_seller_bond(
            &self,
            _: &TokenContract,
        ) -> Result<(), ChainError> {
            match &self.bond_cover_error {
                Some(message) => Err(ChainError::Chain(message.clone())),
                None => Ok(()),
            }
        }

        async fn post_offer(&self, offer: SellOffer, _: &dyn Note) -> Result<(), ChainError> {
            self.posts.fetch_add(1, Ordering::Relaxed);
            let posted = order(self.posted_order_id, &self.owner, &offer.token_contract);
            match self.post_visibility {
                PostVisibility::Immediate => {
                    self.orders.lock().unwrap().push(posted);
                    self.post_submitted.send_replace(true);
                    Ok(())
                }
                PostVisibility::AfterVacantReads(_) | PostVisibility::Never => {
                    *self.pending_post.lock().unwrap() = Some(posted);
                    let mut release = self.post_release.subscribe();
                    self.post_submitted.send_replace(true);
                    if !*release.borrow_and_update() {
                        release
                            .changed()
                            .await
                            .expect("test interrupted-POST release signal stays alive");
                    }
                    Err(ChainError::Transport(
                        "test POST interrupted after submission".to_string(),
                    ))
                }
            }
        }

        async fn confirm_offer_outcome(
            &self,
            _: &TokenContract,
        ) -> Result<Option<SellOfferOutcome>, ChainError> {
            if let Some(ready) = &self.confirm_ready {
                ready.notified().await;
            } else {
                tokio::time::sleep(self.confirm_delay).await;
            }
            Ok(Some(SellOfferOutcome::Rested {
                order_id: self.posted_order_id,
            }))
        }

        async fn raw_resting_sell_orders_for_tc(
            &self,
            token_contract: &TokenContract,
        ) -> Result<Vec<OrderBookOrder>, ChainError> {
            if self.hang_reads {
                return std::future::pending().await;
            }
            if self.posts.load(Ordering::Relaxed) > 0 {
                let reveal = match self.post_visibility {
                    PostVisibility::AfterVacantReads(reads) => {
                        self.post_visibility_reads.fetch_add(1, Ordering::SeqCst) >= reads
                    }
                    PostVisibility::Immediate | PostVisibility::Never => false,
                };
                if reveal {
                    if let Some(posted) = self.pending_post.lock().unwrap().take() {
                        self.orders.lock().unwrap().push(posted);
                    }
                }
            }
            if let CancelBehavior::RemoveAfterReads(keep_reads) = self.behavior {
                if !self.calls.lock().unwrap().is_empty() {
                    let read = self.post_submit_reads.fetch_add(1, Ordering::SeqCst);
                    if read >= keep_reads {
                        self.orders
                            .lock()
                            .unwrap()
                            .retain(|order| order.order_id != self.posted_order_id);
                    }
                }
            }
            Ok(self
                .orders
                .lock()
                .unwrap()
                .iter()
                .filter(|order| order.token_contract.as_ref() == Some(token_contract))
                .cloned()
                .collect())
        }

        async fn cancel_resting_sell_order(
            &self,
            token_contract: &TokenContract,
            order_id: u128,
        ) -> Result<(), ChainError> {
            self.calls
                .lock()
                .unwrap()
                .push((token_contract.clone(), order_id));
            match self.behavior {
                CancelBehavior::Remove => {
                    self.orders
                        .lock()
                        .unwrap()
                        .retain(|order| order.order_id != order_id);
                    Ok(())
                }
                CancelBehavior::Reject => Err(ChainError::Chain(
                    "cancel rejected by owner check".to_string(),
                )),
                CancelBehavior::AmbiguousRemove => {
                    self.orders
                        .lock()
                        .unwrap()
                        .retain(|order| order.order_id != order_id);
                    Err(ChainError::Transport(
                        "response lost after submit".to_string(),
                    ))
                }
                CancelBehavior::Keep
                | CancelBehavior::RemoveAfterReads(_)
                | CancelBehavior::TerminalReject(_) => Ok(()),
                CancelBehavior::Hang => std::future::pending().await,
            }
        }

        async fn resting_sell_cancel_rejection_after(
            &self,
            token_contract: &TokenContract,
            order_id: u128,
            owner_note: &str,
            _: &RestingSellCancelWatch,
        ) -> Result<Option<u8>, ChainError> {
            Ok(match self.behavior {
                CancelBehavior::TerminalReject(reason)
                    if token_contract
                        == self.orders.lock().unwrap()[0]
                            .token_contract
                            .as_ref()
                            .expect("test SELL token contract")
                        && order_id == self.posted_order_id
                        && owner_note == self.owner
                        && !self.calls.lock().unwrap().is_empty() =>
                {
                    Some(reason)
                }
                _ => None,
            })
        }

        async fn poll_seller_fills(
            &self,
            _: &dyn Note,
            _: &mut dexdo_core::MatchWatchCursor,
        ) -> Result<Vec<dexdo_core::MatchedFill>, ChainError> {
            if self.watch_fails {
                return Err(ChainError::Chain(
                    "authoritative match watcher failed".to_string(),
                ));
            }
            Ok(Vec::new())
        }

        async fn read_openable_match_now(
            &self,
            _: &TokenContract,
        ) -> Result<Option<Match>, ChainError> {
            Ok(self.matched.lock().unwrap().clone())
        }

        async fn place_buy(&self, _: &TokenContract, _: &dyn Note) -> Result<(), ChainError> {
            unimplemented!()
        }

        async fn read_match(&self, token_contract: &TokenContract) -> Result<Match, ChainError> {
            self.matched
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| ChainError::NoMatch(token_contract.clone()))
        }

        async fn open_stream(
            &self,
            _: &TokenContract,
            _: Vec<u8>,
            _: &dyn Note,
        ) -> Result<(), ChainError> {
            tokio::time::sleep(self.open_delay).await;
            self.opens.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn read_handover(&self, _: &TokenContract) -> Result<Option<Vec<u8>>, ChainError> {
            Ok(None)
        }

        async fn claim_tokens(
            &self,
            _: &TokenContract,
            _: &dyn Note,
            _: u128,
        ) -> Result<(), ChainError> {
            unimplemented!()
        }

        async fn accept_probe(&self, _: &TokenContract) -> Result<(), ChainError> {
            unimplemented!()
        }

        async fn stop(&self, _: &TokenContract, _: &dyn Note) -> Result<Settlement, ChainError> {
            unimplemented!()
        }

        async fn deal_state(
            &self,
            _: &TokenContract,
        ) -> Result<Option<DealChainState>, ChainError> {
            Ok(Some(DealChainState {
                funded: self.matched.lock().unwrap().is_some(),
                opened: false,
                probe_accepted: false,
                disputed: false,
                deposit: 0,
                finalized_owed: 0,
                tokens_final: 0,
                tokens_pending: 0,
                probe_tick: 0,
                funded_time: None,
                probe_time: 0,
                last_claim_time: 0,
                dispute_time: 0,
            }))
        }

        async fn deal_snapshot(
            &self,
            _: &TokenContract,
        ) -> Result<Option<DealChainSnapshot>, ChainError> {
            if self.matched.lock().unwrap().is_none() {
                return Ok(None);
            }
            let funded_tokens = 8 * dexdo_core::TICK_SIZE;
            Ok(Some(DealChainSnapshot {
                account_code_hash: "test-code".to_string(),
                account_boc_hash: "test-boc".to_string(),
                state: DealChainState {
                    funded: true,
                    opened: false,
                    probe_accepted: false,
                    disputed: false,
                    deposit: 1_000,
                    finalized_owed: 0,
                    tokens_final: 0,
                    tokens_pending: 0,
                    probe_tick: 0,
                    funded_time: Some(1),
                    probe_time: 0,
                    last_claim_time: 0,
                    dispute_time: 0,
                },
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
                    bond_held: 1,
                    bond_required: 1,
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
    }

    fn address(digit: char) -> String {
        format!("0:{}", digit.to_string().repeat(64))
    }

    fn cfg(token_contract: &str) -> SellerConfig {
        SellerConfig {
            token_contract: token_contract.to_string(),
            price_per_tick: 1000,
            max_ticks: 8,
            subscription: false,
            gateway_advertise: "127.0.0.1:0".to_string(),
            mock_token_count: 8,
        }
    }

    fn cfg_for_seller(token_contract: &str, seller: &RunningSeller) -> SellerConfig {
        let mut config = cfg(token_contract);
        config.gateway_advertise = seller.listen_addr.to_string();
        config
    }

    async fn wait_for_gateway_ready(seller: &RunningSeller) {
        tokio::time::timeout(
            Duration::from_secs(10),
            probe_gateway(
                &format!("https://{}", seller.listen_addr),
                &seller.tls_fingerprint,
            ),
        )
        .await
        .expect("the test gateway must finish its pinned-TLS warm-up probe")
        .expect("the test gateway must be reachable before startup begins");
    }

    fn identity(owner: &str, token_contract: &str, order_id: u128) -> RestingOfferIdentity {
        RestingOfferIdentity {
            owner_note: owner.to_string(),
            token_contract: token_contract.to_string(),
            order_id,
        }
    }

    /// A resting SELL as the chain can actually hold one. The deadline is live and finite because a
    /// SELL's is mandatory: `PrivateNote.postSellOffer` refuses `ttl == 0` and caps it at
    /// `MAX_SELL_TTL = 3600` (`contracts/dex/PrivateNote.sol:41,792`), so a zero here would describe a
    /// row the book cannot contain.
    fn order(order_id: u128, owner: &str, token_contract: &str) -> OrderBookOrder {
        order_with_deadline(
            order_id,
            owner,
            token_contract,
            unix_timestamp() + dexdo_core::params::MAX_SELL_TTL.as_secs(),
        )
    }

    fn order_with_deadline(
        order_id: u128,
        owner: &str,
        token_contract: &str,
        deadline: u64,
    ) -> OrderBookOrder {
        OrderBookOrder {
            order_id,
            owner_note: owner.to_string(),
            token_contract: Some(token_contract.to_string()),
            is_buy: false,
            price_per_tick: 1000,
            ticks: 8,
            escrow: 0,
            deadline,
            flags: 0,
            timestamp: 1,
        }
    }

    fn sample_match(token_contract: &str) -> Match {
        Match {
            token_contract: token_contract.to_string(),
            buyer_pubkey: NotePubkey {
                x: [7; 32],
                ed: [8; 32],
            },
            price_per_tick: 1000,
        }
    }

    fn openai(base_url: String) -> UpstreamConfig {
        openai_with_sample_algorithm(base_url, SampleAlgorithm::None)
    }

    fn openai_with_sample_algorithm(
        base_url: String,
        sample_algorithm: SampleAlgorithm,
    ) -> UpstreamConfig {
        UpstreamConfig::OpenAi(OpenAiConfig {
            base_url,
            model: "exact-model".to_string(),
            frame_model: "vendor--exact--v1".to_string(),
            claimed_model_override: None,
            api_key_env: "PATH".to_string(),
            tokenizer_family: "exact".to_string(),
            capabilities: Capabilities {
                max_output_tokens: Some(1024),
                sample_algorithm,
            },
            identity_aliases: Vec::new(),
        })
    }

    async fn http_server(
        status: &'static str,
        body: String,
        delay: Duration,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 8192];
            let _ = socket.read(&mut request).await;
            tokio::time::sleep(delay).await;
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
        (format!("http://{addr}"), task)
    }

    async fn counted_http_server(
        status: &'static str,
        body: String,
        response_count: usize,
    ) -> (
        String,
        Arc<tokio::sync::Notify>,
        tokio::task::JoinHandle<()>,
    ) {
        assert!(response_count > 0, "the server must answer at least once");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response_complete = Arc::new(tokio::sync::Notify::new());
        let task_complete = response_complete.clone();
        let task = tokio::spawn(async move {
            let mut listener = Some(listener);
            for response_index in 0..response_count {
                let (mut socket, _) = listener.as_ref().unwrap().accept().await.unwrap();
                let mut request = vec![0_u8; 8192];
                let _ = socket.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
                if response_index + 1 == response_count {
                    drop(listener.take());
                }
                task_complete.notify_one();
            }
        });
        (format!("http://{addr}"), response_complete, task)
    }

    fn healthy_sse() -> String {
        "data: {\"choices\":[{\"delta\":{\"content\":\"OK\"},\"logprobs\":{\"content\":[{\"token\":\"OK\",\"logprob\":-0.1,\"top_logprobs\":[]}]}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":1,\"total_tokens\":1}}\n\ndata: [DONE]\n\n".to_string()
    }

    async fn read_recorded_request_body(socket: &mut tokio::net::TcpStream) -> String {
        let mut request = Vec::new();
        let mut next = [0_u8; 4096];
        loop {
            let read = socket
                .read(&mut next)
                .await
                .expect("read capability probe request");
            assert_ne!(read, 0, "capability probe request ended before its body");
            request.extend_from_slice(&next[..read]);
            let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") else {
                continue;
            };
            let headers = std::str::from_utf8(&request[..header_end])
                .expect("capability probe request headers are UTF-8");
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length").then(|| {
                        value
                            .trim()
                            .parse::<usize>()
                            .expect("numeric content length")
                    })
                })
                .unwrap_or(0);
            let body_start = header_end + 4;
            if request.len() >= body_start + content_length {
                return String::from_utf8(
                    request[body_start..body_start + content_length].to_vec(),
                )
                .expect("capability probe request body is UTF-8");
            }
        }
    }

    fn recording_http_server(
        script: Vec<String>,
        reject_sample_control: bool,
    ) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        assert!(
            !script.is_empty(),
            "capability probe server needs a response"
        );
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind capability probe upstream");
        listener
            .set_nonblocking(true)
            .expect("capability probe upstream must be non-blocking");
        let address = listener.local_addr().expect("capability probe address");
        let listener =
            tokio::net::TcpListener::from_std(listener).expect("adopt capability probe upstream");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let server = tokio::spawn(async move {
            let mut index = 0_usize;
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let request = read_recorded_request_body(&mut socket).await;
                let sample_control_was_sent = ["seed", "random_seed", "do_sample", "top_k"]
                    .iter()
                    .any(|field| capability_request(&request).get(*field).is_some());
                recorded.lock().unwrap().push(request);
                let scripted_body = &script[std::cmp::min(index, script.len() - 1)];
                index += 1;
                let (status, content_type, body) =
                    if reject_sample_control && sample_control_was_sent {
                        (
                            "400 Bad Request",
                            "application/json",
                            r#"{"error":{"message":"unsupported determinism field"}}"#,
                        )
                    } else {
                        ("200 OK", "text/event-stream", scripted_body.as_str())
                    };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write capability probe response");
            }
        });
        (format!("http://{address}"), requests, server)
    }

    fn capability_plain_sse() -> String {
        "data: {\"model\":\"exact-model\",\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":1,\"total_tokens\":1}}\n\ndata: [DONE]\n\n".to_string()
    }

    fn capability_tool_sse() -> String {
        "data: {\"model\":\"exact-model\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_probe\",\"type\":\"function\",\"function\":{\"name\":\"dexdo_capability_probe\",\"arguments\":\"{}\"}}]}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":4,\"total_tokens\":4}}\n\ndata: [DONE]\n\n".to_string()
    }

    fn capability_wrong_tool_sse() -> String {
        "data: {\"model\":\"exact-model\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_wrong\",\"type\":\"function\",\"function\":{\"name\":\"different_tool\",\"arguments\":\"{}\"}}]}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":4,\"total_tokens\":4}}\n\ndata: [DONE]\n\n".to_string()
    }

    fn capability_think_sse() -> String {
        "data: {\"model\":\"exact-model\",\"choices\":[{\"delta\":{\"reasoning\":\"checked the request\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":4,\"total_tokens\":4,\"completion_tokens_details\":{\"reasoning_tokens\":3}}}\n\ndata: [DONE]\n\n".to_string()
    }

    struct CapabilityStartupResult {
        outcome: Result<SellerStartupOutcome>,
        posts: u64,
        order_ids: Vec<u128>,
        request_bodies: Vec<String>,
    }

    async fn run_capability_startup(
        frame_model: &str,
        first_response: String,
    ) -> CapabilityStartupResult {
        run_capability_startup_with_algorithm(frame_model, first_response, SampleAlgorithm::None)
            .await
    }

    async fn run_capability_startup_with_algorithm(
        frame_model: &str,
        first_response: String,
        sample_algorithm: SampleAlgorithm,
    ) -> CapabilityStartupResult {
        let (base_url, requests, upstream_server) =
            recording_http_server(vec![first_response, capability_plain_sse()], true);
        let mut upstream = openai_with_sample_algorithm(base_url, sample_algorithm);
        let UpstreamConfig::OpenAi(config) = &mut upstream else {
            unreachable!("capability fixture is OpenAI-compatible")
        };
        config.frame_model = frame_model.to_string();
        config.identity_aliases = vec!["exact-model".to_string()];

        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            upstream,
            Arc::new(LocalNote::generate()),
        )
        .await
        .expect("start capability fixture seller");
        let owner = address('7');
        let token_contract = address('8');
        let backend = CancelBackend::new(Vec::new(), owner.clone(), 1227, CancelBehavior::Remove);
        let outcome = prepare_seller_offer_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&token_contract, &seller),
            &owner,
            None,
            std::future::pending(),
            fast_timing(),
        )
        .await;
        let result = CapabilityStartupResult {
            outcome,
            posts: backend.posts.load(Ordering::Relaxed),
            order_ids: backend.order_ids(),
            request_bodies: requests.lock().unwrap().clone(),
        };
        seller.server_task.abort();
        upstream_server.abort();
        result
    }

    fn capability_request(body: &str) -> serde_json::Value {
        serde_json::from_str(body).expect("capability probe request is JSON")
    }

    #[tokio::test]
    async fn issue_1227_tools_flag_with_tool_call_proceeds_to_sell() {
        let result =
            run_capability_startup("vendor--exact--v1--tools", capability_tool_sse()).await;
        let rendered = render_startup(&result.outcome);
        assert!(
            matches!(result.outcome, Ok(SellerStartupOutcome::Ready(_))),
            "a tools-capable upstream must pass startup: {rendered}"
        );
        assert_eq!(result.posts, 1, "capability passed but SELL was not posted");
        assert_eq!(result.order_ids, vec![1227]);
        assert_eq!(
            result.request_bodies.len(),
            2,
            "startup must add no second capability request"
        );
        let capability = capability_request(&result.request_bodies[0]);
        assert_eq!(capability["tools"][0]["type"], "function");
        assert_eq!(
            capability["tools"][0]["function"]["name"],
            "dexdo_capability_probe"
        );
        assert_eq!(
            capability["tool_choice"]["function"]["name"],
            "dexdo_capability_probe"
        );
        // the capability probe's ceiling is the model's OWN declared output cap
        // (`Capabilities { max_output_tokens: Some(1024) }` in this fixture), not a constant of ours.
        assert_eq!(capability["max_tokens"], 1024);
        let after_post = capability_request(&result.request_bodies[1]);
        assert!(after_post.get("tools").is_none());
        assert!(after_post.get("tool_choice").is_none());
    }

    #[tokio::test]
    async fn issue_1227_tools_flag_with_wrong_tool_call_refuses_before_sell() {
        let result =
            run_capability_startup("vendor--exact--v1--tools", capability_wrong_tool_sse()).await;
        let rendered = render_startup(&result.outcome);
        assert!(
            result.outcome.is_err(),
            "a call to an unoffered tool passed --tools: {rendered}"
        );
        assert_eq!(result.posts, 0, "wrong tool call posted a SELL");
        assert!(
            result.order_ids.is_empty(),
            "wrong tool call reached the book"
        );
        assert_eq!(result.request_bodies.len(), 1);
        assert!(rendered.contains("--tools"), "{rendered}");
        assert!(rendered.contains("tool_call=false"), "{rendered}");
    }

    #[tokio::test]
    async fn issue_1227_tools_flag_with_plain_content_refuses_before_sell() {
        let result =
            run_capability_startup("vendor--exact--v1--tools", capability_plain_sse()).await;
        let rendered = render_startup(&result.outcome);
        assert!(
            result.outcome.is_err(),
            "plain content passed --tools: {rendered}"
        );
        assert_eq!(result.posts, 0, "failed --tools preflight posted a SELL");
        assert!(
            result.order_ids.is_empty(),
            "failed --tools preflight reached the book"
        );
        assert_eq!(result.request_bodies.len(), 1);
        for required in ["--tools", "asked", "returned"] {
            assert!(
                rendered.contains(required),
                "--tools refusal must say what was asked and returned: {rendered}"
            );
        }
    }

    #[tokio::test]
    async fn issue_1227_think_flag_with_reasoning_and_usage_proceeds_to_sell() {
        let result =
            run_capability_startup("vendor--exact--v1--think", capability_think_sse()).await;
        let rendered = render_startup(&result.outcome);
        assert!(
            matches!(result.outcome, Ok(SellerStartupOutcome::Ready(_))),
            "a reasoning-capable upstream must pass startup: {rendered}"
        );
        assert_eq!(result.posts, 1, "capability passed but SELL was not posted");
        assert_eq!(result.order_ids, vec![1227]);
        assert_eq!(
            result.request_bodies.len(),
            2,
            "startup must add no second capability request"
        );
        let capability = capability_request(&result.request_bodies[0]);
        assert_eq!(capability["reasoning"]["enabled"], true);
        assert_eq!(capability["reasoning"]["exclude"], false);
        // the capability probe's ceiling is the model's OWN declared output cap
        // (`Capabilities { max_output_tokens: Some(1024) }` in this fixture), not a constant of ours.
        assert_eq!(capability["max_tokens"], 1024);
        let after_post = capability_request(&result.request_bodies[1]);
        assert!(after_post.get("reasoning").is_none());
    }

    #[tokio::test]
    async fn issue_1227_think_flag_with_plain_content_refuses_before_sell() {
        let result =
            run_capability_startup("vendor--exact--v1--think", capability_plain_sse()).await;
        let rendered = render_startup(&result.outcome);
        assert!(
            result.outcome.is_err(),
            "plain content passed --think: {rendered}"
        );
        assert_eq!(result.posts, 0, "failed --think preflight posted a SELL");
        assert!(
            result.order_ids.is_empty(),
            "failed --think preflight reached the book"
        );
        assert_eq!(result.request_bodies.len(), 1);
        for required in ["--think", "asked", "returned"] {
            assert!(
                rendered.contains(required),
                "--think refusal must say what was asked and returned: {rendered}"
            );
        }
    }

    #[tokio::test]
    async fn issue_1227_plain_id_uses_one_readiness_shape_and_no_extra_probe() {
        let result = run_capability_startup("vendor--exact--v1", capability_plain_sse()).await;
        let rendered = render_startup(&result.outcome);
        assert!(
            matches!(result.outcome, Ok(SellerStartupOutcome::Ready(_))),
            "plain startup changed: {rendered}"
        );
        assert_eq!(result.posts, 1);
        assert_eq!(result.order_ids, vec![1227]);
        assert_eq!(
            result.request_bodies.len(),
            2,
            "plain startup gained an extra upstream request"
        );
        assert_eq!(
            result.request_bodies[0], result.request_bodies[1],
            "the pre-SELL and existing post-SELL readiness requests must stay byte-identical"
        );
        assert_eq!(
            capability_request(&result.request_bodies[0]),
            serde_json::json!({
                "model": "exact-model",
                "messages": [{"role": "user", "content": "Reply with OK."}],
                "stream": true,
                "stream_options": {"include_usage": true},
                // the readiness budget moved off 1 -- a model that thinks first spends a
                // one-token budget inside its reasoning channel and delivers nothing, which the
                // seller then reads as billed-without-delivery. What THIS test owns is unchanged and
                // still asserted exactly: the plain path gains no extra request, and the pre-SELL and
                // post-SELL readiness bodies stay byte-identical.
                "max_tokens": 64
            })
        );
    }

    #[tokio::test]
    async fn issue_1951_readiness_ignores_b7_determinism_capability() {
        let result = run_capability_startup_with_algorithm(
            "vendor--exact--v1",
            capability_plain_sse(),
            SampleAlgorithm::Seed,
        )
        .await;
        let rendered = render_startup(&result.outcome);
        assert!(
            matches!(result.outcome, Ok(SellerStartupOutcome::Ready(_))),
            "readiness must not send the B7 determinism field: {rendered}"
        );
        assert_eq!(result.posts, 1, "the accepted profile did not post SELL");
        assert_eq!(result.order_ids, vec![1227]);
        assert_eq!(result.request_bodies.len(), 2);
        for body in &result.request_bodies {
            let request = capability_request(body);
            for field in ["seed", "random_seed", "do_sample", "top_k"] {
                assert!(
                    request.get(field).is_none(),
                    "readiness leaked B7 field {field}: {request}"
                );
            }
        }
    }

    /// pin the CAPABILITY probe body the way
    /// `issue_1227_plain_id_uses_one_readiness_shape_and_no_extra_probe` pins the plain
    /// one, so a future edit cannot silently change what we ask a provider to prove.

    /// The measurement behind these two fields is on the constants themselves. In one sentence:
    /// asked with the readiness prompt and the readiness budget, `qwen/qwen3-32b`,
    /// `qwen/qwen3.6-27b`, `openai/gpt-oss-20b` and `openai/gpt-oss-120b` never called the tool on
    /// Groq (2026-08-12), so a `--tools` market could not start on any of them.

    /// This asserts the WHOLE body, not the two fields that moved: the tool schema and the forced
    /// `tool_choice` are what the provider is actually asked to honour, and they were proven
    /// innocent of precisely by being byte-identical between the failing and passing runs.
    #[tokio::test]
    async fn issue_1278_capability_probe_asks_for_the_call_with_its_own_prompt_and_budget() {
        let result =
            run_capability_startup("vendor--exact--v1--tools", capability_tool_sse()).await;
        assert!(
            matches!(result.outcome, Ok(SellerStartupOutcome::Ready(_))),
            "tools startup changed: {}",
            render_startup(&result.outcome)
        );
        assert_eq!(result.request_bodies.len(), 2);
        assert_eq!(
            capability_request(&result.request_bodies[0]),
            serde_json::json!({
                "model": "exact-model",
                "messages": [{
                    "role": "user",
                    "content": "Call the dexdo_capability_probe tool with an empty object.",
                }],
                "stream": true,
                "stream_options": {"include_usage": true},
                "max_tokens": 1024,
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "dexdo_capability_probe",
                        "description": "Return an empty object to prove tool-call support.",
                        "parameters": {
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false,
                        },
                    },
                }],
                "tool_choice": {
                    "type": "function",
                    "function": {"name": "dexdo_capability_probe"},
                },
            })
        );
        assert_eq!(
            capability_request(&result.request_bodies[0])["messages"][0]["content"],
            serde_json::json!(dexdo_core::params::CAPABILITY_PROBE_PROMPT),
            "the pinned body must be the shipped constant, not a copy of its text"
        );
        assert_eq!(
            capability_request(&result.request_bodies[0])["max_tokens"],
            serde_json::json!(1024),
            "the pinned budget must be the model's own declared output cap"
        );
    }

    /// the capability prompt and budget are the CAPABILITY probe's alone. The post-SELL
    /// readiness request in the very same tools startup must still carry the readiness prompt and
    /// the readiness budget, byte for byte, with no tool fields at all.

    /// Two questions, two constants: is what asking one of them with the other's budget cost,
    /// and this is the assertion that fails if's fix leaks back into the shared path.
    #[tokio::test]
    async fn issue_1278_plain_readiness_body_stays_separate_inside_a_tools_startup() {
        let result =
            run_capability_startup("vendor--exact--v1--tools", capability_tool_sse()).await;
        assert_eq!(result.request_bodies.len(), 2);
        assert_eq!(
            capability_request(&result.request_bodies[1]),
            serde_json::json!({
                "model": "exact-model",
                "messages": [{"role": "user", "content": "Reply with OK."}],
                "stream": true,
                "stream_options": {"include_usage": true},
                "max_tokens": 64
            }),
            "the plain readiness request must stay free of capability-only fields"
        );
        assert_ne!(
            capability_request(&result.request_bodies[0])["max_tokens"],
            capability_request(&result.request_bodies[1])["max_tokens"],
            "the two probes must not share a budget"
        );
        assert_ne!(
            dexdo_core::params::CAPABILITY_PROBE_PROMPT,
            dexdo_core::params::UPSTREAM_HEALTH_PROBE_PROMPT,
            "the two probes must not share a prompt"
        );
    }

    /// a `--think`-only probe must not name a tool it does not offer.

    /// `tools`/`tool_choice` are built from `requirements.tools` alone, so this body carries
    /// `reasoning` and NO tool of any kind. Asking it to "call the dexdo_capability_probe tool"
    /// would be an incoherent request, and a provider entitled to refuse it would make `--think`
    /// markets unstartable -- the same class of outage is fixing. So the prompt is chosen by
    /// whether a tool is OFFERED, and this pins both halves of that choice: the readiness prompt
    /// here, the capability prompt in
    /// `issue_1278_capability_probe_asks_for_the_call_with_its_own_prompt_and_budget`.

    /// The budget is still the capability one: reasoning needs the room even when no tool is asked
    /// for.
    #[tokio::test]
    async fn issue_1278_think_only_probe_keeps_the_readiness_prompt_and_offers_no_tool() {
        let result =
            run_capability_startup("vendor--exact--v1--think", capability_think_sse()).await;
        assert!(
            matches!(result.outcome, Ok(SellerStartupOutcome::Ready(_))),
            "think startup changed: {}",
            render_startup(&result.outcome)
        );
        assert_eq!(result.request_bodies.len(), 2);
        assert_eq!(
            capability_request(&result.request_bodies[0]),
            serde_json::json!({
                "model": "exact-model",
                "messages": [{"role": "user", "content": "Reply with OK."}],
                "stream": true,
                "stream_options": {"include_usage": true},
                "max_tokens": 1024,
                "reasoning": {"enabled": true, "exclude": false},
            }),
            "a --think-only probe must ask for reasoning and offer no tool"
        );
        let probe = capability_request(&result.request_bodies[0]);
        assert!(
            probe.get("tools").is_none() && probe.get("tool_choice").is_none(),
            "no tool is offered, so none may be named"
        );
        assert!(
            !probe["messages"][0]["content"]
                .as_str()
                .expect("probe prompt is a string")
                .contains("dexdo_capability_probe"),
            "the prompt must not name a tool absent from the request"
        );
        assert_eq!(
            probe["messages"][0]["content"],
            serde_json::json!(dexdo_core::params::UPSTREAM_HEALTH_PROBE_PROMPT),
            "the think-only prompt must be the shipped readiness constant"
        );
    }

    #[derive(Clone)]
    struct RejectWithStatus(tonic::Status);

    impl tonic::service::Interceptor for RejectWithStatus {
        fn call(
            &mut self,
            _request: tonic::Request<()>,
        ) -> Result<tonic::Request<()>, tonic::Status> {
            Err(self.0.clone())
        }
    }

    async fn status_seller(status: tonic::Status) -> (RunningSeller, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the local status gateway");
        let advertised = listener.local_addr().unwrap().to_string();
        let listen_addr = listener.local_addr().unwrap();
        let tls = crate::seller::tls::GatewayTls::generate().unwrap();
        crate::seller::tls::ensure_crypto_provider();
        let tls_fingerprint = tls.fingerprint.clone();
        let identity = tonic::transport::Identity::from_pem(tls.cert_pem, tls.key_pem);
        let tls_config = tonic::transport::ServerTlsConfig::new().identity(identity);
        let state = Arc::new(crate::seller::gateway::GatewayState::new());
        let service = crate::seller::gateway::GatewayService::new(state.clone());
        let intercepted =
            dexdo_proto::GatewayServer::with_interceptor(service, RejectWithStatus(status));
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let mut builder = tonic::transport::Server::builder()
            .tls_config(tls_config)
            .expect("configure status gateway TLS");
        let task = tokio::spawn(async move {
            if let Err(error) = builder
                .add_service(intercepted)
                .serve_with_incoming(incoming)
                .await
            {
                panic!("status gateway stopped: {error}");
            }
        });
        (
            RunningSeller {
                state,
                note: Arc::new(LocalNote::generate()),
                server_task: task,
                listen_addr,
                tls_fingerprint,
            },
            advertised,
        )
    }

    /// the cursor used to be written straight into the shared temp directory under a
    /// `<pid>-<seconds>` name and was never removed -- 38 files per workspace run, measured. The
    /// directory is returned with it and must be held for as long as the cursor is read or written.
    fn watch(name: &str) -> (tempfile::TempDir, SellerMatchWatchConfig) {
        let dir = tempfile::Builder::new()
            .prefix(name)
            .tempdir()
            .expect("match watch cursor directory");
        let cursor_path = dir.path().join("cursor.json");
        (
            dir,
            SellerMatchWatchConfig {
                cursor_path,
                poll_interval: Duration::from_millis(50),
            },
        )
    }

    fn fast_timing() -> SupervisionTiming {
        SupervisionTiming {
            health_interval: Duration::from_millis(5),
            health_timeout: Duration::from_millis(500),
            cycle_timeout: Duration::from_millis(600),
            cancel_poll: Duration::from_millis(1),
            expiry_poll: Duration::from_secs(3_600),
            abort_gateway_on_stop: true,
            advertise_probe: AdvertiseProbePolicy::default(),
            reap_timeout: Duration::from_millis(400),
            reap_poll: Duration::from_millis(1),
        }
    }

    #[tokio::test]
    async fn unreachable_advertised_gateway_fails_before_any_sell_post() {
        // The address has to be genuinely REFUSED (the assertions below pin the probe to
        // `stage: tcp_connect`), and it has to STAY refused: the reservation is held for the whole
        // assertion, because a bind-and-drop hands the port back to the kernel.
        let (_unavailable_hold, unavailable) = crate::test_refusing_endpoint::refusing_endpoint();
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        // The budget has to outlast the platform's own refusal latency, or the row reads
        // `handshake_timeout` and never sees the stage it is about to assert. Measured connecting
        // to a held port: Linux and macOS refuse in under a millisecond, `windows-latest` takes
        // 2.031 s. A hundred milliseconds is right for the first two and is always short on the
        // third; five seconds there costs this one row two seconds and keeps it running.
        let probe_budget = if cfg!(windows) {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(100)
        };
        let failure = check_readiness(
            &seller,
            &unavailable,
            probe_budget,
            None,
            &address('b'),
            AdvertiseProbePolicy::default(),
        )
        .await
        .expect_err("unreachable advertised endpoint");

        assert_eq!(failure.component, HealthComponent::AdvertisedGateway);
        // the component error names the probed ADDRESS, the failing STAGE and the underlying
        // cause, instead of the bare `transport error` that cost hours of `rustls=debug`.
        let detail = failure
            .into_startup_error(&unavailable.to_string())
            .to_string();
        assert!(
            detail.starts_with("error[E_ADVERTISE_UNREACHABLE] (network): advertised gateway "),
            "{detail}"
        );
        assert!(detail.contains(&unavailable), "{detail}");
        // The explicit staging reaches `tcp_connect` on a closed port; sniffing the chain could
        // only ever have guessed `tls_handshake` here.
        assert!(detail.contains("(stage: tcp_connect)"), "{detail}");
        assert!(
            detail.contains("\n  cause: advertised gateway self-probe failed at tcp_connect"),
            "{detail}"
        );
        assert!(detail.contains("\n  hint: "), "{detail}");
        // 's tolerance does NOT apply to a non-public advertise, and the hint says so instead
        // of offering the tunnel excuse.
        assert!(
            detail.contains("the advertised address is not public"),
            "{detail}"
        );
        assert!(
            !detail.contains("a remote buyer connects fine"),
            "the tolerant hint must not be offered for a non-public advertise: {detail}"
        );
        seller.server_task.abort();
    }

    /// A seller whose advertised public address refuses the trial delivery does not become ready
    /// and posts no offer to the book.

    /// E2E-ADV-10, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ADV-10/L0
    /// NOT RUN ON WINDOWS. The alias below is a NAME, not a literal -- `127.1` is what makes
    /// `classify_advertise` take its name branch and answer PUBLIC -- so the probe has to resolve it,
    /// and Windows will not. Measured on `windows-latest`: `getaddrinfo("127.1")` fails outright
    /// (`11001`), as do `127.0.1`, `127.000.000.001` and `0x7f000001`; only the full `127.0.0.1`
    /// resolves, and that spelling classifies as loopback, which is the opposite of what this row
    /// needs. No name exists that Windows resolves to loopback AND `classify_name` calls public:
    /// the only such name is `localhost`, which that function lists as non-public by design.

    /// A second measured obstacle stands behind the first: on `windows-latest` even a refused
    /// connect returns after 2.031 s, against this fixture's 300 ms probe budget, so the row would
    /// read `handshake_timeout` whatever spelling it used.

    /// What the row proves -- our own classification and verdict logic -- has nothing platform-bound
    /// in it and is proven on Linux and macOS. Skipping is honest here; a permanently red leg is
    /// not, because it hides the Windows regressions this suite could still catch.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn public_advertise_transport_failure_is_fatal_and_the_book_stays_empty() {
        // A held refusing socket, advertised under its public alias: the connection is REFUSED,
        // deterministically, on an address the classifier calls public.
        let (_refusing_hold, refused) = crate::test_refusing_endpoint::refusing_endpoint();
        let public_unreachable = public_alias(refused.parse().expect("refusing endpoint address"));
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let owner = address('f');
        let tc = address('9');
        let backend = CancelBackend::new(Vec::new(), owner.clone(), 77, CancelBehavior::Remove);
        let mut config = cfg(&tc);
        config.gateway_advertise.clone_from(&public_unreachable);

        let outcome = prepare_seller_offer_with_timing(
            &seller,
            &backend,
            &config,
            &owner,
            None,
            std::future::pending(),
            fatal_preflight_timing(AdvertiseProbePolicy::default()),
        )
        .await;
        let rendered = render_startup(&outcome);

        // The book facts are asserted FIRST and UNCONDITIONALLY, before the shape of the refusal is
        // examined at all: this head rests the offer and cancels it on a later health cycle, and a
        // rested-then-cancelled offer is still matchable inside that window.
        if backend.posts.load(Ordering::Relaxed) != 0 {
            panic!("E2E-ADV-10A public advertise transport refusal posted an offer");
        }
        assert_nothing_was_posted(
            &backend,
            "public advertise, transport probe failure",
            &rendered,
        );
        assert!(
            outcome.is_err(),
            "the preflight must REFUSE, not proceed and clean up afterwards: {rendered}"
        );
        assert!(
            rendered.contains("error[E_ADVERTISE_UNREACHABLE] (network)")
                && rendered.contains(&public_unreachable),
            "{rendered}"
        );
        // The STAGE is asserted, which is what distinguishes this arm from the timeout arm: a
        // refused connection reaches `tcp_connect`, a stalled one reaches `handshake_timeout`.
        // Without this the transport arm and the timeout arm are the same test twice.
        assert!(
            rendered.contains("(stage: tcp_connect)"),
            "this row covers the TRANSPORT arm, not the timeout arm: {rendered}"
        );
        let error = rendered;
        // ADV-05's second half: the operator-facing text must stop promising the cancelled
        // tolerance. A row that changes behaviour and leaves the string is half a fix.
        assert!(
            !error.contains("posted anyway"),
            "the refusal still promises the cancelled tolerance: {error}"
        );
        assert!(
            !error.contains("--require-advertise-probe"),
            "the refusal still offers a flag whose behaviour is now universal: {error}"
        );
        seller.server_task.abort();
    }

    /// Assert that a seller whose preflight failed posted no offer and opened no stream.

    /// Shared by E2E-ADV-10, E2E-ADV-11 and E2E-ADV-12, `tests/e2e/test-specification.md`.

    /// Partial: the buyer-escrow half of those rows is not observable at this layer -- no buyer
    /// note exists in these fixtures -- so only the seller-side facts are asserted.
    fn assert_nothing_was_posted(backend: &CancelBackend, label: &str, error: &str) {
        // 1 -- `postSellOffer` was not called, on the submit path rather than inferred.
        assert_eq!(
            backend.posts.load(Ordering::Relaxed),
            0,
            "{label}: postSellOffer was called behind a failed preflight: {error}"
        );
        // 2 -- the book has no order for this TokenContract.
        assert!(
            backend.order_ids().is_empty(),
            "{label}: an undeliverable offer rests in the book: {:?}",
            backend.order_ids()
        );
        // 3 -- the seller never opened a stream, which is the seller-side money move: `open_stream`
        // posts the `2P` bond and freezes the probe tick.

        // `CancelBackend.opens` counts `open_stream` -- the SELLER's call, not the buyer's escrow.
        // The buyer-escrow half of the row is owed at L1, where a real buyer's note can be read.
        assert_eq!(
            backend.opens.load(Ordering::Relaxed),
            0,
            "{label}: the seller opened a stream behind a failed preflight: {error}"
        );
    }

    /// Re-spell a loopback `127.0.0.1:<port>` as `127.1:<port>`, which the classifier calls PUBLIC
    /// while the real dial still lands on the held local socket.

    /// `advertise_host` splits the single colon and hands `127.1` to
    /// `classify_advertise` (`advertise.rs:143`); Rust's `IpAddr` parser requires four octets, so
    /// `"127.1".parse::<IpAddr>()` FAILS and the string falls through to `classify_name`, which
    /// calls anything outside its reserved-local list public. The dial is a different code path:
    /// `TcpStream::connect` hands the string to `getaddrinfo`, whose `inet_aton` still accepts the
    /// classic BSD abbreviated form and yields `127.0.0.1`.

    /// So the PUBLIC address class becomes controllable, and every case that needs "public AND
    /// something I own is on the other end" is deterministic instead of depending on whether the
    /// host has a default route to TEST-NET-1.
    fn public_alias(addr: std::net::SocketAddr) -> String {
        assert!(addr.ip().is_loopback(), "the alias only re-spells loopback");
        let advertised = format!("127.1:{}", addr.port());
        assert!(
            crate::seller::advertise::advertise_is_public(&advertised),
            "fixture guard: {advertised} must classify PUBLIC, or the case proves the private \
             branch instead"
        );
        advertised
    }

    /// An address that ACCEPTS the connection and then never speaks, for as long as the listener is
    /// held -- the deterministic probe TIMEOUT.

    /// The counterpart of `refusing_endpoint`, and its own doc explains why this works: `listen(2)`
    /// completes the TCP handshake out of the backlog, so `connect` succeeds even though nothing
    /// ever calls `accept`. Stage 1 of `probe_advertised_gateway` therefore passes and stage 2
    /// hangs waiting for a TLS ServerHello that never comes, until the bounded deadline at
    /// `liveness.rs:461` elapses and the timeout branch at `:467-476` builds the fault.
    async fn stalling_endpoint() -> (tokio::net::TcpListener, std::net::SocketAddr) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a listener that never speaks");
        let addr = listener.local_addr().expect("bound address");
        (listener, addr)
    }

    /// Render a startup outcome for an assertion message, whichever arm it took.

    /// A refused preflight can surface two ways: as `Err`, or -- on a head that still tolerates the
    /// failure -- as `Ok(Stopped {.. })` after the offer rested and a later health cycle cancelled
    /// it. Both must be printable, because the book assertion runs before either is ruled out.
    fn render_startup(outcome: &Result<SellerStartupOutcome>) -> String {
        match outcome {
            Ok(startup) => format!("{startup:?}"),
            Err(error) => error.to_string(),
        }
    }

    fn fatal_preflight_timing(advertise_probe: AdvertiseProbePolicy) -> SupervisionTiming {
        SupervisionTiming {
            health_interval: Duration::from_millis(5),
            health_timeout: Duration::from_millis(300),
            cycle_timeout: Duration::from_secs(3),
            cancel_poll: Duration::from_millis(1),
            expiry_poll: Duration::from_secs(3_600),
            abort_gateway_on_stop: true,
            advertise_probe,
            reap_timeout: Duration::from_millis(400),
            reap_poll: Duration::from_millis(1),
        }
    }

    /// E2E-ADV-10, the timeout arm. Predecessor:
    /// `pr795_edge_tolerated_public_probe_timeout_keeps_healthy_upstream_ready_and_posts`, which
    /// asserted `Ok` plus `posts == 1` on the same input.

    /// A HEALTHY upstream is deliberately kept in the fixture: the point of the predecessor was that
    /// a tolerated probe timeout must not starve the exact-model check, and that half of the
    /// reasoning survives the ruling. What changes is the verdict -- a healthy model is not a licence
    /// to advertise an address nobody can dial.

    /// It drives the production readiness->post sequence, so the verification is the BOOK and the
    /// SUBMIT PATH. An earlier revision stopped at `check_readiness_with_probe` and asserted only
    /// the returned error, which could have passed while the posting path still wrote an order.

    /// This covers the TRANSPORT-fault arm. The TIMEOUT arm is a separate construction site in
    /// production (`liveness.rs:467-476` against `:466`) and gets its own test below, which asserts
    /// the classification and the book together.

    /// E2E-ADV-10, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ADV-10/L0
    /// NOT RUN ON WINDOWS. The alias below is a NAME, not a literal -- `127.1` is what makes
    /// `classify_advertise` take its name branch and answer PUBLIC -- so the probe has to resolve it,
    /// and Windows will not. Measured on `windows-latest`: `getaddrinfo("127.1")` fails outright
    /// (`11001`), as do `127.0.1`, `127.000.000.001` and `0x7f000001`; only the full `127.0.0.1`
    /// resolves, and that spelling classifies as loopback, which is the opposite of what this row
    /// needs. No name exists that Windows resolves to loopback AND `classify_name` calls public:
    /// the only such name is `localhost`, which that function lists as non-public by design.

    /// A second measured obstacle stands behind the first: on `windows-latest` even a refused
    /// connect returns after 2.031 s, against this fixture's 300 ms probe budget, so the row would
    /// read `handshake_timeout` whatever spelling it used.

    /// What the row proves -- our own classification and verdict logic -- has nothing platform-bound
    /// in it and is proven on Linux and macOS. Skipping is honest here; a permanently red leg is
    /// not, because it hides the Windows regressions this suite could still catch.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn a_public_transport_probe_failure_is_fatal_even_with_a_healthy_upstream() {
        let (_refusing_hold, refused) = crate::test_refusing_endpoint::refusing_endpoint();
        let public_unreachable = public_alias(refused.parse().expect("refusing endpoint address"));

        let (base_url, upstream_server) =
            http_server("200 OK", healthy_sse(), Duration::from_millis(10)).await;
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            openai(base_url),
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let owner = address('6');
        let tc = address('7');
        let backend = CancelBackend::new(Vec::new(), owner.clone(), 78, CancelBehavior::Remove);
        let mut config = cfg(&tc);
        config.gateway_advertise.clone_from(&public_unreachable);

        let outcome = prepare_seller_offer_with_timing(
            &seller,
            &backend,
            &config,
            &owner,
            None,
            std::future::pending(),
            fatal_preflight_timing(AdvertiseProbePolicy::default()),
        )
        .await;
        let rendered = render_startup(&outcome);
        if backend.posts.load(Ordering::Relaxed) != 0 {
            panic!("E2E-ADV-10B healthy upstream did not make unreachable advertise fatal");
        }
        assert_nothing_was_posted(
            &backend,
            "healthy upstream, unreachable public advertise",
            &rendered,
        );
        assert!(
            outcome.is_err(),
            "a healthy upstream is not a licence to advertise an undialable address: {rendered}"
        );
        assert!(
            rendered.contains("error[E_ADVERTISE_UNREACHABLE] (network)")
                && rendered.contains(&public_unreachable)
                && rendered.contains("(stage: tcp_connect)"),
            "{rendered}"
        );
        seller.server_task.abort();
        upstream_server.await.unwrap();
    }

    /// E2E-ADV-10, the TIMEOUT arm -- the classification and the book, through production's own
    /// composition.

    /// Predecessor: `pr795_edge_tolerated_public_probe_timeout_keeps_healthy_upstream_ready_and_posts`,
    /// which asserted `Ok` plus `posts == 1`. A HEALTHY upstream is kept because the predecessor's
    /// point -- a tolerated probe timeout must not starve the exact-model check -- survives the
    /// ruling; what changes is the verdict.

    /// **Three earlier revisions of this row were wrong, and the last of them was wrong in the
    /// direction that is harder to notice.** It asserted only the returned error; then it
    /// re-implemented `if readiness.is_ok() { post }` in the test, which is the TEST's composition
    /// and would miss a timeout-specific bypass inside the real wrapper; then it CONCEDED the book
    /// as unreachable, on the grounds that no address both classifies PUBLIC and hangs
    /// deterministically. That concession was measured rather than assumed and was still false --
    /// `public_alias` is the counter-example, and an honest `partial` over a closeable gap is its
    /// own kind of wrong answer.

    /// So this drives `prepare_seller_offer_with_timing` -- production's composition, readiness at
    /// `liveness.rs:999`, the no-post return at `:1020`, the post at `:1047` -- against a listener
    /// that accepts and never speaks TLS, advertised under its public alias. The probe reaches
    /// stage 2 and hangs there until the bounded deadline elapses, so the timeout branch at
    /// `:467-476` is the one that builds the fault, and the stage assertion below is what proves
    /// it rather than the transport branch at `:466`.

    /// E2E-ADV-10, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ADV-10/L0
    /// NOT RUN ON WINDOWS. The alias below is a NAME, not a literal -- `127.1` is what makes
    /// `classify_advertise` take its name branch and answer PUBLIC -- so the probe has to resolve it,
    /// and Windows will not. Measured on `windows-latest`: `getaddrinfo("127.1")` fails outright
    /// (`11001`), as do `127.0.1`, `127.000.000.001` and `0x7f000001`; only the full `127.0.0.1`
    /// resolves, and that spelling classifies as loopback, which is the opposite of what this row
    /// needs. No name exists that Windows resolves to loopback AND `classify_name` calls public:
    /// the only such name is `localhost`, which that function lists as non-public by design.

    /// A second measured obstacle stands behind the first: on `windows-latest` even a refused
    /// connect returns after 2.031 s, against this fixture's 300 ms probe budget, so the row would
    /// read `handshake_timeout` whatever spelling it used.

    /// What the row proves -- our own classification and verdict logic -- has nothing platform-bound
    /// in it and is proven on Linux and macOS. Skipping is honest here; a permanently red leg is
    /// not, because it hides the Windows regressions this suite could still catch.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn a_public_probe_timeout_is_fatal_and_the_book_stays_empty() {
        // Held for the whole assertion: the listener is what makes the connect succeed and the
        // TLS handshake hang. Dropping it would turn this into the transport-refusal case.
        let (_stalling_hold, stalled) = stalling_endpoint().await;
        let advertised = public_alias(stalled);

        let (base_url, upstream_server) =
            http_server("200 OK", healthy_sse(), Duration::from_millis(10)).await;
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            openai(base_url),
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let owner = address('8');
        let tc = address('7');
        let backend = CancelBackend::new(Vec::new(), owner.clone(), 79, CancelBehavior::Remove);
        let mut config = cfg(&tc);
        config.gateway_advertise.clone_from(&advertised);

        let outcome = prepare_seller_offer_with_timing(
            &seller,
            &backend,
            &config,
            &owner,
            None,
            std::future::pending(),
            fatal_preflight_timing(AdvertiseProbePolicy::default()),
        )
        .await;
        seller.server_task.abort();
        upstream_server.await.unwrap();

        let rendered = render_startup(&outcome);
        if backend.posts.load(Ordering::Relaxed) != 0 {
            panic!("E2E-ADV-10C public advertise timeout posted an offer");
        }
        assert_nothing_was_posted(&backend, "public advertise, probe TIMEOUT", &rendered);
        assert!(
            outcome.is_err(),
            "the preflight must REFUSE on a timed-out probe: {rendered}"
        );
        assert!(
            rendered.contains("error[E_ADVERTISE_UNREACHABLE] (network)")
                && rendered.contains(&advertised),
            "{rendered}"
        );
        // This is the row's own arm: `handshake_timeout` is built only by the timeout branch
        // (`:467-476`). Without this the case is indistinguishable from the transport rows and a
        // timeout-only bypass would pass between them.
        assert!(
            rendered.contains("(stage: handshake_timeout)"),
            "the probe did not TIME OUT -- this row does not cover the fault it names: {rendered}"
        );
    }

    /// E2E-ADV-03 -- the check is on the ADVERTISED `host:port`, and a successful BIND is not it.

    /// The gateway bind at `cli/seller.rs:2285-2314` proves something is listening on the LISTEN
    /// address. On a listen/advertise mismatch -- the common operator error -- the bind succeeds
    /// while the pair a buyer will dial is closed. This drives the real readiness->post sequence
    /// with a gateway genuinely bound and serving, and a genuinely refused advertised port, and
    /// asserts that nothing was posted.

    /// GREEN on this head, for a reason worth stating: tolerance is gated on
    /// `advertise_is_public`, so a private mismatch is already fatal. The PUBLIC mismatch is the
    /// same operator error one address class over and is NOT green -- that case is
    /// `public_advertise_transport_failure_is_fatal_and_the_book_stays_empty`, which is ignored
    /// until the ruling lands. Read the two together; either alone understates the gap.

    /// E2E-ADV-03, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ADV-03/L0
    #[tokio::test]
    async fn a_closed_advertised_port_posts_nothing_even_though_the_gateway_is_bound() {
        // Held for the whole assertion: a bind-and-drop hands the port back to the kernel and the
        // case would stop proving its own name.
        let (_refusing_hold, closed_advertise) = crate::test_refusing_endpoint::refusing_endpoint();
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        // The bind really did succeed, and on a DIFFERENT port from the advertised one. Without
        // this the test could pass with nothing listening anywhere, which is a different row.
        assert_ne!(seller.listen_addr.to_string(), closed_advertise);
        tokio::net::TcpStream::connect(seller.listen_addr)
            .await
            .expect("the gateway is bound and accepting on its LISTEN address");

        let owner = address('4');
        let tc = address('5');
        let backend = CancelBackend::new(Vec::new(), owner.clone(), 91, CancelBehavior::Remove);
        let mut config = cfg(&tc);
        config.gateway_advertise.clone_from(&closed_advertise);

        let error = prepare_seller_offer_with_timing(
            &seller,
            &backend,
            &config,
            &owner,
            None,
            std::future::pending(),
            SupervisionTiming {
                health_interval: Duration::from_millis(5),
                health_timeout: Duration::from_millis(500),
                cycle_timeout: Duration::from_secs(2),
                cancel_poll: Duration::from_millis(1),
                expiry_poll: Duration::from_secs(3_600),
                abort_gateway_on_stop: true,
                advertise_probe: AdvertiseProbePolicy::default(),
                ..fast_timing()
            },
        )
        .await
        .expect_err("a closed advertised port must stop the SELL even with the gateway bound")
        .to_string();

        assert_eq!(
            backend.posts.load(Ordering::Relaxed),
            0,
            "postSellOffer was called for an address no buyer can dial: {error}"
        );
        assert!(
            backend.order_ids().is_empty(),
            "an undialable offer rests in the book: {:?}",
            backend.order_ids()
        );
        assert_eq!(backend.opens.load(Ordering::Relaxed), 0);
        assert!(
            error.contains("error[E_ADVERTISE_UNREACHABLE] (network)")
                && error.contains(&closed_advertise),
            "the refusal must name the ADVERTISED address, not the bound one: {error}"
        );
        assert!(
            !error.contains(&seller.listen_addr.to_string()),
            "the refusal named the listen address, which is not what a buyer dials: {error}"
        );
        seller.server_task.abort();
    }

    /// E2E-ADV-11 and E2E-ADV-12 -- a wrong-endpoint PROOF stops the post under every
    /// policy, and stays so after the ruling.

    /// This is the half of the old `probe_should_degrade` predicate that had to SURVIVE the
    /// deletion of the degrade path: its `&& !fault.wrong_endpoint` conjunct. deleted the
    /// predicate and the arm it guarded, so a wrong-endpoint fault is now fatal the same way every
    /// other probe fault is, rather than by an exemption written into a tolerance rule. It is
    /// written against `prepare_seller_offer_with_timing` -- the production readiness->post sequence
    /// -- so that removing the predicate could not remove the guarantee with it, AND so that the
    /// verification is the posted-offer facts rather than the value of a returned error.

    /// **Both faults are produced by the real probe against a real counterparty**, never injected.
    /// An earlier revision handed `check_readiness_with_probe` an already-constructed
    /// `ProbeFault::wrong_endpoint`, which asserted that such a fault is fatal ONCE ONE EXISTS and
    /// proved nothing about whether the probe still detects one. Here:

    /// - ADV-11 (pinned-certificate mismatch) is a second real gateway with its own TLS identity,
    /// so detection runs through `connect_pinned` and the rustls verifier;
    /// - ADV-12 (something answering that is not this gateway) is a real gRPC server returning an
    /// application status, so detection runs through the challenge exchange.

    /// **Both dimensions are swept, and the address class is no longer conceded.** An earlier
    /// revision said a PUBLIC address answering with the wrong endpoint could not be produced from
    /// a test host, and marked the rows `partial` on it. `public_alias` refutes that: the
    /// counterparty stays local and the advertised spelling is what the classifier judges, so a
    /// public advertised address is exercised against a real wrong-endpoint proof.

    /// GREEN on this head. A regression, not a specification: the code PR it was written to catch
    /// -- one deleting the tolerance and taking the wrong-endpoint guarantee with it -- is, and
    /// this test stayed green through it. What it catches now is any future PR that reintroduces a
    /// tolerated path and exempts a wrong-endpoint fault from it.

    /// E2E-ADV-11, E2E-ADV-12, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ADV-11/L0
    /// E2E-ROW: E2E-ADV-12/L0
    /// NOT RUN ON WINDOWS. The alias below is a NAME, not a literal -- `127.1` is what makes
    /// `classify_advertise` take its name branch and answer PUBLIC -- so the probe has to resolve it,
    /// and Windows will not. Measured on `windows-latest`: `getaddrinfo("127.1")` fails outright
    /// (`11001`), as do `127.0.1`, `127.000.000.001` and `0x7f000001`; only the full `127.0.0.1`
    /// resolves, and that spelling classifies as loopback, which is the opposite of what this row
    /// needs. No name exists that Windows resolves to loopback AND `classify_name` calls public:
    /// the only such name is `localhost`, which that function lists as non-public by design.

    /// A second measured obstacle stands behind the first: on `windows-latest` even a refused
    /// connect returns after 2.031 s, against this fixture's 300 ms probe budget, so the row would
    /// read `handshake_timeout` whatever spelling it used.

    /// What the row proves -- our own classification and verdict logic -- has nothing platform-bound
    /// in it and is proven on Linux and macOS. Skipping is honest here; a permanently red leg is
    /// not, because it hides the Windows regressions this suite could still catch.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn a_real_wrong_endpoint_proof_stops_the_post_under_every_policy() {
        let mut index = 0_u128;
        for policy in [
            AdvertiseProbePolicy::TolerateTunneledTransportFailure,
            AdvertiseProbePolicy::Required,
        ] {
            for class in ["private", "public"] {
                index += 1;
                // ---- ADV-11: a real foreign TLS identity on the advertised address ----
                let seller = super::super::start_gateway_with_note(
                    "127.0.0.1:0".parse().unwrap(),
                    UpstreamConfig::Mock,
                    Arc::new(LocalNote::generate()),
                )
                .await
                .unwrap();
                let foreign = super::super::start_gateway_with_note(
                    "127.0.0.1:0".parse().unwrap(),
                    UpstreamConfig::Mock,
                    Arc::new(LocalNote::generate()),
                )
                .await
                .unwrap();
                assert_ne!(
                    seller.tls_fingerprint, foreign.tls_fingerprint,
                    "fixture guard: the pin mismatch must be real"
                );
                let advertised = match class {
                    "private" => foreign.listen_addr.to_string(),
                    _ => public_alias(foreign.listen_addr),
                };
                let owner = address('1');
                let backend = CancelBackend::new(
                    Vec::new(),
                    owner.clone(),
                    300 + index,
                    CancelBehavior::Remove,
                );
                let mut config = cfg(&address('2'));
                config.gateway_advertise.clone_from(&advertised);
                let label = format!("ADV-11 pin mismatch on a {class} advertise under {policy:?}");

                let error = prepare_seller_offer_with_timing(
                    &seller,
                    &backend,
                    &config,
                    &owner,
                    None,
                    std::future::pending(),
                    fatal_preflight_timing(policy),
                )
                .await
                .expect_err("a pinned-certificate mismatch must stop the SELL under every policy")
                .to_string();
                assert_nothing_was_posted(&backend, &label, &error);
                assert!(
                    error.contains("error[E_ADVERTISE_WRONG_GATEWAY] (tls)")
                        && error.contains("stage: tls_certificate_pin"),
                    "{label}: {error}"
                );
                seller.server_task.abort();
                foreign.server_task.abort();

                // ---- ADV-12: something ANSWERS on the advertised address and is not this gateway ----

                // The probe must reach `grpc_challenge`, so the TLS pin has to SUCCEED first: a
                // separately generated seller would fail at `tls_certificate_pin` (`liveness.rs:351`)
                // and the case would silently become a second copy of ADV-11 above. So readiness is
                // run for the impostor's OWN identity -- the pin matches, the connection completes, and
                // the interceptor returns a server-side gRPC status, which is what
                // `probe_advertised_gateway` classifies as a wrong endpoint at `:392`.
                let (seller, listen) = status_seller(tonic::Status::new(
                    tonic::Code::PermissionDenied,
                    "a foreign service holds the advertised address",
                ))
                .await;
                let advertised = match class {
                    "private" => listen.clone(),
                    _ => public_alias(listen.parse().expect("impostor address")),
                };
                let owner = address('3');
                let backend = CancelBackend::new(
                    Vec::new(),
                    owner.clone(),
                    310 + index,
                    CancelBehavior::Remove,
                );
                let mut config = cfg(&address('4'));
                config.gateway_advertise.clone_from(&advertised);
                let label =
                    format!("ADV-12 foreign gateway on a {class} advertise under {policy:?}");

                let error = prepare_seller_offer_with_timing(
                    &seller,
                    &backend,
                    &config,
                    &owner,
                    None,
                    std::future::pending(),
                    fatal_preflight_timing(policy),
                )
                .await
                .expect_err("a foreign gateway on the advertised address must stop the SELL")
                .to_string();
                assert_nothing_was_posted(&backend, &label, &error);
                // The STAGE is asserted, not just the code: without it this case passes on the TLS
                // pin failure that ADV-11 already covers, and the gRPC challenge is never exercised.
                assert!(
                    error.contains("error[E_ADVERTISE_WRONG_GATEWAY] (tls)")
                        && error.contains("stage: grpc_challenge"),
                    "{label}: the probe never reached the gRPC challenge: {error}"
                );
                seller.server_task.abort();
            }
        }
    }

    /// E2E-ADV-12 -- **every** server-returned gRPC status is a wrong-endpoint proof, on both
    /// address classes.

    /// Restores `pr795_edge_server_returned_grpc_application_statuses_are_fatal_before_sell`, which
    /// an earlier revision of this file deleted while replacing the area. That test swept five
    /// status classes; production classifies every returned status identically at
    /// `liveness.rs:392` -- *"a server-returned gRPC status proves the connection completed"* -- so
    /// the sweep is what stops a later refactor special-casing one of them into a tolerated
    /// transport fault. Losing it was a regression, and the narrowed replacement covered only
    /// `PermissionDenied`.

    /// Two things are added on top of the restored sweep rather than replacing it: readiness runs
    /// for the impostor's OWN identity so the TLS pin SUCCEEDS and the probe genuinely reaches
    /// `grpc_challenge` (a separately generated seller fails at `tls_certificate_pin` first and
    /// silently proves the pin instead), and the PUBLIC address class is covered via
    /// `public_alias`.

    /// E2E-ADV-12, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ADV-12/L0
    /// NOT RUN ON WINDOWS. The alias below is a NAME, not a literal -- `127.1` is what makes
    /// `classify_advertise` take its name branch and answer PUBLIC -- so the probe has to resolve it,
    /// and Windows will not. Measured on `windows-latest`: `getaddrinfo("127.1")` fails outright
    /// (`11001`), as do `127.0.1`, `127.000.000.001` and `0x7f000001`; only the full `127.0.0.1`
    /// resolves, and that spelling classifies as loopback, which is the opposite of what this row
    /// needs. No name exists that Windows resolves to loopback AND `classify_name` calls public:
    /// the only such name is `localhost`, which that function lists as non-public by design.

    /// A second measured obstacle stands behind the first: on `windows-latest` even a refused
    /// connect returns after 2.031 s, against this fixture's 300 ms probe budget, so the row would
    /// read `handshake_timeout` whatever spelling it used.

    /// What the row proves -- our own classification and verdict logic -- has nothing platform-bound
    /// in it and is proven on Linux and macOS. Skipping is honest here; a permanently red leg is
    /// not, because it hides the Windows regressions this suite could still catch.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn every_server_returned_grpc_status_is_fatal_before_sell_on_both_address_classes() {
        let mut order_id = 200_u128;
        for code in [
            tonic::Code::PermissionDenied,
            tonic::Code::InvalidArgument,
            tonic::Code::Internal,
            tonic::Code::ResourceExhausted,
            tonic::Code::Unavailable,
        ] {
            for class in ["private", "public"] {
                // A FRESH impostor per case: `prepare_seller_offer_with_timing` aborts the gateway
                // task on a readiness failure (`liveness.rs:1035`), so a reused one would fail the
                // next case at `GatewayTask` and never reach the probe at all -- proving nothing
                // about the status under test.
                let (seller, listen) = status_seller(tonic::Status::new(
                    code,
                    format!("injected server status {code:?}"),
                ))
                .await;
                let listen_addr: std::net::SocketAddr = listen.parse().expect("impostor address");
                let advertised = match class {
                    "private" => listen.clone(),
                    _ => public_alias(listen_addr),
                };
                let owner = address('8');
                let backend =
                    CancelBackend::new(Vec::new(), owner.clone(), order_id, CancelBehavior::Remove);
                order_id += 1;
                let mut config = cfg(&address('9'));
                config.gateway_advertise.clone_from(&advertised);
                let label = format!("{code:?} on a {class} advertise");

                let outcome = prepare_seller_offer_with_timing(
                    &seller,
                    &backend,
                    &config,
                    &owner,
                    None,
                    std::future::pending(),
                    fatal_preflight_timing(AdvertiseProbePolicy::default()),
                )
                .await;
                let error = render_startup(&outcome);
                assert_nothing_was_posted(&backend, &label, &error);
                assert!(
                    outcome.is_err(),
                    "{label}: a server-returned application status must fail before SELL: {error}"
                );
                assert!(
                    error.contains("error[E_ADVERTISE_WRONG_GATEWAY] (tls)")
                        && error.contains("stage: grpc_challenge"),
                    "{label}: the probe never reached the gRPC challenge: {error}"
                );
                // The component that failed, pinned through `HealthFailure`'s own rendering, so a
                // refactor cannot attribute this to the upstream or the gateway task.
                assert!(
                    error.contains("cause: advertised_gateway failed:"),
                    "{label}: the failure is not attributed to the advertised_gateway component: \
                     {error}"
                );
                seller.server_task.abort();
            }
        }
    }

    /// E2E-ADV-10, swept -- successor to
    /// `probe_degradation_covers_only_transport_faults_on_a_public_advertise`, which was the only
    /// direct assertion that degradation exists at all.

    /// The predecessor's first assertion -- transport fault + public advertise + the default policy
    /// DEGRADES -- is the one the ruling cancels. Its other three remain true and are kept, so this
    /// is a strict superset of what it replaces: no transport fault, on any address class, under
    /// any policy, is tolerated.

    /// Driven through `prepare_seller_offer_with_timing` with a REAL probe against a real refused
    /// address, and verified on the posted-offer facts. An earlier revision injected the fault into
    /// `check_readiness_with_probe` and asserted only the returned error, which could have passed
    /// while the posting path still wrote an order.

    /// Both address classes are reachable here because a transport fault -- unlike a wrong-endpoint
    /// proof -- needs nothing to answer: `192.0.2.1` is public and unroutable, and a bound-but-not-
    /// listening loopback socket is private and refused. The refusing reservation is held for the
    /// whole assertion, since a bind-and-drop hands the port back to the kernel.

    /// E2E-ADV-10, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ADV-10/L0
    /// NOT RUN ON WINDOWS. The alias below is a NAME, not a literal -- `127.1` is what makes
    /// `classify_advertise` take its name branch and answer PUBLIC -- so the probe has to resolve it,
    /// and Windows will not. Measured on `windows-latest`: `getaddrinfo("127.1")` fails outright
    /// (`11001`), as do `127.0.1`, `127.000.000.001` and `0x7f000001`; only the full `127.0.0.1`
    /// resolves, and that spelling classifies as loopback, which is the opposite of what this row
    /// needs. No name exists that Windows resolves to loopback AND `classify_name` calls public:
    /// the only such name is `localhost`, which that function lists as non-public by design.

    /// A second measured obstacle stands behind the first: on `windows-latest` even a refused
    /// connect returns after 2.031 s, against this fixture's 300 ms probe budget, so the row would
    /// read `handshake_timeout` whatever spelling it used.

    /// What the row proves -- our own classification and verdict logic -- has nothing platform-bound
    /// in it and is proven on Linux and macOS. Skipping is honest here; a permanently red leg is
    /// not, because it hides the Windows regressions this suite could still catch.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn no_transport_fault_is_tolerated_on_any_address_class_or_policy() {
        let (_refusing_hold, refused_private) = crate::test_refusing_endpoint::refusing_endpoint();
        // ONE held refusing socket, spelled two ways: the classifier sees a private literal and a
        // public name, the dial lands on the same dead port either way. That keeps the two cases
        // differing ONLY in address class, which is the dimension under test -- and it needs no
        // TEST-NET-1 literal, so PR820's reclassification of the documentation ranges cannot
        // silently collapse the sweep onto one branch.
        let refused_public = public_alias(refused_private.parse().expect("refusing address"));
        assert!(!crate::seller::advertise::advertise_is_public(
            &refused_private
        ));

        let mut order_id = 320_u128;
        for advertised in [refused_public.as_str(), refused_private.as_str()] {
            for policy in [
                AdvertiseProbePolicy::TolerateTunneledTransportFailure,
                AdvertiseProbePolicy::Required,
            ] {
                let seller = super::super::start_gateway_with_note(
                    "127.0.0.1:0".parse().unwrap(),
                    UpstreamConfig::Mock,
                    Arc::new(LocalNote::generate()),
                )
                .await
                .unwrap();
                let owner = address('5');
                let backend =
                    CancelBackend::new(Vec::new(), owner.clone(), order_id, CancelBehavior::Remove);
                order_id += 1;
                let mut config = cfg(&address('a'));
                config.gateway_advertise = advertised.to_string();
                let label = format!("transport fault on {advertised} under {policy:?}");

                let outcome = prepare_seller_offer_with_timing(
                    &seller,
                    &backend,
                    &config,
                    &owner,
                    None,
                    std::future::pending(),
                    fatal_preflight_timing(policy),
                )
                .await;
                seller.server_task.abort();

                let rendered = render_startup(&outcome);
                if backend.posts.load(Ordering::Relaxed) != 0 {
                    panic!("E2E-ADV-10D transport failure was tolerated by address or policy");
                }
                assert_nothing_was_posted(&backend, &label, &rendered);
                assert!(
                    outcome.is_err(),
                    "{label} was tolerated and the seller reached the posting path; under the \
                     ruling every probe failure is fatal: {rendered}"
                );
                assert!(
                    rendered.contains("error[E_ADVERTISE_UNREACHABLE] (network)")
                        && rendered.contains(advertised),
                    "{label}: {rendered}"
                );
                // A refused connection reaches `tcp_connect`, never the timeout branch: this row
                // covers the TRANSPORT fault, and the timeout arm has its own.
                assert!(
                    rendered.contains("(stage: tcp_connect)"),
                    "{label}: this row covers the transport fault, not the timeout: {rendered}"
                );
            }
        }
    }

    #[tokio::test]
    async fn upstream_unreachable_rejected_missing_model_and_timeout_fail_closed() {
        // The first case is the UNREACHABLE upstream (refused at connect), a different fail-closed
        // path from the fourth (a server that answers too slowly). If a bind-and-drop port is taken
        // by somebody else the two collapse into one and the case stops proving its own name
        // so the refusing reservation is held for the whole assertion.
        let (_dead_hold, dead) = crate::test_refusing_endpoint::refusing_endpoint();
        let cases = [
            (format!("http://{dead}"), None, Duration::from_secs(1)),
            (
                http_server(
                    "401 Unauthorized",
                    "{\"error\":{\"message\":\"bad credential\"}}".to_string(),
                    Duration::ZERO,
                )
                .await
                .0,
                None,
                Duration::from_secs(1),
            ),
            (
                http_server(
                    "404 Not Found",
                    "{\"error\":{\"message\":\"model absent\"}}".to_string(),
                    Duration::ZERO,
                )
                .await
                .0,
                None,
                Duration::from_secs(1),
            ),
            (
                http_server("200 OK", healthy_sse(), Duration::from_millis(500))
                    .await
                    .0,
                Some(true),
                Duration::from_millis(100),
            ),
        ];

        for (base_url, expect_timeout, timeout) in cases {
            let seller = super::super::start_gateway_with_note(
                "127.0.0.1:0".parse().unwrap(),
                openai(base_url),
                Arc::new(LocalNote::generate()),
            )
            .await
            .unwrap();
            let advertised = seller.listen_addr.to_string();

            // the two readiness components share ONE deadline, are polled with `join!`, and
            // the advertised-gateway verdict is consulted first. So whenever both can expire, the
            // component that gets reported is decided by which of them finished inside the budget --
            // an ordering nothing promises. The last case gives readiness 100 ms against an upstream
            // that answers in 500 ms; on a loaded host the local pinned-TLS self-probe loses that
            // same 100 ms and the failure reads `AdvertisedGateway` instead of `UpstreamModel`.

            // The subject of this test is the UPSTREAM: a bad upstream must fail readiness, and the
            // slow one must be reported as a timeout. So the gateway's outcome is ESTABLISHED
            // instead of raced, the way settled the same class -- run the REAL probe against
            // the REAL gateway first and assert its REAL result, then hand the readiness call that
            // settled outcome so the only component still racing the budget is the one under test.

            // This injects no fault and hides none: `probe_gateway` never reaches the upstream (DNS,
            // TCP, pinned TLS and the gRPC challenge all terminate at the seller's own gateway), so
            // a probe that stopped detecting a broken gateway fails the assertion below rather than
            // being concealed by it.
            probe_advertised_gateway(&seller, &advertised)
                .await
                .expect("the gateway must be serving before the upstream bound is exercised");

            let failure = check_readiness_with_probe(
                &seller,
                &advertised,
                timeout,
                None,
                &address('c'),
                AdvertiseProbePolicy::default(),
                std::future::ready(Ok(())),
            )
            .await
            .expect_err("bad upstream must fail readiness");
            assert_eq!(failure.component, HealthComponent::UpstreamModel);
            if expect_timeout.is_some() {
                assert!(failure.timed_out);
            }
            seller.server_task.abort();
        }
    }

    #[tokio::test]
    async fn repeated_health_cycles_leave_no_health_nonces_on_success_or_failure() {
        const BUYER_TC: &str = "0:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let healthy = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let buyer = LocalNote::generate();
        let buyer_nonces = [b"buyer-a".as_slice(), b"buyer-b".as_slice()];
        healthy.state.auth.register(BUYER_TC, buyer.pubkey());
        for nonce in buyer_nonces {
            healthy.state.auth.issue_challenge(BUYER_TC, nonce.to_vec());
        }

        for _ in 0..3 {
            check_readiness(
                &healthy,
                &healthy.listen_addr.to_string(),
                Duration::from_secs(1),
                None,
                BUYER_TC,
                AdvertiseProbePolicy::default(),
            )
            .await
            .expect("healthy cycle");
            assert_eq!(
                healthy
                    .state
                    .auth
                    .outstanding_challenge_count(HEALTH_CHALLENGE_TC),
                0,
                "health challenge is discarded after every successful cycle"
            );
            assert_eq!(
                healthy.state.auth.outstanding_challenge_count(BUYER_TC),
                2,
                "health cleanup must not consume concurrent buyer challenges"
            );
        }

        for nonce in buyer_nonces {
            let signature = buyer.sign(&crate::seller::auth::challenge_bytes(BUYER_TC, nonce));
            assert!(
                healthy
                    .state
                    .auth
                    .verify_response(BUYER_TC, nonce, &signature),
                "ordinary buyer challenges retain consume-on-success semantics"
            );
        }
        assert_eq!(healthy.state.auth.outstanding_challenge_count(BUYER_TC), 0);
        healthy.server_task.abort();

        let (base_url, upstream_server) = http_server(
            "401 Unauthorized",
            "{\"error\":{\"message\":\"bad credential\"}}".to_string(),
            Duration::ZERO,
        )
        .await;
        let failing = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            openai(base_url),
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let failure = check_readiness(
            &failing,
            &failing.listen_addr.to_string(),
            Duration::from_secs(1),
            None,
            BUYER_TC,
            AdvertiseProbePolicy::default(),
        )
        .await
        .expect_err("upstream rejection fails the health cycle");
        assert_eq!(failure.component, HealthComponent::UpstreamModel);
        assert_eq!(
            failing
                .state
                .auth
                .outstanding_challenge_count(HEALTH_CHALLENGE_TC),
            0,
            "health challenge is discarded before a later component fails"
        );
        failing.server_task.abort();
        upstream_server.await.unwrap();
    }

    /// E2E-ADV-01 positive control: all readiness components pass before exactly one accepted
    /// post. The negative components are the existing exact tests
    /// `a_closed_advertised_port_posts_nothing_even_though_the_gateway_is_bound`,
    /// `upstream_unreachable_rejected_missing_model_and_timeout_fail_closed`, and
    /// `gateway_task_death_cancels_within_one_health_cycle`.

    /// E2E-ADV-01, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ADV-01/L0
    #[tokio::test]
    async fn e2e_adv_01_all_preflights_pass_before_one_post() {
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let owner = address('d');
        let tc = address('e');
        let backend = CancelBackend::new(Vec::new(), owner.clone(), 91, CancelBehavior::Remove);
        let startup = prepare_seller_offer_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &owner,
            None,
            std::future::pending(),
            fast_timing(),
        )
        .await
        .unwrap();

        assert_eq!(backend.order_ids(), vec![91]);
        assert_eq!(backend.posts.load(Ordering::Relaxed), 1);
        assert!(matches!(
            startup,
            SellerStartupOutcome::Ready(SellerOfferStartup::Posted {
                outcome: Some(SellOfferOutcome::Rested { order_id: 91 }),
            })
        ));
        seller.server_task.abort();
    }

    /// E2E-ADV-14 -- the DISPATCH half. The record predicate itself was always correct; the defect was
    /// that startup never invoked it, so the fact worth proving is that
    /// `prepare_seller_offer_with_timing` CALLS `assert_note_covers_seller_bond` and that a refusal
    /// stops the seller before `postSellOffer`.

    /// This is `e2e_adv_01_all_preflights_pass_before_one_post` with ONE variable changed -- that row
    /// is healthy, ready, and posts exactly once on this same fixture -- so `posts == 0` here can only
    /// be the bond gate.

    /// The error must also SURVIVE as an error. A startup `Err` raised any later than this is
    /// swallowed by `resolve_interrupted_startup` (`liveness.rs:1274-1357`) into
    /// `Ok(SellerStartupOutcome::Stopped {.. })`, so gating inside `post_offer` would have reported
    /// a clean stop over a bond the note cannot pay.

    /// E2E-ADV-14, `tests/e2e/test-specification.md`.
    #[tokio::test]
    async fn e2e_adv_14_a_record_that_cannot_cover_the_bond_stops_startup_before_any_post() {
        const SHORTFALL: &str = "seller note 0:aa has getDetails.balance[2] SHELL raw units 1999, \
                                 below required seller bond 2P = 2000";
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let owner = address('d');
        let tc = address('e');
        let backend = CancelBackend::new(Vec::new(), owner.clone(), 91, CancelBehavior::Remove)
            .with_bond_cover_error(SHORTFALL);

        let outcome = prepare_seller_offer_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &owner,
            None,
            std::future::pending(),
            fast_timing(),
        )
        .await;
        let rendered = render_startup(&outcome);

        assert!(
            outcome.is_err(),
            "a bond-short record must REFUSE, not resolve into a Stopped outcome: {rendered}"
        );
        assert!(
            rendered.contains(SHORTFALL),
            "the refusal must carry the backend's own shortfall message: {rendered}"
        );
        assert_nothing_was_posted(&backend, "seller note cannot cover the 2P bond", &rendered);
        seller.server_task.abort();
    }

    /// E2E-ADV-02/L0 -- both required adversaries drive the actual readiness-to-post composition.
    /// The first provider is down. The second accepts the configured `exact-model` request but its
    /// OpenAI-compatible response identifies the model which actually served it as
    /// `foreign-provider-model`; changing the signal manifest is deliberately not used as a
    /// substitute for provider identity. Both arms assert the submit counter and authoritative
    /// book. L1/L2 remain live gates and are not claimed by this deterministic row.

    /// E2E-ADV-02, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ADV-02/L0
    #[tokio::test]
    async fn e2e_adv_02_different_upstream_model_never_reaches_the_book() {
        let owner = address('3');
        let tc = address('4');

        let (_down_hold, down_address) = crate::test_refusing_endpoint::refusing_endpoint();
        let down_seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            openai(format!("http://{down_address}")),
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let down_backend =
            CancelBackend::new(Vec::new(), owner.clone(), 211, CancelBehavior::Remove);
        let down_outcome = prepare_seller_offer_with_timing(
            &down_seller,
            &down_backend,
            &cfg_for_seller(&tc, &down_seller),
            &owner,
            None,
            std::future::pending(),
            fast_timing(),
        )
        .await;
        assert_nothing_was_posted(
            &down_backend,
            "E2E-ADV-02 unavailable upstream",
            &render_startup(&down_outcome),
        );
        assert!(down_outcome.is_err(), "unavailable upstream became ready");
        down_seller.server_task.abort();

        let foreign_sse = "data: {\"model\":\"foreign-provider-model\",\"choices\":[{\"delta\":{\"content\":\"OK\"},\"logprobs\":{\"content\":[{\"token\":\"OK\",\"logprob\":-0.1,\"top_logprobs\":[]}]}}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":0,\"completion_tokens\":1,\"total_tokens\":1}}\n\ndata: [DONE]\n\n".to_string();
        let (base_url, _probes, foreign_server) = scripted_http_server(vec![
            ("200 OK", foreign_sse.clone(), Duration::ZERO),
            ("200 OK", foreign_sse, Duration::ZERO),
        ]);
        let foreign_seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            openai(base_url),
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let foreign_backend =
            CancelBackend::new(Vec::new(), owner.clone(), 212, CancelBehavior::Remove);
        let foreign_outcome = prepare_seller_offer_with_timing(
            &foreign_seller,
            &foreign_backend,
            &cfg_for_seller(&tc, &foreign_seller),
            &owner,
            None,
            std::future::pending(),
            fast_timing(),
        )
        .await;
        assert!(
            foreign_backend.posts.load(Ordering::Relaxed) == 0
                && foreign_backend.order_ids().is_empty()
                && foreign_outcome.is_err(),
            "E2E-ADV-02 different served model reached postSellOffer"
        );
        foreign_seller.server_task.abort();
        foreign_server.abort();
    }

    /// E2E-ADV-06 -- reachability is an invariant for the whole resting lifetime, not a startup
    /// sample. Start with a verified endpoint and an exact resting id, then replace the advertised
    /// route with a deterministic public transport refusal while supervision is live. The exact
    /// order must be cancelled and confirmed absent.

    /// E2E-ADV-06, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ADV-06/L0
    #[tokio::test]
    async fn e2e_adv_06_lost_reachability_cancels_the_exact_resting_order() {
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let owner = address('9');
        let tc = address('a');
        let id = identity(&owner, &tc, 223);
        let backend = CancelBackend::new(
            Vec::new(),
            owner.clone(),
            id.order_id,
            CancelBehavior::Remove,
        );
        let initial = cfg_for_seller(&tc, &seller);
        let startup = prepare_seller_offer_with_timing(
            &seller,
            &backend,
            &initial,
            &owner,
            None,
            std::future::pending(),
            fast_timing(),
        )
        .await
        .unwrap();
        assert!(matches!(startup, SellerStartupOutcome::Ready(_)));
        assert_eq!(backend.order_ids(), vec![id.order_id]);

        let (_refusing_hold, refused) = crate::test_refusing_endpoint::refusing_endpoint();
        let mut lost = initial;
        lost.gateway_advertise = public_alias(refused.parse().expect("refusing endpoint address"));
        let (_watch_dir, watch_config) = watch("adv06-loss");
        let supervision = supervise_with_timing(
            &seller,
            &backend,
            &lost,
            &watch_config,
            &id,
            std::future::pending(),
            fast_timing(),
        );
        tokio::time::timeout(Duration::from_secs(10), supervision)
            .await
            .expect("E2E-ADV-06 supervision must reach its terminal outcome")
            .expect("E2E-ADV-06 supervision must not fail");

        assert!(
            backend.order_ids().is_empty() && backend.calls() == vec![(tc, id.order_id)],
            "E2E-ADV-06 undialable address retained its resting offer"
        );
        seller.server_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delayed_fresh_resting_confirmation_rechecks_health_before_ready() {
        let (base_url, first_response_complete, upstream_server) =
            counted_http_server("200 OK", healthy_sse(), 1).await;
        let owner = address('d');
        let tc = address('e');
        let order_id = 90;
        let backend =
            CancelBackend::new(Vec::new(), owner.clone(), order_id, CancelBehavior::Remove)
                .with_confirmation_ready(first_response_complete);
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            openai(base_url),
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        wait_for_gateway_ready(&seller).await;

        let outcome = prepare_seller_offer_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &owner,
            None,
            std::future::pending(),
            SupervisionTiming {
                health_interval: Duration::from_millis(5),
                health_timeout: Duration::from_millis(100),
                cycle_timeout: Duration::from_millis(300),
                cancel_poll: Duration::from_millis(1),
                expiry_poll: Duration::from_secs(3_600),
                abort_gateway_on_stop: true,
                advertise_probe: AdvertiseProbePolicy::default(),
                ..fast_timing()
            },
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            SellerStartupOutcome::Stopped {
                identity: Some(RestingOfferIdentity { order_id: 90, .. }),
                reason: RestingStopReason::Health(HealthFailure {
                    component: HealthComponent::UpstreamModel,
                    ..
                }),
                disposition: CancellationDisposition::Cancelled,
            }
        ));
        assert_eq!(backend.posts.load(Ordering::Relaxed), 1);
        assert_eq!(backend.calls(), vec![(tc, order_id)]);
        assert!(backend.order_ids().is_empty());
        upstream_server.await.unwrap();
    }

    /// A seller whose provider answers the way a REAL provider answers reaches the book.

    /// Every readiness row above this one is fed a hand-written stream, and every one of them closes
    /// with a single terminal usage record. A live Groq `qwen/qwen3-32b` does not: it states the same
    /// total twice, on the `finish_reason` chunk and again on the dedicated
    /// `stream_options.include_usage` chunk. The adapter counted that as a contradiction, readiness
    /// failed the `upstream_authentication_and_model` component, and the seller exited with
    /// "seller readiness failed before SELL" -- so the book stayed empty and no buyer had anywhere to
    /// go. Offline the whole suite was green, because no fixture had ever been the real wire.

    /// So this row is fed `LIVE_GROQ_READINESS_CAPTURE` -- the exact recorded bytes, unedited -- and
    /// drives production's own composition (`prepare_seller_offer_with_timing`: readiness, then the
    /// post). The claim is the one the campaign falsified: the seller becomes ready and the SELL is
    /// posted.
    #[tokio::test]
    async fn a_live_provider_stream_reaches_readiness_and_posts_the_sell() {
        // The scripted server keeps serving, so every health cycle in the startup path sees the same
        // real stream; a single-shot listener would prove only the first probe.
        let (base_url, _probes, upstream_server) = scripted_http_server(vec![(
            "200 OK",
            crate::seller::upstream::openai::LIVE_GROQ_READINESS_CAPTURE.to_string(),
            Duration::ZERO,
        )]);
        // the capture is a real `qwen/qwen3-32b` stream, so this row must SELL the model it
        // actually replays -- readiness refuses a market whose provider answers as another model. The
        // market id is the knob, not the outbound slug: the slug is what we ask the provider for, and a
        // provider that echoes it cannot thereby prove anything about the market (E2E-ADV-02/L2).
        let mut upstream = openai(base_url);
        if let UpstreamConfig::OpenAi(cfg) = &mut upstream {
            cfg.frame_model = crate::seller::upstream::openai::DEFAULT_MODEL.to_string();
        }
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            upstream,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let owner = address('c');
        let tc = address('f');
        let order_id = 861;
        let backend =
            CancelBackend::new(Vec::new(), owner.clone(), order_id, CancelBehavior::Remove);

        let outcome = prepare_seller_offer_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &owner,
            None,
            std::future::pending(),
            fast_timing(),
        )
        .await;

        let rendered = render_startup(&outcome);
        assert!(
            matches!(outcome, Ok(SellerStartupOutcome::Ready(_))),
            "a real provider stream must reach readiness: {rendered}"
        );
        assert_eq!(
            backend.posts.load(Ordering::Relaxed),
            1,
            "readiness that passes must leave the SELL on the book: {rendered}"
        );
        assert_eq!(backend.order_ids(), vec![order_id]);
        seller.server_task.abort();
        upstream_server.abort();
    }

    #[tokio::test]
    async fn shutdown_during_sell_confirmation_cancels_the_new_exact_order() {
        let owner = address('d');
        let tc = address('e');
        let order_id = 92;
        let backend =
            CancelBackend::new(Vec::new(), owner.clone(), order_id, CancelBehavior::Remove)
                .with_confirm_delay(Duration::from_millis(40));
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();

        let outcome = prepare_seller_offer_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &owner,
            None,
            async {
                while backend.posts.load(Ordering::Relaxed) == 0 {
                    tokio::task::yield_now().await;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            },
            SupervisionTiming {
                health_interval: Duration::from_millis(5),
                health_timeout: Duration::from_secs(1),
                cycle_timeout: Duration::from_secs(2),
                cancel_poll: Duration::from_millis(1),
                expiry_poll: Duration::from_secs(3_600),
                abort_gateway_on_stop: true,
                advertise_probe: AdvertiseProbePolicy::default(),
                ..fast_timing()
            },
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            SellerStartupOutcome::Stopped {
                identity: Some(RestingOfferIdentity { order_id: 92, .. }),
                reason: RestingStopReason::Shutdown,
                disposition: CancellationDisposition::Cancelled,
            }
        ));
        assert_eq!(backend.posts.load(Ordering::Relaxed), 1);
        assert_eq!(backend.calls(), vec![(tc, order_id)]);
        assert!(backend.order_ids().is_empty());
    }

    #[tokio::test]
    async fn gateway_death_during_sell_confirmation_cancels_the_new_exact_order() {
        let owner = address('d');
        let tc = address('f');
        let order_id = 93;
        let backend = Arc::new(
            CancelBackend::new(Vec::new(), owner.clone(), order_id, CancelBehavior::Remove)
                .with_confirm_delay(Duration::from_millis(200)),
        );
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let abort = seller.server_task.abort_handle();
        let abort_after_post = backend.clone();
        tokio::spawn(async move {
            while abort_after_post.posts.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            abort.abort();
        });

        let outcome = prepare_seller_offer_with_timing(
            &seller,
            backend.as_ref(),
            &cfg_for_seller(&tc, &seller),
            &owner,
            None,
            std::future::pending(),
            SupervisionTiming {
                health_interval: Duration::from_millis(5),
                health_timeout: Duration::from_secs(1),
                cycle_timeout: Duration::from_secs(2),
                cancel_poll: Duration::from_millis(1),
                expiry_poll: Duration::from_secs(3_600),
                abort_gateway_on_stop: true,
                advertise_probe: AdvertiseProbePolicy::default(),
                ..fast_timing()
            },
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            SellerStartupOutcome::Stopped {
                identity: Some(RestingOfferIdentity { order_id: 93, .. }),
                reason: RestingStopReason::Health(HealthFailure {
                    component: HealthComponent::GatewayTask,
                    ..
                }),
                disposition: CancellationDisposition::Cancelled,
            }
        ));
        assert_eq!(backend.posts.load(Ordering::Relaxed), 1);
        assert_eq!(backend.calls(), vec![(tc, order_id)]);
        assert!(backend.order_ids().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupted_fresh_post_cancels_after_delayed_authoritative_visibility() {
        let owner = address('d');
        let tc = address('9');
        let order_id = 94;
        let backend =
            CancelBackend::new(Vec::new(), owner.clone(), order_id, CancelBehavior::Remove)
                .with_post_visibility_after_vacant_reads(1);
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        wait_for_gateway_ready(&seller).await;

        let outcome = prepare_seller_offer_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &owner,
            None,
            async {
                backend.wait_for_post_submission().await;
                backend.release_interrupted_post();
            },
            SupervisionTiming {
                health_interval: Duration::from_millis(5),
                health_timeout: Duration::from_millis(100),
                cycle_timeout: Duration::from_millis(300),
                cancel_poll: Duration::from_millis(1),
                expiry_poll: Duration::from_secs(3_600),
                abort_gateway_on_stop: true,
                advertise_probe: AdvertiseProbePolicy::default(),
                ..fast_timing()
            },
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            SellerStartupOutcome::Stopped {
                identity: Some(RestingOfferIdentity { order_id: 94, .. }),
                reason: RestingStopReason::Shutdown,
                disposition: CancellationDisposition::Cancelled,
            }
        ));
        assert_eq!(backend.posts.load(Ordering::Relaxed), 1);
        assert_eq!(backend.calls(), vec![(tc, order_id)]);
        assert!(backend.order_ids().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interrupted_fresh_post_without_terminal_fact_is_unknown_not_absent() {
        let owner = address('d');
        let tc = address('a');
        let backend = CancelBackend::new(Vec::new(), owner.clone(), 95, CancelBehavior::Remove)
            .with_post_never_visible();
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        wait_for_gateway_ready(&seller).await;

        let outcome = prepare_seller_offer_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &owner,
            None,
            async {
                backend.wait_for_post_submission().await;
                backend.release_interrupted_post();
            },
            SupervisionTiming {
                health_interval: Duration::from_millis(5),
                health_timeout: Duration::from_millis(100),
                cycle_timeout: Duration::from_millis(150),
                cancel_poll: Duration::from_millis(1),
                expiry_poll: Duration::from_secs(3_600),
                abort_gateway_on_stop: true,
                advertise_probe: AdvertiseProbePolicy::default(),
                ..fast_timing()
            },
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            SellerStartupOutcome::Stopped {
                identity: None,
                reason: RestingStopReason::Shutdown,
                disposition: CancellationDisposition::UnknownFailure { ref known_result },
            } if known_result.contains("authoritative_state=vacant")
        ));
        assert!(
            !seller.server_task.is_finished(),
            "unknown fresh-post outcome must not explicitly kill the gateway"
        );
        seller.server_task.abort();
    }

    /// E2E-SELL-06 /: the live incident. The seller posted order 11 with deadline
    /// `2026-08-02 16:48:45 MSK`; after it passed, the process stayed alive, kept logging healthy
    /// cycles and kept waiting for a match that could no longer happen, while the buyer-facing
    /// executable-book read already returned nothing.

    /// Health here is deliberately PERFECT and the match watcher never fires, so nothing but the
    /// deadline itself can end supervision -- a health-driven cancel would be a different outcome and
    /// would fail the disposition assertion below.
    #[tokio::test]
    async fn resting_offer_expiry_is_a_terminal_outcome_while_health_stays_green() {
        let owner = address('e');
        let tc = address('f');
        let id = identity(&owner, &tc, 11);
        // Already past its deadline when supervision looks: the row is still in the book, because
        // on-chain expiry removal is lazy and nobody may ever call the permissionless sweep.
        let deadline = unix_timestamp() - 779;
        let backend = CancelBackend::new(
            vec![order_with_deadline(id.order_id, &owner, &tc, deadline)],
            owner.clone(),
            id.order_id,
            CancelBehavior::Keep,
        );
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();

        let outcome = supervise_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &watch("expiry-terminal").1,
            &id,
            std::future::pending(),
            SupervisionTiming {
                health_interval: Duration::from_millis(5),
                health_timeout: Duration::from_secs(1),
                cycle_timeout: Duration::from_secs(2),
                cancel_poll: Duration::from_millis(1),
                expiry_poll: Duration::from_millis(5),
                abort_gateway_on_stop: true,
                advertise_probe: AdvertiseProbePolicy::default(),
                ..fast_timing()
            },
        )
        .await
        .unwrap();

        match outcome {
            RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Expired(expired),
                disposition: CancellationDisposition::NotAttemptedExpired,
            } => {
                assert_eq!(
                    expired.deadline, deadline,
                    "the outcome carries the book's own absolute deadline, not a local estimate"
                );
                assert!(
                    expired.observed_at >= deadline,
                    "expiry is only reported from a clock at or past the deadline"
                );
            }
            other => panic!("expiry must be its own terminal outcome, got {other:?}"),
        }

        assert!(
            backend.calls().is_empty(),
            "an order the matcher already skips must not be cancelled: that spends gas to remove \
             what is already unmatchable and races the permissionless sweep for nothing"
        );
        assert!(
            !seller.server_task.is_finished(),
            "an expired offer is not an unhealthy seller, so the gateway keeps serving"
        );
        seller.server_task.abort();
    }

    /// A resting offer whose deadline is safely in the future stays this seller's to supervise; the
    /// shutdown branch, rather than expiry, is what ends supervision. The expired half fixes
    /// `deadline == now`, making its deadline-second assertion stable by construction.
    #[tokio::test]
    async fn live_resting_offer_stays_supervised_until_shutdown() {
        let owner = address('e');
        let tc = address('f');
        let id = identity(&owner, &tc, 11);
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();

        // Safely live: the canonical maximum SELL TTL is well beyond any scheduling delay here, so
        // supervision must continue until the shutdown branch ends it.
        let live_backend = CancelBackend::new(
            vec![order_with_deadline(
                id.order_id,
                &owner,
                &tc,
                unix_timestamp() + dexdo_core::params::MAX_SELL_TTL.as_secs(),
            )],
            owner.clone(),
            id.order_id,
            CancelBehavior::Remove,
        );
        let timing = SupervisionTiming {
            health_interval: Duration::from_secs(3_600),
            health_timeout: Duration::from_secs(1),
            cycle_timeout: Duration::from_secs(2),
            cancel_poll: Duration::from_millis(1),
            expiry_poll: Duration::from_millis(1),
            abort_gateway_on_stop: false,
            advertise_probe: AdvertiseProbePolicy::default(),
            ..fast_timing()
        };
        let outcome = supervise_with_timing(
            &seller,
            &live_backend,
            &cfg_for_seller(&tc, &seller),
            &watch("expiry-boundary-live").1,
            &id,
            async { tokio::time::sleep(Duration::from_millis(30)).await },
            timing,
        )
        .await
        .unwrap();
        assert!(
            matches!(
                outcome,
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Shutdown,
                    ..
                }
            ),
            "an order whose deadline has not arrived is still this seller's to supervise"
        );

        // At the deadline second: expired, without the clock having moved any further.
        let now = unix_timestamp();
        let lapsed_backend = CancelBackend::new(
            vec![order_with_deadline(id.order_id, &owner, &tc, now)],
            owner,
            id.order_id,
            CancelBehavior::Remove,
        );
        let outcome = supervise_with_timing(
            &seller,
            &lapsed_backend,
            &cfg_for_seller(&tc, &seller),
            &watch("expiry-boundary-lapsed").1,
            &id,
            std::future::pending(),
            timing,
        )
        .await
        .unwrap();
        assert!(
            matches!(
                outcome,
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Expired(RestingOfferExpiry { deadline, .. }),
                    ..
                } if deadline == now
            ),
            "at `now == deadline` the book already refuses the order, so the seller must too"
        );
        seller.server_task.abort();
    }

    /// an unreadable book is not proof of expiry. A transport blip must leave the offer
    /// supervised rather than manufacture a terminal outcome, because a false expiry abandons an
    /// offer that is still live and matchable.
    #[tokio::test]
    async fn an_unreadable_book_never_becomes_an_expiry_outcome() {
        let owner = address('e');
        let tc = address('f');
        let id = identity(&owner, &tc, 11);
        let backend = CancelBackend::new(Vec::new(), owner, id.order_id, CancelBehavior::Remove)
            .with_hanging_reads();
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();

        let outcome = supervise_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &watch("expiry-unreadable").1,
            &id,
            async { tokio::time::sleep(Duration::from_millis(40)).await },
            SupervisionTiming {
                health_interval: Duration::from_secs(3_600),
                health_timeout: Duration::from_secs(1),
                cycle_timeout: Duration::from_millis(60),
                cancel_poll: Duration::from_millis(1),
                expiry_poll: Duration::from_millis(1),
                abort_gateway_on_stop: false,
                advertise_probe: AdvertiseProbePolicy::default(),
                ..fast_timing()
            },
        )
        .await
        .unwrap();

        assert!(
            !matches!(
                outcome,
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Expired(_),
                    ..
                }
            ),
            "a book this client could not read says nothing about the order's deadline"
        );
        seller.server_task.abort();
    }

    /// The book and the deal as the contracts hold them, for the expiry-relist paths.

    /// Every rule below is a contract rule, not a convenience:
    /// - a post while `_offerPosted` is set is SILENTLY dropped
    /// (`contracts/airegistry/TokenContract.sol:713`), which is exactly the failure a seller that
    /// relists without confirming the latch would walk into;
    /// - each posting gets a NEW order id, because `_removeFromBook` deletes the old one and the book
    /// allocates the next;
    /// - `expireOrder` is permissionless and idempotent, and silently ignores an order that is gone
    /// AND one whose deadline has not passed
    /// (`contracts/airegistry/InferenceOrderBook.sol:1679-1691`);
    /// - reaping a SELL frees the deal's latch through `onSellClosed`
    /// (`contracts/airegistry/InferenceOrderBook.sol:1138-1149`);
    /// - `getDeal()` is constructor-bound, so the deal's terms survive every ask posted against them.
    struct RelistBackend {
        token_contract: String,
        owner: String,
        rows: Mutex<Vec<OrderBookOrder>>,
        /// Every ask this deal ever rested, oldest first -- the successor outlives the book row,
        /// which a shutdown cancels on its way out.
        postings: Mutex<Vec<OrderBookOrder>>,
        cancelled: Mutex<Vec<u128>>,
        offer_posted: Mutex<bool>,
        deal_terms: Option<(u64, u64)>,
        state: Mutex<DealChainState>,
        matched: Mutex<Option<Match>>,
        next_order_id: AtomicU64,
        posts: AtomicU64,
        expire_calls: Mutex<Vec<u128>>,
        max_rows_seen: AtomicU64,
        /// The permissionless sweep leaves the row in the book: an expiry this client cannot prove.
        expiry_is_ignored: bool,
        /// `onSellClosed` has not landed yet after this many latch reads.
        latch_release_after_reads: u64,
        latch_reads: AtomicU64,
        expire_fails: bool,
        /// Set the moment the expiry write is submitted, so a test can order a shutdown after cleanup.
        reaped: Arc<tokio::sync::Notify>,
        /// Set after the freshly posted successor is read back from the authoritative book.
        successor_observed: Arc<tokio::sync::Notify>,
    }

    impl RelistBackend {
        fn new(token_contract: &str, owner: &str, ticks: u64) -> Self {
            let backend = Self {
                token_contract: token_contract.to_string(),
                owner: owner.to_string(),
                rows: Mutex::new(Vec::new()),
                postings: Mutex::new(Vec::new()),
                cancelled: Mutex::new(Vec::new()),
                offer_posted: Mutex::new(false),
                deal_terms: Some((1000, ticks)),
                state: Mutex::new(fresh_deal_state()),
                matched: Mutex::new(None),
                next_order_id: AtomicU64::new(11),
                posts: AtomicU64::new(0),
                expire_calls: Mutex::new(Vec::new()),
                max_rows_seen: AtomicU64::new(0),
                expiry_is_ignored: false,
                latch_release_after_reads: 0,
                latch_reads: AtomicU64::new(0),
                expire_fails: false,
                reaped: Arc::new(tokio::sync::Notify::new()),
                successor_observed: Arc::new(tokio::sync::Notify::new()),
            };
            backend.rest_offer(unix_timestamp() - 1, ticks);
            backend
        }

        /// Put one ask in the book the way `postFromNote` does: a fresh id, the deal's own size, and
        /// the latch set.
        fn rest_offer(&self, deadline: u64, ticks: u64) -> u128 {
            let order_id = u128::from(self.next_order_id.fetch_add(1, Ordering::SeqCst));
            let row = OrderBookOrder {
                order_id,
                owner_note: self.owner.clone(),
                token_contract: Some(self.token_contract.clone()),
                is_buy: false,
                price_per_tick: 1000,
                ticks: u128::from(ticks),
                escrow: 0,
                deadline,
                flags: 0,
                timestamp: 1,
            };
            self.postings.lock().unwrap().push(row.clone());
            let mut rows = self.rows.lock().unwrap();
            rows.push(row);
            self.max_rows_seen
                .fetch_max(rows.len() as u64, Ordering::SeqCst);
            *self.offer_posted.lock().unwrap() = true;
            order_id
        }

        fn expire_calls(&self) -> Vec<u128> {
            self.expire_calls.lock().unwrap().clone()
        }

        fn cancelled(&self) -> Vec<u128> {
            self.cancelled.lock().unwrap().clone()
        }

        /// The ask this deal rested after the seeded one: the successor, as it was accepted, before
        /// any later shutdown cancelled its book row.
        fn successor(&self) -> OrderBookOrder {
            self.postings
                .lock()
                .unwrap()
                .get(1)
                .cloned()
                .expect("a successor ask was accepted")
        }

        fn resting_order_id(&self) -> Option<u128> {
            self.rows.lock().unwrap().first().map(|row| row.order_id)
        }

        fn live_rows(&self) -> Vec<OrderBookOrder> {
            self.rows.lock().unwrap().clone()
        }

        fn with_ignored_expiry(mut self) -> Self {
            self.expiry_is_ignored = true;
            self
        }

        fn with_latch_stuck_for(mut self, reads: u64) -> Self {
            self.latch_release_after_reads = reads;
            self
        }

        fn with_unreadable_latch(mut self) -> Self {
            self.deal_terms = None;
            self
        }

        fn with_deal_terms(mut self, terms: (u64, u64)) -> Self {
            self.deal_terms = Some(terms);
            self
        }

        fn with_deal_state(self, state: DealChainState) -> Self {
            *self.state.lock().unwrap() = state;
            self
        }

        fn with_match(self, matched: Match) -> Self {
            *self.matched.lock().unwrap() = Some(matched);
            self
        }

        fn with_failing_expiry(mut self) -> Self {
            self.expire_fails = true;
            self
        }

        /// Another actor (a matcher, a keeper) reaped the ask before this seller looked.
        fn reaped_by_another_actor(self) -> Self {
            self.rows.lock().unwrap().clear();
            *self.offer_posted.lock().unwrap() = false;
            self
        }
    }

    fn fresh_deal_state() -> DealChainState {
        DealChainState {
            funded: false,
            opened: false,
            probe_accepted: false,
            disputed: false,
            deposit: 0,
            finalized_owed: 0,
            tokens_final: 0,
            tokens_pending: 0,
            probe_tick: 0,
            funded_time: None,
            probe_time: 0,
            last_claim_time: 0,
            dispute_time: 0,
        }
    }

    #[async_trait::async_trait]
    impl ChainBackend for RelistBackend {
        async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
            Ok(Vec::new())
        }

        async fn post_offer(&self, offer: SellOffer, _: &dyn Note) -> Result<(), ChainError> {
            self.posts.fetch_add(1, Ordering::SeqCst);
            // `postFromNote` returns without posting while the latch is set or the deal is funded:
            // the seller gets no error, which is why the client must prove the latch first.
            if *self.offer_posted.lock().unwrap() || self.state.lock().unwrap().funded {
                return Ok(());
            }
            self.rest_offer(
                unix_timestamp() + dexdo_core::params::MAX_SELL_TTL.as_secs(),
                offer.max_ticks,
            );
            Ok(())
        }

        async fn confirm_offer_outcome(
            &self,
            _: &TokenContract,
        ) -> Result<Option<SellOfferOutcome>, ChainError> {
            if self.matched.lock().unwrap().is_some() {
                return Ok(Some(SellOfferOutcome::Matched));
            }
            Ok(self
                .resting_order_id()
                .map(|order_id| SellOfferOutcome::Rested { order_id }))
        }

        async fn raw_resting_sell_orders_for_tc(
            &self,
            token_contract: &TokenContract,
        ) -> Result<Vec<OrderBookOrder>, ChainError> {
            let rows = self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|row| row.token_contract.as_ref() == Some(token_contract))
                .cloned()
                .collect::<Vec<_>>();
            if self.posts.load(Ordering::SeqCst) > 0 && !rows.is_empty() {
                self.successor_observed.notify_one();
            }
            Ok(rows)
        }

        async fn expire_resting_sell_order(
            &self,
            _: &TokenContract,
            order_id: u128,
        ) -> Result<(), ChainError> {
            self.expire_calls.lock().unwrap().push(order_id);
            self.reaped.notify_waiters();
            if self.expire_fails {
                return Err(ChainError::Transport("response lost after submit".into()));
            }
            if self.expiry_is_ignored {
                return Ok(());
            }
            let mut rows = self.rows.lock().unwrap();
            let Some(position) = rows.iter().position(|row| row.order_id == order_id) else {
                return Ok(());
            };
            if unix_timestamp() < rows[position].deadline {
                return Ok(());
            }
            rows.remove(position);
            if self.latch_release_after_reads == 0 {
                *self.offer_posted.lock().unwrap() = false;
            }
            Ok(())
        }

        async fn token_contract_offer_latch(
            &self,
            _: &TokenContract,
        ) -> Result<Option<DealOfferLatch>, ChainError> {
            if self.deal_terms.is_none() {
                return Ok(None);
            }
            let reads = self.latch_reads.fetch_add(1, Ordering::SeqCst) + 1;
            if self.latch_release_after_reads > 0
                && reads >= self.latch_release_after_reads
                && self.rows.lock().unwrap().is_empty()
            {
                *self.offer_posted.lock().unwrap() = false;
            }
            Ok(Some(DealOfferLatch {
                offer_posted: *self.offer_posted.lock().unwrap(),
            }))
        }

        async fn sell_offer_terms(
            &self,
            _: &TokenContract,
        ) -> Result<Option<(u64, u64)>, ChainError> {
            Ok(self.deal_terms)
        }

        async fn deal_state(
            &self,
            _: &TokenContract,
        ) -> Result<Option<DealChainState>, ChainError> {
            Ok(Some(*self.state.lock().unwrap()))
        }

        async fn cancel_resting_sell_order(
            &self,
            _: &TokenContract,
            order_id: u128,
        ) -> Result<(), ChainError> {
            self.cancelled.lock().unwrap().push(order_id);
            let mut rows = self.rows.lock().unwrap();
            rows.retain(|row| row.order_id != order_id);
            if rows.is_empty() {
                *self.offer_posted.lock().unwrap() = false;
            }
            Ok(())
        }

        async fn poll_seller_fills(
            &self,
            _: &dyn Note,
            _: &mut dexdo_core::MatchWatchCursor,
        ) -> Result<Vec<dexdo_core::MatchedFill>, ChainError> {
            Ok(Vec::new())
        }

        async fn read_openable_match_now(
            &self,
            _: &TokenContract,
        ) -> Result<Option<Match>, ChainError> {
            Ok(self.matched.lock().unwrap().clone())
        }

        async fn read_match(&self, token_contract: &TokenContract) -> Result<Match, ChainError> {
            self.matched
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| ChainError::NoMatch(token_contract.clone()))
        }

        async fn place_buy(&self, _: &TokenContract, _: &dyn Note) -> Result<(), ChainError> {
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
            Ok(None)
        }

        async fn claim_tokens(
            &self,
            _: &TokenContract,
            _: &dyn Note,
            _: u128,
        ) -> Result<(), ChainError> {
            unimplemented!()
        }

        async fn accept_probe(&self, _: &TokenContract) -> Result<(), ChainError> {
            unimplemented!()
        }

        async fn stop(&self, _: &TokenContract, _: &dyn Note) -> Result<Settlement, ChainError> {
            unimplemented!()
        }

        async fn snapshot(&self, _: &TokenContract) -> Option<StreamSnapshot> {
            None
        }
    }

    /// Timing that lets a whole expiry -> reap -> relist cycle run inside a test: the deadline is
    /// re-read every millisecond and the reap gets a real but short budget.
    fn relist_timing() -> SupervisionTiming {
        SupervisionTiming {
            health_interval: Duration::from_secs(3_600),
            health_timeout: Duration::from_secs(2),
            cycle_timeout: Duration::from_secs(2),
            cancel_poll: Duration::from_millis(1),
            expiry_poll: Duration::from_millis(1),
            reap_timeout: Duration::from_millis(300),
            reap_poll: Duration::from_millis(1),
            abort_gateway_on_stop: false,
            advertise_probe: AdvertiseProbePolicy::default(),
        }
    }

    async fn relist_seller() -> RunningSeller {
        super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .expect("a healthy gateway for the relist paths")
    }

    /// One supervision cycle that ends at the first non-expiry outcome, so a test that expects
    /// exactly one relist is not racing an unbounded loop: the successor's deadline is a full
    /// `MAX_SELL_TTL` away, so the second generation never expires inside the test.
    async fn run_relist(
        seller: &RunningSeller,
        backend: &RelistBackend,
        identity: &RestingOfferIdentity,
        shutdown: impl Future<Output = ()>,
        cfg: SellerConfig,
    ) -> RestingSellerOutcome {
        supervise_and_relist_with_timing(
            seller,
            backend,
            &cfg,
            &watch("relist").1,
            identity,
            shutdown,
            relist_timing(),
        )
        .await
        .expect("supervision must return an outcome rather than an error")
    }

    fn relist_cfg(backend: &RelistBackend, seller: &RunningSeller, ticks: u64) -> SellerConfig {
        let mut cfg = cfg_for_seller(&backend.token_contract, seller);
        cfg.price_per_tick = 1000;
        cfg.max_ticks = ticks;
        cfg
    }

    /// E2E-SELL-10 /, the whole money path: deadline -> `expireOrder(exact id)` -> the deal's
    /// latch released -> exactly one accepted successor for the authoritative remaining capacity.

    /// The offer this seller posted is 1024 ticks and nothing ever filled it, so the deal's
    /// `getDeal().maxTicks` is still 1024 -- the successor is sized from THAT read, not from the
    /// expired row. The successor carries a fresh order id and a strictly later absolute deadline,
    /// and the book never holds two rows for this deal at any point.
    /// E2E-ROW: E2E-SELL-10/L0
    #[tokio::test]
    async fn e2e_sell_10_expiry_cleanup_permits_exactly_one_accepted_relist() {
        let owner = address('e');
        let tc = address('f');
        let backend = RelistBackend::new(&tc, &owner, 1024);
        let expired_order_id = backend.resting_order_id().expect("the expired ask rests");
        let expired_deadline = backend.live_rows()[0].deadline;
        let seller = relist_seller().await;
        // The seller's own configuration carries a STALE size and price -- the figures a client would
        // have if it copied them out of an old event row. The successor must be sized from the deal's
        // `getDeal()` instead, so these two are what the assertions below must NOT see.
        let mut cfg = relist_cfg(&backend, &seller, 8);
        cfg.price_per_tick = 999;
        // Stop once the authoritative book read has observed the successor, never after a fixed
        // wait: every assertion below is about chain state, so the scheduler must not decide
        // whether the relist got that far.
        let successor_observed = backend.successor_observed.clone();

        let outcome = run_relist(
            &seller,
            &backend,
            &identity(&owner, &tc, expired_order_id),
            async move { successor_observed.notified().await },
            cfg,
        )
        .await;

        assert!(
            matches!(
                outcome,
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Shutdown,
                    ..
                }
            ),
            "after the successor rested the seller is supervising again, so only the operator's \
             shutdown ends this run: {outcome:?}"
        );
        assert_eq!(
            backend.expire_calls(),
            vec![expired_order_id],
            "the seller expires its OWN exact order, once"
        );
        let successor = backend.successor();
        assert_ne!(
            successor.order_id, expired_order_id,
            "a reaped order id is never handed back; the successor is a new order"
        );
        assert!(
            successor.deadline > expired_deadline,
            "the successor's absolute deadline advances past the reaped one: {} vs {}",
            successor.deadline,
            expired_deadline
        );
        assert!(
            successor.deadline <= unix_timestamp() + dexdo_core::params::MAX_SELL_TTL.as_secs(),
            "the successor is published with the canonical finite TTL, capped at MAX_SELL_TTL = {}s \
             (`contracts/dex/PrivateNote.sol:41,792`)",
            dexdo_core::params::MAX_SELL_TTL.as_secs()
        );
        assert_eq!(
            successor.ticks, 1024,
            "capacity is conserved: an unfunded deal sold nothing, so the whole authoritative \
             getDeal().maxTicks is re-offered -- 1024, not the stale 8 this seller was configured with"
        );
        assert_eq!(
            successor.price_per_tick, 1000,
            "the successor keeps the deal's own price, which is a constructor static of the same TC"
        );
        assert_eq!(
            backend.cancelled(),
            vec![successor.order_id],
            "the shutdown cancels the SUCCESSOR's exact id, which is the identity supervision moved \
             onto after the relist"
        );
        assert_eq!(
            backend.posts.load(Ordering::SeqCst),
            1,
            "exactly one successor was submitted for the expired generation"
        );
        assert_eq!(
            backend.max_rows_seen.load(Ordering::SeqCst),
            1,
            "at most one live offer existed for this TokenContract throughout expiry, cleanup and \
             relist"
        );
        assert!(
            !seller.server_task.is_finished(),
            "the gateway the successor advertises is the same one, still serving"
        );
        seller.server_task.abort();
    }

    /// E2E-SELL-13 /: a matcher or another keeper reaped the ask first. The seller must
    /// reconcile that completed cleanup instead of writing its own, and still post exactly one
    /// successor.
    /// E2E-ROW: E2E-SELL-13/L0
    #[tokio::test]
    async fn e2e_sell_13_cleanup_completed_by_another_actor_is_reconciled_not_repeated() {
        let owner = address('e');
        let tc = address('f');
        let backend = RelistBackend::new(&tc, &owner, 512).reaped_by_another_actor();
        let expired_order_id = 11;
        let seller = relist_seller().await;
        let cfg = relist_cfg(&backend, &seller, 512);
        // The reconciliation is the subject, so the run ends when the successor has been read back
        // from the book -- not when a timer says it probably has.
        let successor_observed = backend.successor_observed.clone();

        let outcome = run_relist(
            &seller,
            &backend,
            &identity(&owner, &tc, expired_order_id),
            async move { successor_observed.notified().await },
            cfg,
        )
        .await;

        assert!(
            matches!(
                outcome,
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Shutdown,
                    ..
                }
            ),
            "the successor rested, so the run ends at the operator's shutdown: {outcome:?}"
        );
        assert!(
            backend.expire_calls().is_empty(),
            "cleanup that is already complete is reconciled by reading it, not by paying for a \
             second `expireOrder`"
        );
        let successor = backend
            .postings
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a successor ask was accepted");
        assert_eq!(
            successor.ticks, 512,
            "the successor carries the deal's authoritative remaining capacity"
        );
        assert_eq!(
            backend.max_rows_seen.load(Ordering::SeqCst),
            1,
            "the book never held two live offers for this deal"
        );
        assert_eq!(
            backend.posts.load(Ordering::SeqCst),
            1,
            "one successor, not one per actor that could have expired the order"
        );
        seller.server_task.abort();
    }

    /// E2E-SELL-11 /: an expiry the seller cannot prove is never a licence to post.

    /// The permissionless sweep is silent in both directions, so an `expireOrder` that changed
    /// nothing looks exactly like one that worked. Here the row never leaves the book: posting anyway
    /// would either duplicate the offer or -- because `postFromNote` drops a post while `_offerPosted`
    /// is set -- leave the seller believing it is ready with nothing on the book.
    /// E2E-ROW: E2E-SELL-11/L0
    #[tokio::test]
    async fn e2e_sell_11_an_unconfirmed_expiry_never_becomes_a_successor() {
        let owner = address('e');
        let tc = address('f');
        let backend = RelistBackend::new(&tc, &owner, 64).with_ignored_expiry();
        let expired_order_id = backend.resting_order_id().expect("the expired ask rests");
        let seller = relist_seller().await;
        let cfg = relist_cfg(&backend, &seller, 64);

        let outcome = run_relist(
            &seller,
            &backend,
            &identity(&owner, &tc, expired_order_id),
            std::future::pending(),
            cfg,
        )
        .await;

        match outcome {
            RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Expired(_),
                disposition: CancellationDisposition::UnknownFailure { known_result },
            } => {
                assert!(
                    known_result.contains("still_in_book") && known_result.contains("dexdo orders"),
                    "the terminal diagnostic names the unproven fact and one operator action: \
                     {known_result}"
                );
            }
            other => panic!("an unproven reap must fail closed, got {other:?}"),
        }
        assert_eq!(
            backend.posts.load(Ordering::SeqCst),
            0,
            "nothing was posted while the expired order was still in the book"
        );
        assert_eq!(
            backend.live_rows().len(),
            1,
            "the book still holds the one expired row and no successor beside it"
        );
        seller.server_task.abort();
    }

    /// E2E-SELL-11 /: the row is gone but `onSellClosed` has not landed, so `_offerPosted` is
    /// still set. A post now is silently dropped by the deal
    /// (`contracts/airegistry/TokenContract.sol:713`) and the seller would report readiness for an
    /// offer that does not exist. It waits for the latch, and posts only after it is released.
    /// E2E-ROW: E2E-SELL-11/L0
    #[tokio::test]
    async fn e2e_sell_11_the_successor_waits_for_the_offer_latch_to_be_released() {
        let owner = address('e');
        let tc = address('f');
        let backend = RelistBackend::new(&tc, &owner, 32).with_latch_stuck_for(4);
        let expired_order_id = backend.resting_order_id().expect("the expired ask rests");
        let seller = relist_seller().await;
        let cfg = relist_cfg(&backend, &seller, 32);
        // The latch is held for four reads, so how long the wait takes is the seller's business.
        // What the test needs is the successor actually resting, which is the signal below.
        let successor_observed = backend.successor_observed.clone();

        let outcome = run_relist(
            &seller,
            &backend,
            &identity(&owner, &tc, expired_order_id),
            async move { successor_observed.notified().await },
            cfg,
        )
        .await;

        assert!(
            matches!(
                outcome,
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Shutdown,
                    ..
                }
            ),
            "once the latch was released the successor rested and supervision resumed: {outcome:?}"
        );
        assert!(
            backend.latch_reads.load(Ordering::SeqCst) >= 4,
            "the seller kept re-reading the deal's own latch instead of assuming the callback landed"
        );
        assert_eq!(
            backend.posts.load(Ordering::SeqCst),
            1,
            "exactly one post, and it happened after `_offerPosted` was clear"
        );
        assert_eq!(
            backend.successor().ticks,
            32,
            "the accepted successor carries the deal's whole remaining capacity"
        );
        assert_eq!(
            backend.max_rows_seen.load(Ordering::SeqCst),
            1,
            "at no point did two offers rest for this deal"
        );
        seller.server_task.abort();
    }

    /// E2E-SELL-11 /: a deal that is no longer this seller's to re-offer, and a deal with less
    /// capacity than the contract's minimum fill. Neither may produce a successor.
    /// E2E-ROW: E2E-SELL-11/L0
    #[tokio::test]
    async fn e2e_sell_11_used_or_undersized_deals_never_get_a_successor() {
        let owner = address('e');
        let tc = address('f');
        let seller = relist_seller().await;

        // `_match` refuses a trade below two ticks because `fundFromOrderBook` rejects a sub-2 fund
        // (`contracts/airegistry/InferenceOrderBook.sol:1051`), so one leftover tick is capacity that
        // can never become a deal.
        for (label, backend) in [
            (
                "opened deal",
                RelistBackend::new(&tc, &owner, 8).with_deal_state(DealChainState {
                    funded: true,
                    opened: true,
                    funded_time: Some(1),
                    ..fresh_deal_state()
                }),
            ),
            (
                "sub-minimum capacity",
                RelistBackend::new(&tc, &owner, 8).with_deal_terms((1000, 1)),
            ),
            (
                "unreadable deal terms",
                RelistBackend::new(&tc, &owner, 8).with_unreadable_latch(),
            ),
        ] {
            let expired_order_id = backend.resting_order_id().expect("the expired ask rests");
            let cfg = relist_cfg(&backend, &seller, 8);
            let outcome = run_relist(
                &seller,
                &backend,
                &identity(&owner, &tc, expired_order_id),
                std::future::pending(),
                cfg,
            )
            .await;
            match outcome {
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Expired(_),
                    disposition: CancellationDisposition::ReapedNotRelisted { reason },
                } => assert!(
                    // Compare by ACCOUNT ID, not by one spelling: the message renders the canonical
                    // `<dapp>::<account>` form while `tc` holds the legacy `0:<account>` one.
                    reason.contains(tc.trim_start_matches("0:")),
                    "{label}: the refusal names the deal it refused: {reason}"
                ),
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Expired(_),
                    disposition: CancellationDisposition::UnknownFailure { known_result },
                } => assert!(
                    known_result.contains(&tc),
                    "{label}: the terminal diagnostic names the deal: {known_result}"
                ),
                other => panic!("{label} must not relist, got {other:?}"),
            }
            assert_eq!(
                backend.posts.load(Ordering::SeqCst),
                0,
                "{label}: no successor was submitted"
            );
            assert!(
                backend
                    .live_rows()
                    .iter()
                    .all(|row| row.order_id == expired_order_id),
                "{label}: the book gained no new order for this deal"
            );
        }
        seller.server_task.abort();
    }

    /// E2E-SELL-11 /: a match that landed before the deadline outranks the expiry. The deal is
    /// served, not reaped and relisted -- and the seller writes no expiry at all, because it reconciles
    /// the authoritative state before paying for a cleanup.
    /// E2E-ROW: E2E-SELL-11/L0
    #[tokio::test]
    async fn e2e_sell_11_a_match_wins_the_expiry_race_and_no_successor_is_posted() {
        let owner = address('e');
        let tc = address('f');
        let backend = RelistBackend::new(&tc, &owner, 8)
            .with_match(sample_match(&tc))
            .with_deal_state(DealChainState {
                funded: true,
                funded_time: Some(1),
                ..fresh_deal_state()
            });
        let expired_order_id = backend.resting_order_id().expect("the expired ask rests");
        let seller = relist_seller().await;
        let cfg = relist_cfg(&backend, &seller, 8);

        let outcome = run_relist(
            &seller,
            &backend,
            &identity(&owner, &tc, expired_order_id),
            std::future::pending(),
            cfg,
        )
        .await;

        match outcome {
            RestingSellerOutcome::Matched(matched) => assert_eq!(
                matched.token_contract, tc,
                "the matched deal is handed back to the match path"
            ),
            other => panic!("a matched deal must be served, not relisted, got {other:?}"),
        }
        assert!(
            backend.expire_calls().is_empty(),
            "a sold deal is never swept: the seller reads the match before it writes an expiry"
        );
        assert_eq!(
            backend.posts.load(Ordering::SeqCst),
            0,
            "no successor competes with the buyer's own deal"
        );
        seller.server_task.abort();
    }

    /// E2E-SELL-11 /: an operator shutdown that arrives after the cleanup still creates no
    /// successor. The shutdown is released by the expiry write itself, so it lands in the exact window
    /// between a completed reap and the post it authorises.
    /// E2E-ROW: E2E-SELL-11/L0
    #[tokio::test]
    async fn e2e_sell_11_a_shutdown_after_cleanup_never_creates_a_successor() {
        let owner = address('e');
        let tc = address('f');
        let backend = RelistBackend::new(&tc, &owner, 8);
        let expired_order_id = backend.resting_order_id().expect("the expired ask rests");
        let seller = relist_seller().await;
        let cfg = relist_cfg(&backend, &seller, 8);
        let reaped = backend.reaped.clone();

        let outcome = run_relist(
            &seller,
            &backend,
            &identity(&owner, &tc, expired_order_id),
            async move { reaped.notified().await },
            cfg,
        )
        .await;

        assert!(
            matches!(
                outcome,
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Shutdown,
                    ..
                }
            ),
            "the shutdown is the terminal outcome, not a relist: {outcome:?}"
        );
        assert_eq!(
            backend.posts.load(Ordering::SeqCst),
            0,
            "a normal operator shutdown never creates a successor order"
        );
        assert!(
            backend.live_rows().is_empty(),
            "the expired order was still reaped; only the successor is skipped"
        );
        seller.server_task.abort();
    }

    /// E2E-SELL-11 /: an ambiguous expiry write. The submit fails with a lost response, which
    /// says nothing about whether the book acted on it -- so the seller reconciles the read-back, and
    /// relists only because the read proves the order is gone and the latch is free.
    /// E2E-ROW: E2E-SELL-11/L0
    #[tokio::test]
    async fn e2e_sell_11_an_ambiguous_expiry_write_is_resolved_by_the_read_back() {
        let owner = address('e');
        let tc = address('f');
        let backend = RelistBackend::new(&tc, &owner, 8).with_failing_expiry();
        let expired_order_id = backend.resting_order_id().expect("the expired ask rests");
        let seller = relist_seller().await;
        let cfg = relist_cfg(&backend, &seller, 8);

        let outcome = run_relist(
            &seller,
            &backend,
            &identity(&owner, &tc, expired_order_id),
            std::future::pending(),
            cfg,
        )
        .await;

        match outcome {
            RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Expired(_),
                disposition: CancellationDisposition::UnknownFailure { known_result },
            } => assert!(
                known_result.contains("expiry_submit=failed"),
                "the diagnostic carries the write's own ambiguity: {known_result}"
            ),
            other => panic!("an unresolved expiry must fail closed, got {other:?}"),
        }
        assert_eq!(
            backend.expire_calls().len(),
            1,
            "the ambiguous write is not repeated inside one reap: a second submit could land twice"
        );
        assert_eq!(
            backend.posts.load(Ordering::SeqCst),
            0,
            "no successor while the expiry outcome is unknown"
        );
        seller.server_task.abort();
    }

    /// E2E-SELL-11 /: readiness is re-checked before the successor, not assumed from the fact
    /// that this seller was healthy an hour ago. With the gateway gone, the deal's capacity stays off
    /// the book rather than being advertised through an endpoint nobody can reach.
    /// E2E-ROW: E2E-SELL-11/L0
    #[tokio::test]
    async fn e2e_sell_11_an_unhealthy_gateway_gets_no_successor() {
        let owner = address('e');
        let tc = address('f');
        let backend = RelistBackend::new(&tc, &owner, 8);
        let expired_order_id = backend.resting_order_id().expect("the expired ask rests");
        let seller = relist_seller().await;
        let cfg = relist_cfg(&backend, &seller, 8);
        // The gateway dies while the ask is already past its deadline: supervision reaches the expiry
        // outcome first (its health cycle is an hour away), so the refusal below can only come from
        // the readiness gate in front of the successor.
        seller.server_task.abort();
        while !seller.server_task.is_finished() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let outcome = supervise_and_relist_with_timing(
            &seller,
            &backend,
            &cfg,
            &watch("relist-unhealthy").1,
            &identity(&owner, &tc, expired_order_id),
            std::future::pending(),
            relist_timing(),
        )
        .await;

        let error = outcome.expect_err("a dead gateway before the successor is a startup failure");
        assert!(
            format!("{error:#}").contains("gateway"),
            "the failure names the readiness component that refused the successor: {error:#}"
        );
        assert_eq!(
            backend.posts.load(Ordering::SeqCst),
            0,
            "no successor is advertised through a gateway that stopped answering"
        );
        assert!(
            backend.live_rows().is_empty(),
            "the expired ask was still reaped; only the successor is withheld"
        );
    }

    /// E2E-SELL-12 /: a restart at each write boundary of an expiry/relist reconciles to exactly
    /// one live order, and writes nothing twice.

    /// The entry point is the production startup path a restarting seller runs
    /// (`prepare_seller_offer_with_liveness`, called by `prepare_pool_deal` for every pool deal), not
    /// an internal recovery helper: the reconciliation under test is its authoritative
    /// `inspect_seller_offer` read, and what it decides is asserted on the book.
    /// E2E-ROW: E2E-SELL-12/L0
    #[tokio::test]
    async fn e2e_sell_12_a_restart_at_each_relist_write_boundary_creates_no_duplicate() {
        let owner = address('e');
        let tc = address('f');
        let seller = relist_seller().await;

        // Boundary 1: the expiry write landed and the process died before posting. The deal is
        // unsold, its latch is free, and the book is empty for this TC.
        let after_expiry = RelistBackend::new(&tc, &owner, 128);
        let reaped_id = after_expiry
            .resting_order_id()
            .expect("the expired ask rests");
        after_expiry
            .expire_resting_sell_order(&tc, reaped_id)
            .await
            .expect("the permissionless sweep landed before the crash");
        let startup = prepare_seller_offer_with_timing(
            &seller,
            &after_expiry,
            &relist_cfg(&after_expiry, &seller, 128),
            &owner,
            None,
            std::future::pending(),
            relist_timing(),
        )
        .await
        .expect("restart startup returns an outcome");
        match startup {
            SellerStartupOutcome::Ready(SellerOfferStartup::Posted {
                outcome: Some(dexdo_core::SellOfferOutcome::Rested { order_id }),
            }) => assert_ne!(
                order_id, reaped_id,
                "the restart posts a NEW order rather than resurrecting the reaped id"
            ),
            other => panic!("a reaped deal must be re-offered once on restart, got {other:?}"),
        }
        assert_eq!(
            after_expiry.posts.load(Ordering::SeqCst),
            1,
            "exactly one post after the restart"
        );
        assert_eq!(
            after_expiry.live_rows().len(),
            1,
            "the book holds exactly one live order for this deal"
        );
        assert_eq!(
            after_expiry.max_rows_seen.load(Ordering::SeqCst),
            1,
            "no duplicate ever coexisted"
        );

        // Boundary 2: the successor write landed and the process died before recording it. The
        // restart must adopt that exact order and submit nothing.
        let after_successor = RelistBackend::new(&tc, &owner, 128);
        let reaped_id = after_successor
            .resting_order_id()
            .expect("the expired ask rests");
        after_successor
            .expire_resting_sell_order(&tc, reaped_id)
            .await
            .expect("the sweep landed");
        let successor_id = after_successor.rest_offer(
            unix_timestamp() + dexdo_core::params::MAX_SELL_TTL.as_secs(),
            128,
        );
        let startup = prepare_seller_offer_with_timing(
            &seller,
            &after_successor,
            &relist_cfg(&after_successor, &seller, 128),
            &owner,
            None,
            std::future::pending(),
            relist_timing(),
        )
        .await
        .expect("restart startup returns an outcome");
        assert!(
            matches!(
                startup,
                SellerStartupOutcome::Ready(SellerOfferStartup::ResumedResting { order_id })
                    if order_id == successor_id
            ),
            "the restart adopts the exact accepted successor, got {startup:?}"
        );
        assert_eq!(
            after_successor.posts.load(Ordering::SeqCst),
            0,
            "an accepted successor is adopted, never posted again"
        );
        assert_eq!(
            after_successor.live_rows().len(),
            1,
            "still exactly one live order for this deal"
        );

        // Boundary 3: the crash happened between the deadline and the expiry write, so the reaped
        // generation is still in the book. The restart adopts it rather than posting beside it, and
        // the relist loop reaps it exactly once from there.
        let before_expiry = RelistBackend::new(&tc, &owner, 128);
        let stale_id = before_expiry
            .resting_order_id()
            .expect("the expired ask rests");
        let startup = prepare_seller_offer_with_timing(
            &seller,
            &before_expiry,
            &relist_cfg(&before_expiry, &seller, 128),
            &owner,
            None,
            std::future::pending(),
            relist_timing(),
        )
        .await
        .expect("restart startup returns an outcome");
        assert!(
            matches!(
                startup,
                SellerStartupOutcome::Ready(SellerOfferStartup::ResumedResting { order_id })
                    if order_id == stale_id
            ),
            "an expired row still in the book is adopted, not doubled, got {startup:?}"
        );
        assert_eq!(
            before_expiry.posts.load(Ordering::SeqCst),
            0,
            "no second offer is posted beside the un-swept generation"
        );
        // Stop only after the authoritative book read has observed the successor. The invariant is
        // about chain state, so scheduler time must not decide whether the relist gets that far.
        let successor_observed = before_expiry.successor_observed.clone();
        let outcome = run_relist(
            &seller,
            &before_expiry,
            &identity(&owner, &tc, stale_id),
            async move { successor_observed.notified().await },
            relist_cfg(&before_expiry, &seller, 128),
        )
        .await;
        assert!(
            matches!(
                outcome,
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Shutdown,
                    ..
                }
            ),
            "the adopted generation is reaped and replaced once: {outcome:?}"
        );
        assert_eq!(
            before_expiry.expire_calls(),
            vec![stale_id],
            "exactly one expiry write, for the adopted order"
        );
        assert_eq!(
            before_expiry.posts.load(Ordering::SeqCst),
            1,
            "exactly one successor after the restart"
        );
        assert_eq!(
            before_expiry.max_rows_seen.load(Ordering::SeqCst),
            1,
            "at no point did two offers rest for this deal"
        );
        seller.server_task.abort();
    }

    /// E2E-SELL-10 / (invariant): however many deadlines a healthy seller lives through, the
    /// book holds at most one live offer for its deal, each generation's successor carries a strictly
    /// later deadline and a strictly newer id than the ask it replaced, and the capacity offered
    /// never changes -- the deal's own `getDeal()` is the only source it is read from.

    /// Between generations the run is ended by a shutdown, which cancels the live successor, so each
    /// following generation is re-seeded as the chain would present it: one resting ask, past its
    /// deadline, still in the book because expiry removal is lazy. The gateway is renewed with it:
    /// the invariants below are the book's, and holding one TLS server open across the whole suite
    /// only buys a way for this test to fail for a reason it is not about.
    /// E2E-ROW: E2E-SELL-10/L0
    #[tokio::test]
    async fn relisting_conserves_capacity_and_never_doubles_the_live_offer() {
        let owner = address('e');
        let tc = address('f');
        let backend = RelistBackend::new(&tc, &owner, 256);
        let mut reaped = backend.live_rows()[0].clone();

        for generation in 1..=4_u64 {
            let seller = relist_seller().await;
            let cfg = relist_cfg(&backend, &seller, 256);
            // End the generation on THIS generation's successor, and on nothing else.

            // `successor_observed` is a `notify_one`, so it stores a permit when nobody is waiting:
            // a leftover from the previous generation would satisfy the wait before this one had
            // posted anything. The post count is what makes the wait about this generation's fact
            // rather than about any notification -- the notify only wakes the check.
            let successor_observed = backend.successor_observed.clone();
            let posts = &backend.posts;
            let outcome = run_relist(
                &seller,
                &backend,
                &identity(&owner, &tc, reaped.order_id),
                async move {
                    while posts.load(Ordering::SeqCst) < generation {
                        successor_observed.notified().await;
                    }
                },
                cfg,
            )
            .await;
            assert!(
                matches!(
                    outcome,
                    RestingSellerOutcome::Stopped {
                        reason: RestingStopReason::Shutdown,
                        ..
                    }
                ),
                "generation {generation} ends with a live successor and an operator shutdown: \
                 {outcome:?}"
            );
            let successor = backend
                .postings
                .lock()
                .unwrap()
                .last()
                .cloned()
                .expect("each generation accepts one successor");
            assert!(
                successor.order_id > reaped.order_id,
                "generation {generation}: a reaped id is never reused ({} after {})",
                successor.order_id,
                reaped.order_id
            );
            assert!(
                successor.deadline > reaped.deadline,
                "generation {generation}: the successor outlives what it replaced ({} after {})",
                successor.deadline,
                reaped.deadline
            );
            assert_eq!(
                successor.ticks, 256,
                "generation {generation}: capacity is conserved"
            );
            assert_eq!(
                backend.posts.load(Ordering::SeqCst),
                generation,
                "exactly one successor per expired generation"
            );
            assert_eq!(
                backend.max_rows_seen.load(Ordering::SeqCst),
                1,
                "generation {generation}: the book never held two live offers for this deal"
            );
            assert_eq!(
                backend.expire_calls().len(),
                generation as usize,
                "one expiry write per generation, targeting that generation's own order"
            );
            assert!(
                backend.live_rows().is_empty(),
                "the shutdown left no resting offer behind before the next generation is seeded"
            );
            // Seed the next generation already expired at construction: the relist invariant must
            // not depend on production idling after it has read an authoritative terminal fact.
            let next_deadline = unix_timestamp().saturating_sub(1);
            let next_id = backend.rest_offer(next_deadline, 256);
            reaped = backend
                .live_rows()
                .into_iter()
                .find(|row| row.order_id == next_id)
                .expect("the seeded ask rests");
            seller.server_task.abort();
        }
    }

    /// E2E-ROW: E2E-SELL-03/L0
    #[tokio::test]
    async fn gateway_task_death_cancels_within_one_health_cycle() {
        let owner = address('1');
        let tc = address('2');
        let id = identity(&owner, &tc, 101);
        let backend = CancelBackend::new(
            vec![order(id.order_id, &owner, &tc)],
            owner.clone(),
            id.order_id,
            CancelBehavior::Remove,
        );
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        seller.server_task.abort();
        tokio::task::yield_now().await;

        let outcome = supervise_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &watch("gateway-death").1,
            &id,
            std::future::pending(),
            fast_timing(),
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Health(HealthFailure {
                    component: HealthComponent::GatewayTask,
                    ..
                }),
                disposition: CancellationDisposition::Cancelled,
            }
        ));
        assert_eq!(backend.calls(), vec![(tc.clone(), id.order_id)]);
    }

    #[tokio::test]
    async fn health_check_and_cancel_share_one_cycle_deadline() {
        let owner = address('1');
        let tc = address('3');
        let id = identity(&owner, &tc, 111);
        let backend = CancelBackend::new(
            vec![order(id.order_id, &owner, &tc)],
            owner,
            id.order_id,
            CancelBehavior::Hang,
        );
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        seller.server_task.abort();
        tokio::task::yield_now().await;
        let _ = take_observed_cycle_deadlines();

        let outcome = supervise_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &watch("shared-deadline").1,
            &id,
            std::future::pending(),
            SupervisionTiming {
                health_interval: Duration::from_millis(5),
                health_timeout: Duration::from_millis(10),
                cycle_timeout: Duration::from_millis(30),
                cancel_poll: Duration::from_millis(1),
                expiry_poll: Duration::from_secs(3_600),
                abort_gateway_on_stop: true,
                advertise_probe: AdvertiseProbePolicy::default(),
                ..fast_timing()
            },
        )
        .await
        .unwrap();

        // the property this test is named after, asserted as itself.

        // The cancellation must run against the deadline the readiness check was already bounded
        // by, not against a fresh one -- one cycle deadline, read twice. This used to be inferred
        // from `started.elapsed() < 80ms`, on the reasoning that two deadlines would take about
        // twice as long. A wall clock cannot tell "two deadlines" apart from "this thread was
        // descheduled", so inside the full binary it reported the second and blamed the first.

        // Deliberately not "exactly one health observation": how many readiness cycles run before
        // one fails is a scheduling detail, and pinning it would rebuild the flakiness this
        // replaces. What must hold is that the cancellation shares the deadline of the cycle that
        // decided to cancel -- the one immediately before it.
        let observed = take_observed_cycle_deadlines();
        let cancels: Vec<_> = observed
            .iter()
            .filter(|(site, _)| *site == CycleDeadlineSite::Cancel)
            .collect();
        assert_eq!(
            cancels.len(),
            1,
            "exactly one cancellation was expected for one failed cycle; observed {observed:?}"
        );
        let cancel_index = observed
            .iter()
            .position(|(site, _)| *site == CycleDeadlineSite::Cancel)
            .expect("the cancellation observation was just counted");
        let (_, cancel_deadline) = observed[cancel_index];
        let (_, deciding_health_deadline) = *observed[..cancel_index]
            .iter()
            .rev()
            .find(|(site, _)| *site == CycleDeadlineSite::HealthCheck)
            .unwrap_or_else(|| {
                panic!(
                    "the cancellation must follow the readiness check that triggered it; \
                     observed {observed:?}"
                )
            });
        assert_eq!(
            cancel_deadline, deciding_health_deadline,
            "the health check and the cancel must share ONE cycle deadline; the cancel started a \
             second one, which gives the cycle twice its budget. Observed {observed:?}"
        );

        let known_result = match outcome {
            RestingSellerOutcome::Stopped {
                reason:
                    RestingStopReason::Health(HealthFailure {
                        component: HealthComponent::GatewayTask,
                        ..
                    }),
                disposition: CancellationDisposition::UnknownFailure { known_result },
            } => known_result,
            other => panic!("expected structured unknown_failure, got {other:?}"),
        };
        assert!(known_result.contains("budget_ms=30"));
        assert!(known_result.contains("cancel resting order 111"));
        assert!(!known_result.contains("--order-id"));
    }

    /// E2E-SELL-03 -- a deterministic upstream rejection cancels the exact id at occurrence one.
    /// `timed_out=false` is the structured class distinction from E2E-SELL-01/02 timeouts; the
    /// existing gateway-task and watcher tests pin the other deterministic classes.

    /// E2E-SELL-03, `tests/e2e/test-specification.md`.
    #[tokio::test]
    async fn e2e_sell_03_deterministic_failure_cancels_exact_offer_once() {
        let (base_url, server) = http_server(
            "401 Unauthorized",
            "{\"error\":{\"message\":\"credential rejected\"}}".to_string(),
            Duration::ZERO,
        )
        .await;
        let owner = address('3');
        let tc = address('4');
        let id = identity(&owner, &tc, 102);
        let backend = CancelBackend::new(
            vec![order(id.order_id, &owner, &tc)],
            owner,
            id.order_id,
            CancelBehavior::Remove,
        );
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            openai(base_url),
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();

        let outcome = supervise_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &watch("upstream-failure").1,
            &id,
            std::future::pending(),
            fast_timing(),
        )
        .await
        .unwrap();

        assert!(matches!(
            &outcome,
            RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Health(HealthFailure {
                    component: HealthComponent::UpstreamModel,
                    timed_out: false,
                    ..
                }),
                disposition: CancellationDisposition::Cancelled,
            }
        ));
        assert_eq!(backend.calls(), vec![(tc, id.order_id)]);
        assert!(backend.order_ids().is_empty());
        assert_eq!(backend.posts.load(Ordering::Relaxed), 0);
        server.abort();
    }

    #[tokio::test]
    async fn failed_restart_readiness_cancels_existing_resting_sell_without_post() {
        let (base_url, upstream_server) = http_server(
            "401 Unauthorized",
            "{\"error\":{\"message\":\"credential rejected\"}}".to_string(),
            Duration::ZERO,
        )
        .await;
        let owner = address('e');
        let tc = address('f');
        let id = identity(&owner, &tc, 107);
        let backend = CancelBackend::new(
            vec![order(id.order_id, &owner, &tc)],
            owner,
            id.order_id,
            CancelBehavior::Remove,
        );
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            openai(base_url),
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();

        let failure = check_readiness(
            &seller,
            &seller.listen_addr.to_string(),
            Duration::from_secs(1),
            Some(&id),
            &tc,
            AdvertiseProbePolicy::default(),
        )
        .await
        .expect_err("restart readiness must fail");
        assert_eq!(failure.component, HealthComponent::UpstreamModel);
        let disposition = cancel_and_confirm_with_timing(
            &backend,
            &cfg(&tc),
            &id,
            Duration::from_millis(20),
            Duration::from_millis(1),
        )
        .await;

        assert!(matches!(disposition, CancellationDisposition::Cancelled));
        assert_eq!(backend.posts.load(Ordering::Relaxed), 0);
        assert!(backend.order_ids().is_empty());
        seller.server_task.abort();
        upstream_server.abort();
    }

    #[tokio::test]
    async fn graceful_shutdown_confirms_cancel_before_return() {
        let owner = address('5');
        let tc = address('6');
        let id = identity(&owner, &tc, 103);
        let backend = CancelBackend::new(
            vec![order(id.order_id, &owner, &tc)],
            owner,
            id.order_id,
            CancelBehavior::Remove,
        );
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();

        let outcome = supervise_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &watch("shutdown").1,
            &id,
            std::future::ready(()),
            fast_timing(),
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Shutdown,
                disposition: CancellationDisposition::Cancelled,
            }
        ));
        assert!(backend.order_ids().is_empty());
        tokio::task::yield_now().await;
        assert!(seller.server_task.is_finished());
    }

    #[tokio::test]
    async fn watcher_error_cancels_exact_resting_offer_before_return() {
        let owner = address('5');
        let tc = address('7');
        let id = identity(&owner, &tc, 108);
        let backend = CancelBackend::new(
            vec![order(id.order_id, &owner, &tc)],
            owner,
            id.order_id,
            CancelBehavior::Remove,
        )
        .with_watcher_error();
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();

        let outcome = supervise_with_timing(
            &seller,
            &backend,
            &cfg_for_seller(&tc, &seller),
            &watch("watcher-error").1,
            &id,
            std::future::pending(),
            fast_timing(),
        )
        .await
        .unwrap();

        assert!(matches!(
            outcome,
            RestingSellerOutcome::Stopped {
                reason: RestingStopReason::Watcher(ref error),
                disposition: CancellationDisposition::Cancelled,
            } if error.contains("authoritative match watcher failed")
        ));
        assert_eq!(backend.calls(), vec![(tc, id.order_id)]);
        assert!(backend.order_ids().is_empty());
        tokio::task::yield_now().await;
        assert!(seller.server_task.is_finished());
    }

    #[tokio::test]
    async fn shutdown_match_race_waits_for_delayed_tc_visibility_and_preserves_control() {
        let owner = address('7');
        let tc = address('8');
        let target = identity(&owner, &tc, 104);
        let other_tc = address('9');
        let backend = CancelBackend::new(
            vec![
                order(target.order_id, &owner, &tc),
                order(999, &owner, &other_tc),
            ],
            owner,
            target.order_id,
            CancelBehavior::Remove,
        )
        .with_open_delay(Duration::from_millis(5));
        backend
            .orders
            .lock()
            .unwrap()
            .retain(|order| order.order_id != target.order_id);
        let matched = backend.matched.clone();
        let matched_tc = tc.clone();
        let visibility = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            matched.lock().unwrap().replace(sample_match(&matched_tc));
        });
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            UpstreamConfig::Mock,
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let shutdown = std::future::ready(()).fuse();
        tokio::pin!(shutdown);
        let (_cursor_dir, watch) = watch("match-cancel-race");
        let cfg = cfg_for_seller(&tc, &seller);

        let outcome = supervise_with_timing(
            &seller,
            &backend,
            &cfg,
            &watch,
            &target,
            shutdown.as_mut(),
            fast_timing(),
        )
        .await
        .unwrap();

        let matched = match outcome {
            RestingSellerOutcome::Matched(matched) => matched,
            stopped => panic!("expected match, got {stopped:?}"),
        };
        assert_eq!(matched.token_contract, tc);
        super::super::serve_watched_match(&seller, &backend, &cfg, &watch, matched)
            .await
            .unwrap();
        assert_eq!(backend.opens.load(Ordering::Relaxed), 1);
        assert!(
            shutdown.is_terminated(),
            "the initiating shutdown must remain observable after handover"
        );
        assert!(backend.calls().is_empty());
        assert_eq!(backend.order_ids(), vec![999]);
        assert!(!seller.server_task.is_finished());
        visibility.await.unwrap();
        seller.server_task.abort();
    }

    #[tokio::test]
    async fn rejected_or_unconfirmed_cancel_never_reports_success() {
        for (behavior, expected_result) in
            [(CancelBehavior::Reject, "cancel rejected by owner check")]
        {
            let owner = address('a');
            let tc = address('b');
            let id = identity(&owner, &tc, 105);
            let backend = CancelBackend::new(
                vec![order(id.order_id, &owner, &tc)],
                owner.clone(),
                id.order_id,
                behavior,
            );

            let disposition = cancel_and_confirm_with_timing(
                &backend,
                &cfg(&tc),
                &id,
                Duration::from_millis(5),
                Duration::from_millis(1),
            )
            .await;
            let known_result = match disposition {
                CancellationDisposition::UnknownFailure { known_result } => known_result,
                other => panic!("present order cannot be reported terminal: {other:?}"),
            };

            assert!(known_result.contains(expected_result));
            assert!(known_result.contains("authoritative_state=present"));
            assert!(known_result.contains("operator_action="));
            // the guidance names the order to cancel and the command, without printing a
            // line that cannot run (`orders` needs identity and market/model this module lacks).
            assert!(known_result.contains("cancel resting order 105"));
            assert!(known_result.contains("`dexdo orders cancel`"));
            assert!(!known_result.contains("--order-id"));
            assert_eq!(backend.order_ids(), vec![id.order_id]);
        }

        let owner = address('a');
        let tc = address('b');
        let id = identity(&owner, &tc, 105);
        let backend = CancelBackend::new(
            vec![order(id.order_id, &owner, &tc)],
            owner,
            id.order_id,
            CancelBehavior::RemoveAfterReads(3),
        );

        let disposition = tokio::time::timeout(
            Duration::from_millis(100),
            cancel_and_confirm_with_timing(
                &backend,
                &cfg(&tc),
                &id,
                Duration::from_millis(2),
                Duration::from_millis(1),
            ),
        )
        .await
        .expect("an accepted cancel must keep watching past its pre-acceptance deadline");
        assert!(matches!(disposition, CancellationDisposition::Cancelled));
        assert!(
            backend.post_submit_reads.load(Ordering::SeqCst) >= 4,
            "the exact row must be observed present several times before its removal"
        );
        assert!(backend.order_ids().is_empty());

        let owner = address('a');
        let tc = address('b');
        let id = identity(&owner, &tc, 105);
        let backend = CancelBackend::new(
            vec![order(id.order_id, &owner, &tc)],
            owner,
            id.order_id,
            CancelBehavior::TerminalReject(2),
        );

        let disposition = tokio::time::timeout(
            Duration::from_millis(100),
            cancel_and_confirm_with_timing(
                &backend,
                &cfg(&tc),
                &id,
                Duration::from_millis(2),
                Duration::from_millis(1),
            ),
        )
        .await
        .expect("a terminal chain rejection must end the accepted-cancel watch");
        assert_eq!(disposition.as_str(), "rejected_still_resting");
        let known_result = disposition
            .known_result()
            .expect("a still-resting terminal rejection needs operator guidance");
        assert!(known_result.contains("InferenceOrderCancelRejected"));
        assert!(known_result.contains("reason=2"));
        assert!(known_result.contains("operator_action="));
        assert!(known_result.contains("cancel resting order 105"));
        assert_eq!(backend.order_ids(), vec![id.order_id]);
    }

    #[tokio::test]
    async fn cancellation_deadline_bounds_initial_read_and_submit() {
        for (hang_reads, behavior) in [
            (true, CancelBehavior::Remove),
            (false, CancelBehavior::Hang),
        ] {
            let owner = address('a');
            let tc = address('c');
            let id = identity(&owner, &tc, 110);
            let mut backend = CancelBackend::new(
                vec![order(id.order_id, &owner, &tc)],
                owner,
                id.order_id,
                behavior,
            );
            if hang_reads {
                backend = backend.with_hanging_reads();
            }

            let disposition = tokio::time::timeout(
                Duration::from_millis(200),
                cancel_and_confirm_with_timing(
                    &backend,
                    &cfg(&tc),
                    &id,
                    Duration::from_millis(20),
                    Duration::from_millis(1),
                ),
            )
            .await
            .expect("production cancellation must honor its own hard deadline");
            let known_result = match disposition {
                CancellationDisposition::UnknownFailure { known_result } => known_result,
                other => panic!("a hung cancellation cannot report success: {other:?}"),
            };

            assert!(known_result.contains("budget_ms=20"));
            assert!(known_result.contains("operator_action="));
            assert!(known_result.contains("cancel resting order 110"));
            assert!(!known_result.contains("--order-id"));
        }
    }

    #[tokio::test]
    async fn ambiguous_submit_reconciles_by_fact_without_false_failure() {
        let owner = address('c');
        let tc = address('d');
        let id = identity(&owner, &tc, 106);
        let backend = CancelBackend::new(
            vec![order(id.order_id, &owner, &tc)],
            owner,
            id.order_id,
            CancelBehavior::AmbiguousRemove,
        );

        let disposition = cancel_and_confirm_with_timing(
            &backend,
            &cfg(&tc),
            &id,
            Duration::from_millis(20),
            Duration::from_millis(1),
        )
        .await;

        assert!(matches!(
            disposition,
            CancellationDisposition::AlreadyAbsent
        ));
    }

    // ---------------------------------------------------------------------------------------
    // SELL -- seller supervision. The fixture below belongs to E2E-SELL-01.
    // ---------------------------------------------------------------------------------------

    /// A fake model server that answers a different way each time it is asked, and counts how
    /// many times it was asked.

    /// One script entry per accepted connection. The last entry repeats for every request past the
    /// end of the script, so `vec![failing]` is a permanently broken model and
    /// `vec![failing, healthy]` is one that hiccups once.
    fn scripted_http_server(
        script: Vec<(&'static str, String, Duration)>,
    ) -> (String, Arc<AtomicU64>, tokio::task::JoinHandle<()>) {
        assert!(
            !script.is_empty(),
            "the probe script needs at least one arm"
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind scripted upstream");
        listener
            .set_nonblocking(true)
            .expect("scripted upstream must be non-blocking for tokio");
        let addr = listener.local_addr().expect("scripted upstream address");
        let listener =
            tokio::net::TcpListener::from_std(listener).expect("adopt the scripted upstream");
        let probes = Arc::new(AtomicU64::new(0));
        let counter = probes.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let index = counter.fetch_add(1, Ordering::SeqCst) as usize;
                let (status, body, delay) = script[std::cmp::min(index, script.len() - 1)].clone();
                tokio::spawn(async move {
                    let mut request = vec![0_u8; 8192];
                    let _ = socket.read(&mut request).await;
                    tokio::time::sleep(delay).await;
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), probes, task)
    }

    /// An offer sitting in the market that expires at a stated moment.

    /// `order()` above always says zero, which for a sell offer means malformed rather than "never
    /// expires", so anything about expiry needs a real second here.
    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the epoch")
            .as_secs()
    }

    /// E2E-SELL-01 -- when the model is slow to answer once and fine the next time, the seller's
    /// offer stays on sale.

    /// Setup: an offer already on sale, a model that answers the first check too slowly and the
    /// second check normally. Do: run the seller's periodic checking. Observe: the offer with that
    /// exact number is still listed, no withdrawal was submitted, and the model was asked more
    /// than once.

    /// E2E-SELL-01, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-SELL-01/L0
    #[tokio::test]
    async fn e2e_sell_01_transient_timeout_leaves_exact_order_resting() {
        let (base_url, probes, upstream) = scripted_http_server(vec![
            // Probe 1: answers far past the per-cycle health bound below -> a TIMEOUT class fault.
            ("200 OK", healthy_sse(), Duration::from_millis(400)),
            // Probe 2: the same endpoint, healthy and immediate -- as measured it afterwards.
            ("200 OK", healthy_sse(), Duration::ZERO),
            // Probe 3: observing this request proves probe 2 completed successfully and supervision
            // entered another health cycle; the counter increments before a response is processed.
            ("200 OK", healthy_sse(), Duration::ZERO),
        ]);
        let owner = address('1');
        let tc = address('2');
        let id = identity(&owner, &tc, 8);
        let backend = CancelBackend::new(
            vec![order_with_deadline(
                id.order_id,
                &owner,
                &tc,
                now_unix() + dexdo_core::params::MAX_SELL_TTL.as_secs(),
            )],
            owner.clone(),
            id.order_id,
            CancelBehavior::Remove,
        );
        let seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            openai(base_url),
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();

        let timing = SupervisionTiming {
            health_interval: Duration::from_millis(20),
            health_timeout: Duration::from_millis(120),
            cycle_timeout: Duration::from_millis(600),
            cancel_poll: Duration::from_millis(1),
            expiry_poll: Duration::from_millis(1),
            abort_gateway_on_stop: true,
            advertise_probe: AdvertiseProbePolicy::default(),
            ..fast_timing()
        };
        let config = cfg_for_seller(&tc, &seller);
        let (_watch_dir, watch_config) = watch("sell01-transient");
        {
            let supervisor = supervise_with_timing(
                &seller,
                &backend,
                &config,
                &watch_config,
                &id,
                std::future::pending(),
                timing,
            );
            tokio::pin!(supervisor);

            tokio::select! {
                _outcome = &mut supervisor => {
                    panic!("E2E-SELL-01 supervisor returned before the third upstream probe");
                }
                observed = tokio::time::timeout(Duration::from_secs(5), async {
                    while probes.load(Ordering::SeqCst) < 3 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }) => {
                    observed.unwrap_or_else(|_| {
                        panic!("E2E-SELL-01 timed out waiting for the third upstream probe")
                    });
                }
            }

            assert_eq!(
                backend.order_ids(),
                vec![id.order_id],
                "E2E-SELL-01 exact order must still rest after the healthy retry"
            );
            assert!(
                backend.calls().is_empty(),
                "E2E-SELL-01 must submit no cancellation before the healthy retry completes"
            );
        }
        seller.server_task.abort();
        upstream.abort();
    }

    /// E2E-SELL-01 diagnostic contract -- the first timeout classification carries one-based
    /// `attempt=1`, the exact transient failure class, and a positive remaining budget. This is a
    /// schema/semantics oracle only; it claims neither a canonical numeric value nor provenance.

    /// E2E-SELL-01, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-SELL-01/L0
    #[ignore = "EXPECTED TO FAIL until timeout health failures expose first attempt class and positive remaining budget"]
    #[tokio::test]
    async fn e2e_sell_01_timeout_diagnostic_carries_attempt_class_and_budget() {
        let owner = address('1');
        let tc = address('2');
        let id = identity(&owner, &tc, 8);
        let (diagnostic_url, diagnostic_server) =
            http_server("200 OK", healthy_sse(), Duration::from_millis(400)).await;
        let diagnostic_seller = super::super::start_gateway_with_note(
            "127.0.0.1:0".parse().unwrap(),
            openai(diagnostic_url),
            Arc::new(LocalNote::generate()),
        )
        .await
        .unwrap();
        let failure = check_readiness(
            &diagnostic_seller,
            &diagnostic_seller.listen_addr.to_string(),
            Duration::from_millis(120),
            Some(&id),
            &tc,
            AdvertiseProbePolicy::Required,
        )
        .await
        .expect_err("the scripted probe is slower than the readiness bound");
        let fields = failure
            .detail
            .split_ascii_whitespace()
            .filter_map(|field| field.split_once('='))
            .collect::<Vec<_>>();
        assert!(
            fields.len() == 3
                && fields[0] == ("attempt", "1")
                && fields[1] == ("failure_class", "timeout")
                && fields[2].0 == "remaining_budget"
                && fields[2]
                    .1
                    .parse::<u64>()
                    .is_ok_and(|remaining| remaining > 0),
            "E2E-SELL-01 timeout diagnostic omitted attempt class or remaining budget"
        );
        diagnostic_seller.server_task.abort();
        diagnostic_server.abort();
    }

    #[tokio::test]
    async fn upstream_health_diagnostic_never_echoes_secret() {
        let secret = std::env::var("PATH").unwrap();
        let (base_url, server) = http_server(
            "401 Unauthorized",
            format!(
                "{{\"error\":{{\"message\":{}}}}}",
                serde_json::to_string(&secret).unwrap()
            ),
            Duration::ZERO,
        )
        .await;

        let error = openai(base_url)
            .check_health()
            .await
            .expect_err("rejected credential")
            .to_string();

        assert!(!error.contains(&secret));
        server.abort();
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn failed_health_cycle_cancels_the_captured_owner_tc_order_tuple(
            order_id in 1_u128..u128::MAX,
            owner_digit in 1_u8..=9,
            tc_digit in 10_u8..=15,
        ) {
            let owner_char = char::from_digit(u32::from(owner_digit), 16).unwrap();
            let tc_char = char::from_digit(u32::from(tc_digit), 16).unwrap();
            let owner = address(owner_char);
            let tc = address(tc_char);
            let id = identity(&owner, &tc, order_id);
            let backend = CancelBackend::new(
                vec![order(order_id, &owner, &tc)],
                owner.clone(),
                order_id,
                CancelBehavior::Remove,
            );
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let outcome = runtime.block_on(async {
                let seller = super::super::start_gateway_with_note(
                    "127.0.0.1:0".parse().unwrap(),
                    UpstreamConfig::Mock,
                    Arc::new(LocalNote::generate()),
                )
                .await
                .unwrap();
                seller.server_task.abort();
                tokio::task::yield_now().await;
                supervise_with_timing(
                    &seller,
                    &backend,
                    &cfg_for_seller(&tc, &seller),
                    &watch(&format!("property-{order_id}")).1,
                    &id,
                    std::future::pending(),
                    fast_timing(),
                )
                .await
                .unwrap()
            });

            let stopped_after_health_failure = matches!(
                outcome,
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Health(HealthFailure {
                        component: HealthComponent::GatewayTask,
                        ..
                    }),
                    disposition: CancellationDisposition::Cancelled,
                }
            );
            prop_assert!(stopped_after_health_failure);
            prop_assert_eq!(backend.calls(), vec![(tc, order_id)]);
            prop_assert!(backend.order_ids().is_empty());
        }
    }

    mod issue_1168_tests {
        use super::*;
        use dexdo_core::params::{TRANSIENT_READ_ATTEMPT_TIMEOUT, TRANSIENT_READ_TOTAL_BUDGET};
        use std::collections::VecDeque;
        use std::io::{self, Write};

        enum ExpiryReadStep {
            Fail,
            Live,
            Expired,
            SlowExpired(Duration),
            Pending(Arc<tokio::sync::Notify>),
        }

        enum PendingWatcherOutcome {
            Match(Match),
            Error(&'static str),
        }

        struct ScriptedExpiryBackend {
            attempts: AtomicU64,
            steps: Mutex<VecDeque<ExpiryReadStep>>,
            live: OrderBookOrder,
            expired: OrderBookOrder,
            watcher_release: Option<Arc<tokio::sync::Notify>>,
            watcher_outcome: Option<PendingWatcherOutcome>,
        }

        impl ScriptedExpiryBackend {
            fn new(
                owner: &str,
                token_contract: &str,
                order_id: u128,
                steps: impl IntoIterator<Item = ExpiryReadStep>,
            ) -> Self {
                Self {
                    attempts: AtomicU64::new(0),
                    steps: Mutex::new(steps.into_iter().collect()),
                    live: order_with_deadline(
                        order_id,
                        owner,
                        token_contract,
                        unix_timestamp() + dexdo_core::params::MAX_SELL_TTL.as_secs(),
                    ),
                    expired: order_with_deadline(
                        order_id,
                        owner,
                        token_contract,
                        unix_timestamp().saturating_sub(1),
                    ),
                    watcher_release: None,
                    watcher_outcome: None,
                }
            }

            fn with_watcher(
                mut self,
                release: Arc<tokio::sync::Notify>,
                outcome: PendingWatcherOutcome,
            ) -> Self {
                self.watcher_release = Some(release);
                self.watcher_outcome = Some(outcome);
                self
            }

            fn attempts(&self) -> u64 {
                self.attempts.load(Ordering::SeqCst)
            }
        }

        #[async_trait::async_trait]
        impl ChainBackend for ScriptedExpiryBackend {
            async fn discover_offers(&self) -> Result<Vec<OfferListing>, ChainError> {
                Ok(Vec::new())
            }

            async fn post_offer(&self, _: SellOffer, _: &dyn Note) -> Result<(), ChainError> {
                unimplemented!()
            }

            async fn sell_offer_terms(
                &self,
                _: &TokenContract,
            ) -> Result<Option<(u64, u64)>, ChainError> {
                Ok(Some((
                    u64::try_from(self.live.price_per_tick).unwrap(),
                    u64::try_from(self.live.ticks).unwrap(),
                )))
            }

            async fn raw_resting_sell_orders_for_tc(
                &self,
                _: &TokenContract,
            ) -> Result<Vec<OrderBookOrder>, ChainError> {
                self.attempts.fetch_add(1, Ordering::SeqCst);
                let step = self.steps.lock().unwrap().pop_front().ok_or_else(|| {
                    ChainError::Chain("unexpected authoritative deadline read".to_string())
                })?;
                match step {
                    ExpiryReadStep::Fail => Err(ChainError::Transport(
                        "scripted authoritative deadline read failure".to_string(),
                    )),
                    ExpiryReadStep::Live => Ok(vec![self.live.clone()]),
                    ExpiryReadStep::Expired => Ok(vec![self.expired.clone()]),
                    ExpiryReadStep::SlowExpired(delay) => {
                        tokio::time::sleep(delay).await;
                        Ok(vec![self.expired.clone()])
                    }
                    ExpiryReadStep::Pending(started) => {
                        started.notify_one();
                        std::future::pending().await
                    }
                }
            }

            async fn poll_seller_fills(
                &self,
                _: &dyn Note,
                _: &mut dexdo_core::MatchWatchCursor,
            ) -> Result<Vec<dexdo_core::MatchedFill>, ChainError> {
                if let Some(release) = &self.watcher_release {
                    release.notified().await;
                    return match self
                        .watcher_outcome
                        .as_ref()
                        .expect("scripted watcher release requires an outcome")
                    {
                        PendingWatcherOutcome::Match(matched) => {
                            Ok(vec![dexdo_core::MatchedFill {
                                order_id: self.live.order_id,
                                token_contract: matched.token_contract.clone(),
                                ticks: self.live.ticks,
                                price_per_tick: u128::from(matched.price_per_tick),
                            }])
                        }
                        PendingWatcherOutcome::Error(error) => {
                            Err(ChainError::Chain((*error).to_string()))
                        }
                    };
                }
                Ok(Vec::new())
            }

            async fn read_openable_match_now(
                &self,
                _: &TokenContract,
            ) -> Result<Option<Match>, ChainError> {
                Ok(None)
            }

            async fn place_buy(&self, _: &TokenContract, _: &dyn Note) -> Result<(), ChainError> {
                unimplemented!()
            }

            async fn read_match(
                &self,
                token_contract: &TokenContract,
            ) -> Result<Match, ChainError> {
                if let Some(PendingWatcherOutcome::Match(matched)) = &self.watcher_outcome {
                    return Ok(matched.clone());
                }
                Err(ChainError::NoMatch(token_contract.clone()))
            }

            async fn open_stream(
                &self,
                _: &TokenContract,
                _: Vec<u8>,
                _: &dyn Note,
            ) -> Result<(), ChainError> {
                unimplemented!()
            }

            async fn read_handover(
                &self,
                _: &TokenContract,
            ) -> Result<Option<Vec<u8>>, ChainError> {
                Ok(None)
            }

            async fn claim_tokens(
                &self,
                _: &TokenContract,
                _: &dyn Note,
                _: u128,
            ) -> Result<(), ChainError> {
                unimplemented!()
            }

            async fn stop(
                &self,
                _: &TokenContract,
                _: &dyn Note,
            ) -> Result<Settlement, ChainError> {
                unimplemented!()
            }

            async fn snapshot(&self, _: &TokenContract) -> Option<StreamSnapshot> {
                None
            }
        }

        fn quiet_timing() -> SupervisionTiming {
            SupervisionTiming {
                health_interval: Duration::from_secs(3_600),
                health_timeout: Duration::from_secs(1),
                cycle_timeout: Duration::from_secs(2),
                cancel_poll: Duration::from_millis(1),
                expiry_poll: SellerLivenessParams::canonical().offer_expiry_poll,
                reap_timeout: Duration::from_secs(2),
                reap_poll: Duration::from_millis(1),
                abort_gateway_on_stop: false,
                advertise_probe: AdvertiseProbePolicy::default(),
            }
        }

        fn quiet_watch(name: &str) -> (tempfile::TempDir, SellerMatchWatchConfig) {
            let (directory, mut watch) = watch(name);
            watch.poll_interval = Duration::from_secs(3_600);
            (directory, watch)
        }

        async fn run_to_expiry(
            backend: &ScriptedExpiryBackend,
            name: &str,
        ) -> RestingSellerOutcome {
            let owner = address('e');
            let token_contract = address('f');
            let identity = identity(&owner, &token_contract, 11);
            let seller = super::super::super::start_gateway_with_note(
                "127.0.0.1:0".parse().unwrap(),
                UpstreamConfig::Mock,
                Arc::new(LocalNote::generate()),
            )
            .await
            .unwrap();
            let (_watch_directory, watch) = quiet_watch(name);
            let outcome = supervise_with_timing(
                &seller,
                backend,
                &cfg_for_seller(&token_contract, &seller),
                &watch,
                &identity,
                std::future::pending(),
                quiet_timing(),
            )
            .await
            .unwrap();
            seller.server_task.abort();
            outcome
        }

        async fn release_watcher_during_pending_expiry_read(
            name: &'static str,
            watcher_outcome: PendingWatcherOutcome,
        ) -> (RestingSellerOutcome, Duration) {
            let owner = address('e');
            let token_contract = address('f');
            let identity = identity(&owner, &token_contract, 11);
            let timing = quiet_timing();
            assert!(timing.expiry_poll < TRANSIENT_READ_TOTAL_BUDGET);
            let expiry_read_started = Arc::new(tokio::sync::Notify::new());
            let release_watcher = Arc::new(tokio::sync::Notify::new());
            let backend = ScriptedExpiryBackend::new(
                &owner,
                &token_contract,
                identity.order_id,
                [ExpiryReadStep::Pending(Arc::clone(&expiry_read_started))],
            )
            .with_watcher(Arc::clone(&release_watcher), watcher_outcome);
            let supervisor = tokio::spawn(async move {
                let seller = super::super::super::start_gateway_with_note(
                    "127.0.0.1:0".parse().unwrap(),
                    UpstreamConfig::Mock,
                    Arc::new(LocalNote::generate()),
                )
                .await
                .unwrap();
                let (_watch_directory, watch) = quiet_watch(name);
                let outcome = supervise_with_timing(
                    &seller,
                    &backend,
                    &cfg_for_seller(&token_contract, &seller),
                    &watch,
                    &identity,
                    std::future::pending(),
                    timing,
                )
                .await;
                seller.server_task.abort();
                outcome
            });

            tokio::time::timeout(
                timing.expiry_poll + timing.expiry_poll,
                expiry_read_started.notified(),
            )
            .await
            .expect("the authoritative expiry read must enter its pending state");
            let released_at = tokio::time::Instant::now();
            release_watcher.notify_one();
            let outcome = tokio::time::timeout(timing.expiry_poll, supervisor)
                .await
                .expect("the pinned watcher must win promptly, before the expiry read bound")
                .expect("supervision task must not panic")
                .expect("supervision must return an outcome");
            (outcome, released_at.elapsed())
        }

        #[tokio::test(start_paused = true)]
        async fn pending_expiry_read_preserves_pinned_match_watcher_outcomes() {
            let expected = sample_match(&address('f'));
            let (matched_outcome, matched_elapsed) = release_watcher_during_pending_expiry_read(
                "issue-1168-pending-read-match",
                PendingWatcherOutcome::Match(expected.clone()),
            )
            .await;
            let matched = match matched_outcome {
                RestingSellerOutcome::Matched(matched) => matched,
                other => panic!("pending expiry read swallowed the match: {other:?}"),
            };
            assert_eq!(matched.token_contract, expected.token_contract);
            assert_eq!(matched.buyer_pubkey, expected.buyer_pubkey);
            assert_eq!(matched.price_per_tick, expected.price_per_tick);
            assert!(
                matched_elapsed < quiet_timing().expiry_poll,
                "match waited {matched_elapsed:?} instead of winning promptly"
            );

            let watcher_error = "scripted pinned match watcher failure";
            let (error_outcome, error_elapsed) = release_watcher_during_pending_expiry_read(
                "issue-1168-pending-read-watcher-error",
                PendingWatcherOutcome::Error(watcher_error),
            )
            .await;
            assert!(
                matches!(
                    error_outcome,
                    RestingSellerOutcome::Stopped {
                        reason: RestingStopReason::Watcher(ref error),
                        ..
                    } if error.contains(watcher_error)
                ),
                "pending expiry read swallowed the pinned watcher error: {error_outcome:?}"
            );
            assert!(
                error_elapsed < quiet_timing().expiry_poll,
                "watcher error waited {error_elapsed:?} instead of surfacing promptly"
            );
        }

        #[tokio::test(start_paused = true)]
        async fn slow_authoritative_deadline_read_outlives_poll_cadence() {
            let owner = address('e');
            let token_contract = address('f');
            let poll_cadence = SellerLivenessParams::canonical().offer_expiry_poll;
            let read_delay = poll_cadence + poll_cadence;
            assert!(read_delay > poll_cadence);
            assert!(read_delay < TRANSIENT_READ_TOTAL_BUDGET);
            let backend = ScriptedExpiryBackend::new(
                &owner,
                &token_contract,
                11,
                [ExpiryReadStep::SlowExpired(read_delay)],
            );

            let outcome = tokio::time::timeout(
                TRANSIENT_READ_ATTEMPT_TIMEOUT,
                run_to_expiry(&backend, "issue-1168-slow-read"),
            )
            .await
            .expect(
                "a read inside the transient-read budget must not be cancelled by poll cadence",
            );

            assert!(matches!(
                outcome,
                RestingSellerOutcome::Stopped {
                    reason: RestingStopReason::Expired(_),
                    disposition: CancellationDisposition::NotAttemptedExpired,
                }
            ));
            assert_eq!(backend.attempts(), 1, "the slow read must complete once");
        }

        #[derive(Clone)]
        struct SharedWriter(Arc<Mutex<Vec<u8>>>);

        impl Write for SharedWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        fn capture_expiry_read_logs(name: &str, steps: Vec<ExpiryReadStep>) -> String {
            let output = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&output);
            let subscriber = tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_target(false)
                .with_max_level(tracing::Level::TRACE)
                .with_writer(move || SharedWriter(Arc::clone(&captured)))
                .finish();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap();
            let backend = ScriptedExpiryBackend::new(&address('e'), &address('f'), 11, steps);

            tracing::subscriber::with_default(subscriber, || {
                let outcome = runtime.block_on(run_to_expiry(&backend, name));
                assert!(matches!(
                    outcome,
                    RestingSellerOutcome::Stopped {
                        reason: RestingStopReason::Expired(_),
                        disposition: CancellationDisposition::NotAttemptedExpired,
                    }
                ));
            });

            let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
            output
        }

        #[test]
        fn deadline_read_reporting_escalates_only_for_consecutive_failures() {
            let isolated = capture_expiry_read_logs(
                "issue-1168-isolated-failures",
                vec![
                    ExpiryReadStep::Fail,
                    ExpiryReadStep::Live,
                    ExpiryReadStep::Fail,
                    ExpiryReadStep::Expired,
                ],
            );
            assert_eq!(
                isolated.matches("seller_offer_expiry_read_failed").count(),
                2
            );
            assert!(!isolated.contains("seller_offer_expiry_read_blind"));
            assert!(isolated.contains("consecutive_failures=1"));
            assert!(isolated.contains("attempt_total=1"));
            assert!(isolated.contains("attempt_total=3"));

            let consecutive = capture_expiry_read_logs(
                "issue-1168-consecutive-failures",
                vec![
                    ExpiryReadStep::Fail,
                    ExpiryReadStep::Fail,
                    ExpiryReadStep::Expired,
                ],
            );
            let escalation = consecutive
                .lines()
                .find(|line| line.contains("seller_offer_expiry_read_blind"))
                .expect("a consecutive failure run must emit the distinct blindness report");
            assert!(escalation.contains("ERROR"), "{escalation}");
            assert!(
                escalation.contains("consecutive_failures=2"),
                "{escalation}"
            );
            assert!(escalation.contains("attempt_total=2"), "{escalation}");
            assert!(
                escalation.contains("elapsed_since_last_successful_read_ms="),
                "{escalation}"
            );
            assert!(
                escalation.contains("current expiry is unverified"),
                "{escalation}"
            );
            assert!(!consecutive.contains("the offer stays supervised"));
        }
    }
}
