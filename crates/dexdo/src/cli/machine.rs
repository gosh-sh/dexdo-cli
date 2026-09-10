use anyhow::Result;
use rand::RngCore;
use serde::Serialize;
use serde_json::{json, Map, Value};

// v2: every SHELL figure in these objects is stated in SHELL, not in raw ECC[2] units. A consumer
// that kept reading `best_ask` as raw would be off by a factor of a billion, and a version is the
// only thing that says so before it is read.
pub(crate) const MARKETS_SCHEMA: &str = "dexdo.markets.v2";
// v1: `markets address` carries no SHELL figure at all -- it names an address, a model hash and the
// registry spelling the name resolved to, so the SHELL-unit revisions above do not apply to it.
pub(crate) const MARKETS_ADDRESS_SCHEMA: &str = "dexdo.markets_address.v1";
// v2: prices and costs in SHELL. See the note on MARKETS_SCHEMA.
pub(crate) const QUOTE_SCHEMA: &str = "dexdo.quote.v2";
pub(crate) const BUYER_EVENT_SCHEMA: &str = dexdo::runtime_events::BUYER_EVENT_SCHEMA;
// v3: the accounting figures are SHELL. See the note on MARKETS_SCHEMA.
pub(crate) const STATUS_SCHEMA: &str = "dexdo.status.v3";
pub(crate) const CLOSE_SCHEMA: &str = "dexdo.close.v1";
pub(crate) const NOTE_DEPLOY_SCHEMA: &str = "dexdo.note_deploy.v1";
/// One schema for `subscription place`, `subscription status` and `subscription cancel`.

/// Three commands, ONE object shape and ONE version, because they are three moves in a single
/// pre-match lifecycle and an orchestrator branches on `operation`, not on a shape. A field that
/// does not exist for this command at this moment is `null` -- never omitted, never a placeholder --
/// so "no fill yet" and "no refund observed" are readable facts instead of missing keys.
// v2: terms, escrow and refunds in SHELL. See the note on MARKETS_SCHEMA.
pub(crate) const SUBSCRIPTION_SCHEMA: &str = "dexdo.subscription.v2";
/// The three readers a runtime asks first: what do I have, what is on it, what of mine is open.

/// Declared HERE with the others rather than inline at the print site, so the schema gates below
/// see them: one asserts the constants against the normative document, the other stops the
/// document drifting from the code. A name that exists only as a literal in the command that
/// prints it is outside both.
pub(crate) const DEALS_SCHEMA: &str = "dexdo.deals.v1";
pub(crate) const NOTE_LIST_SCHEMA: &str = "dexdo.note_list.v1";
pub(crate) const NOTE_BALANCE_SCHEMA: &str = "dexdo.note_balance.v1";
/// The on-chain model registry, read out whole.

/// Kept beside the other schema constants so the pin and document-drift gates cover it.
pub(crate) const MODEL_REGISTRY_SCHEMA: &str = "dexdo.model_registry.v1";
pub(crate) const DOCTOR_SCHEMA: &str = "dexdo.doctor.v1";
pub(crate) const ERROR_SCHEMA: &str = "dexdo.error.v1";

#[derive(Serialize)]
pub(crate) struct DoctorVersion {
    pub(crate) contract: String,
    pub(crate) version: String,
}

#[derive(Serialize)]
pub(crate) struct DoctorCheck {
    pub(crate) name: String,
    pub(crate) verdict: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) skip_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expected: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) actual: Option<String>,
    pub(crate) message: String,
}

#[derive(Serialize)]
pub(crate) struct DoctorPolicy {
    pub(crate) status: &'static str,
    pub(crate) problems: Vec<String>,
}

#[derive(Serialize)]
pub(crate) struct DoctorSummary {
    pub(crate) checked: usize,
    pub(crate) passed: usize,
    pub(crate) failed: usize,
    pub(crate) skipped: usize,
}

#[derive(Serialize)]
pub(crate) struct DoctorResponse {
    pub(crate) schema: &'static str,
    pub(crate) network: String,
    pub(crate) endpoint: String,
    pub(crate) manifest_generation: Option<String>,
    pub(crate) chain_generation: Option<String>,
    pub(crate) versions: Vec<DoctorVersion>,
    pub(crate) checks: Vec<DoctorCheck>,
    /// Positive means the local clock is ahead of the chain; negative means it is behind.
    pub(crate) clock_skew_seconds: i64,
    pub(crate) policy: DoctorPolicy,
    pub(crate) summary: DoctorSummary,
    pub(crate) verdict: &'static str,
}

/// The structured form of the Hot-funding outcome carried inside a command's ONE success object.

/// It deliberately carries only the stable lifecycle fact. Provider responses and local paths can
/// contain sensitive material, while an orchestrator only needs to know which funding state the
/// command observed. Keeping this type in `machine.rs` makes it part of the same explicit machine
/// contract as the neighbouring response/event types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum MachineFundingNotice {
    AlreadyFunded,
    RequestSubmitted,
    RequestAlreadyPending,
    RequestExecuted,
    RequestIndeterminate,
    ManualTopUpRequested,
}

/// The funding state a money command had already reached when it failed.

/// A `note deploy` that creates a Vault -> Hot request and then times out waiting for the balance
/// leaves an orchestrator with the one question it cannot answer from an exit code: has money
/// already left the Vault? The success object answers it with `funding_notice`; before this, the
/// failure answered it only in `stderr` prose.

/// Carried as a typed cause rather than as wording, so the envelope is built by matching a type -
/// the same way `classify_error` reads every other machine-relevant fact - and the operator-facing
/// message above it is free to change without changing the machine contract.

/// It holds ONLY the stable event. No address, no provider response, no local path, no key.
#[derive(Debug)]
pub(crate) struct FundingContext {
    notice: MachineFundingNotice,
    source: anyhow::Error,
}

impl FundingContext {
    /// Carry `notice` alongside `source` without changing what `source` says or loses.
    pub(crate) fn wrap(notice: MachineFundingNotice, source: anyhow::Error) -> anyhow::Error {
        anyhow::Error::new(Self { notice, source })
    }

    pub(crate) fn notice(&self) -> MachineFundingNotice {
        self.notice
    }

    /// The failure the state was attached to, for a renderer that would otherwise repeat it.
    pub(crate) fn source_error(&self) -> &anyhow::Error {
        &self.source
    }
}

impl std::fmt::Display for FundingContext {
    /// The source's FIRST line, and deliberately not `{source:#}`.

    /// Written through a plain `{}` rather than by forwarding `f`, so this holds however the
    /// wrapper itself is formatted: forwarding would let a `{context:#}` render the source's whole
    /// flattened chain here, and that chain is already rendered below this node.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.source)
    }
}

impl std::error::Error for FundingContext {
    /// The wrapped failure, whole.

    /// Tried skipping one level here to stop a renderer printing this node's text twice; that
    /// broke the classifier, which walks this chain to downcast the typed cause it codes by, and a
    /// transport fault started coding as `Internal`. The duplicate is a rendering problem and is
    /// solved where rendering happens; the chain stays exactly as it was.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// one fact, said once.
#[cfg(test)]
mod funding_context_1432_tests {
    use super::*;

    #[derive(Debug)]
    struct Deeper;

    impl std::fmt::Display for Deeper {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("the transport gave up")
        }
    }

    impl std::error::Error for Deeper {}

    /// The chain this wrapper hands on must stay walkable and downcastable: the classifier reads it
    /// to find the typed cause it codes by, and shortening it turned a transport fault into
    /// `Internal`. The duplicate text this wrapper produces is dealt with at render time.
    #[test]
    fn the_typed_cause_stays_reachable_through_the_chain() {
        let inner = anyhow::Error::new(Deeper).context("Hot is still short 100 SHELL");
        let wrapped = FundingContext::wrap(MachineFundingNotice::RequestSubmitted, inner);

        assert!(
            wrapped
                .chain()
                .any(|cause| cause.downcast_ref::<Deeper>().is_some()),
            "the typed cause fell out of the chain"
        );
        assert!(format!("{wrapped:#}").contains("the transport gave up"));
    }

    /// The machine path reads its own accessor and must not notice any of this.
    #[test]
    fn the_machine_path_still_sees_the_whole_source() {
        let inner = anyhow::Error::new(Deeper).context("Hot is still short 100 SHELL");
        let wrapped = FundingContext::wrap(MachineFundingNotice::RequestSubmitted, inner);
        let context = wrapped
            .downcast_ref::<FundingContext>()
            .expect("the wrapper is what was built");

        let source = format!("{:#}", context.source_error());
        assert!(source.contains("Hot is still short 100 SHELL"), "{source}");
        assert!(source.contains("the transport gave up"), "{source}");
        assert_eq!(context.notice(), MachineFundingNotice::RequestSubmitted);
    }
}

pub(crate) const OP_MARKETS: &str = "markets";
pub(crate) const OP_QUOTE: &str = "quote";
pub(crate) const OP_BUYER_START: &str = "buyer_start";
pub(crate) const OP_BUYER_RUNTIME: &str = "buyer_runtime";
pub(crate) const OP_BUYER_SHUTDOWN: &str = "buyer_shutdown";
pub(crate) const OP_STATUS: &str = "status";
pub(crate) const OP_CLOSE: &str = "close";
pub(crate) const OP_NOTE_DEPLOY: &str = "note_deploy";
pub(crate) const OP_SETTLEMENT_RECEIPT: &str = "settlement_receipt";
pub(crate) const OP_DEALS: &str = "deals";
pub(crate) const OP_NOTE_LIST: &str = "note_list";
pub(crate) const OP_NOTE_BALANCE: &str = "note_balance";
pub(crate) const OP_MODEL_REGISTRY: &str = "model_registry";
pub(crate) const OP_DOCTOR: &str = "doctor";
pub(crate) const OP_SUBSCRIPTION_PLACE: &str = "subscription_place";
pub(crate) const OP_SUBSCRIPTION_STATUS: &str = "subscription_status";
pub(crate) const OP_SUBSCRIPTION_CANCEL: &str = "subscription_cancel";
pub(crate) const NOTE_DEPLOY_GENERATION_MISMATCH_MARKER: &str = "NETWORK_GENERATION_MISMATCH";
pub(crate) const NOTE_DEPLOY_GENERATION_MISMATCH_MESSAGE: &str =
    "NETWORK_GENERATION_MISMATCH: upgrade dexdo or point DEXDO_MANIFEST at a matching manifest, then retry; \
     no wallet transaction was signed or submitted, no voucher was generated, and no funds were spent";

/// the stable machine-readable name for "this instance has no funding wallet bound".
pub(crate) const WALLET_NOT_CONFIGURED_CODE: &str = "wallet_not_configured";

/// The remediation, carried on `message` rather than only inside `cause`.

/// `message` is the field an orchestrator surfaces to a human, so it carries one directly
/// executable setup command. The general onboarding command still lets the operator choose another
/// provider; the remediation itself contains no placeholder or prose-only provider list.
pub(crate) const WALLET_NOT_CONFIGURED_MESSAGE: &str =
    "wallet is not configured; run `dexdo wallet onboard gosh-ai` first";

#[derive(Debug)]
pub(crate) struct MachineErrorPrinted;

impl std::fmt::Display for MachineErrorPrinted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("machine error already emitted")
    }
}

impl std::error::Error for MachineErrorPrinted {}

pub(crate) fn printed_error() -> anyhow::Error {
    anyhow::anyhow!(MachineErrorPrinted)
}

pub(crate) fn is_printed_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<MachineErrorPrinted>().is_some()
}

#[derive(Debug)]
pub(crate) struct SubscriptionStatusOrderNotFound {
    message: String,
}

impl SubscriptionStatusOrderNotFound {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for SubscriptionStatusOrderNotFound {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SubscriptionStatusOrderNotFound {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorCode {
    InvalidArgument,
    StaleClient,
    DealRecordSchemaTooNew,
    NoLiquidity,
    IncompleteQuote,
    InsufficientBalance,
    HandoverTimeout,
    HandoverDecryptFailed,
    EndpointBindFailed,
    EndpointReadinessFailed,
    GatewayConnectFailed,
    GatewayAuthFailed,
    ChainTransport,
    /// the account could not be READ, and reading it again will not change that.

    /// Non-retryable by construction, not by a separate setting: `retryable()` is a closed list of
    /// the codes worth retrying and this one is deliberately absent from it. A code whose whole
    /// meaning is "a retry will not change this" cannot be retryable without contradicting its own
    /// name.

    /// What it does NOT cover, said here so it does not quietly become a second `Internal`:

    /// - a read that failed TRANSIENTLY -- that is `ChainTransport`, and that one IS retryable;
    /// - a read that succeeded and found no such account -- answering "absent" is not failing to read;
    /// - anything about what the account HOLDS. This is the whole point of: a wallet that
    /// could not be read used to report as `InsufficientBalance`, sending an operator to top up an
    /// account that was never short. This code states nothing about funds;
    /// - a submit, a contract result or a revert. Nothing was sent.
    AccountUnreadable,
    ChainRevert,
    AmbiguousSubmit,
    SettlementFailed,
    NotRecoverableYet,
    DisputedDeal,
    /// The named order is not a resting order this note owns. Covers absent, wrong owner
    /// and wrong shape together, because an orchestrator's next move is the same for all three and
    /// telling them apart would leak whose order it is.
    OrderNotFound,
    /// A cancel lost the race: the order filled before the book removed it. Distinct from
    /// `AMBIGUOUS_SUBMIT` -- the outcome is known, and it is not a refund.
    OrderAlreadyMatched,
    /// Durable and on-chain facts disagree, e.g. one order id is simultaneously resting and
    /// filled. Distinct from `AMBIGUOUS_SUBMIT`: nothing is in flight, the two records conflict.
    ContradictoryState,
    /// This note already has a buy resting in the book, so nothing was sent. Its own code
    /// because it is the one outcome an orchestrator must NOT retry and must not read as a fault
    /// either: the earlier order is working exactly as it was asked to, and the next move is to look
    /// at it (`dexdo orders journal`) rather than to place a second one. Without this it fell through
    /// the text rules to `INTERNAL` -- "escalate a client bug" for a healthy order -- or, worse, the
    /// human path's early exit reported success with nothing placed.
    BuyAlreadyResting,
    /// the command needs a funding (Hot) wallet and this instance has none bound. It is the
    /// operator's own configuration state, not a client fault, and the fix is a setup command -- so
    /// it is its own code rather than `INTERNAL`, which tells an orchestrator to escalate a bug.
    WalletNotConfigured,
    Internal,
}

impl ErrorCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::StaleClient => "STALE_CLIENT",
            Self::DealRecordSchemaTooNew => "DEAL_RECORD_SCHEMA_TOO_NEW",
            Self::NoLiquidity => "NO_LIQUIDITY",
            Self::IncompleteQuote => "INCOMPLETE_QUOTE",
            Self::InsufficientBalance => "INSUFFICIENT_BALANCE",
            Self::HandoverTimeout => "HANDOVER_TIMEOUT",
            Self::HandoverDecryptFailed => "HANDOVER_DECRYPT_FAILED",
            Self::EndpointBindFailed => "ENDPOINT_BIND_FAILED",
            Self::EndpointReadinessFailed => "ENDPOINT_READINESS_FAILED",
            Self::GatewayConnectFailed => "GATEWAY_CONNECT_FAILED",
            Self::GatewayAuthFailed => "GATEWAY_AUTH_FAILED",
            Self::ChainTransport => "CHAIN_TRANSPORT",
            Self::AccountUnreadable => "ACCOUNT_UNREADABLE",
            Self::ChainRevert => "CHAIN_REVERT",
            Self::AmbiguousSubmit => "AMBIGUOUS_SUBMIT",
            Self::SettlementFailed => "SETTLEMENT_FAILED",
            Self::NotRecoverableYet => "NOT_RECOVERABLE_YET",
            Self::DisputedDeal => "DISPUTED_DEAL",
            Self::OrderNotFound => "ORDER_NOT_FOUND",
            Self::OrderAlreadyMatched => "ORDER_ALREADY_MATCHED",
            Self::ContradictoryState => "CONTRADICTORY_STATE",
            Self::BuyAlreadyResting => "BUY_ALREADY_RESTING",
            Self::WalletNotConfigured => WALLET_NOT_CONFIGURED_CODE,
            Self::Internal => "INTERNAL",
        }
    }

    pub(crate) fn retryable(self) -> bool {
        matches!(
            self,
            Self::NoLiquidity
                | Self::IncompleteQuote
                | Self::HandoverTimeout
                | Self::EndpointBindFailed
                | Self::EndpointReadinessFailed
                | Self::GatewayConnectFailed
                | Self::ChainTransport
                | Self::SettlementFailed
                | Self::NotRecoverableYet
        )
    }

    pub(crate) fn safe_message(self) -> &'static str {
        match self {
            Self::InvalidArgument => "invalid or missing command input",
            Self::StaleClient => NOTE_DEPLOY_GENERATION_MISMATCH_MESSAGE,
            Self::DealRecordSchemaTooNew => "durable deal record schema is newer than this runtime",
            Self::NoLiquidity => "no executable liquidity is available",
            Self::IncompleteQuote => "liquidity is insufficient for the requested quote",
            Self::InsufficientBalance => "balance is insufficient for the selected action",
            Self::HandoverTimeout => "seller did not write the handover before the deadline",
            Self::HandoverDecryptFailed => "handover is malformed or not decryptable by this note",
            Self::EndpointBindFailed => "local endpoint bind failed",
            Self::EndpointReadinessFailed => "local endpoint readiness check failed",
            Self::GatewayConnectFailed => "seller gateway connection failed",
            Self::GatewayAuthFailed => "seller gateway authentication failed",
            Self::ChainTransport => "chain transport failed before a by-fact result",
            Self::AccountUnreadable => {
                "the account could not be read, and retrying will not change that"
            }
            Self::ChainRevert => "chain returned a non-success contract result",
            Self::AmbiguousSubmit => "money submit outcome is unknown and must not be retried",
            Self::SettlementFailed => "settlement submission failed",
            Self::NotRecoverableYet => "deal is not recoverable yet",
            Self::DisputedDeal => "deal is disputed and needs dispute resolution",
            Self::OrderNotFound => "order is not resting under this owner in this book",
            Self::OrderAlreadyMatched => "order matched before it could be cancelled",
            Self::ContradictoryState => "durable and on-chain records contradict each other",
            Self::BuyAlreadyResting => "a buy from this note is already resting in the book",
            Self::WalletNotConfigured => WALLET_NOT_CONFIGURED_MESSAGE,
            Self::Internal => "internal invariant failed",
        }
    }
}

#[cfg(test)]
#[path = "machine_classifier_conformance_1795.rs"]
mod machine_classifier_conformance_1795;

pub(crate) fn classify_error(operation: &str, err: &anyhow::Error) -> ErrorCode {
    for cause in err.chain() {
        if cause
            .downcast_ref::<super::deals::DealHandleSchemaTooNew>()
            .is_some()
        {
            return ErrorCode::DealRecordSchemaTooNew;
        }
        // and it is typed on purpose. The defect being fixed here was a wallet READ failure
        // that matched the substring "balance" and reported as INSUFFICIENT_BALANCE, sending the
        // operator to top up an account that was never short. Answering a text-matching defect with
        // another text match would move the disease onto a new sentinel word rather than cure it,
        // so the carrier is a type and the message is free to say whatever reads best.
        if cause
            .downcast_ref::<super::note_cmd::FundingWalletUnreadable>()
            .is_some()
        {
            return ErrorCode::AccountUnreadable;
        }
        // The refusal that says the deal is NOT disputed. `:1067` reserves DISPUTED_DEAL for
        // "Deal is disputed and cannot be closed", so the old verdict told the consumer the reverse
        // of the fact -- the word matched inside its own negation, after the message was lower-cased.
        // `:1049` INVALID_ARGUMENT: the request named a deal with no dispute to resolve.
        if cause
            .downcast_ref::<super::recover::DealIsNotDisputed>()
            .is_some()
        {
            return ErrorCode::InvalidArgument;
        }
        if let Some(gateway) = cause.downcast_ref::<dexdo_core::DexdoError>() {
            if gateway.code() == dexdo_core::error_codes::E_GATEWAY_UNREACHABLE.code() {
                return ErrorCode::GatewayConnectFailed;
            }
            if gateway.code() == dexdo_core::error_codes::E_GATEWAY_WRONG_ENDPOINT.code() {
                return ErrorCode::GatewayAuthFailed;
            }
            // the wallet fail-fast is a TYPED error carrying a stable code, so it is read
            // from the code and never from its wording. Without this it fell past every rule below
            // to `INTERNAL` -- which tells an orchestrator "this client has a bug" when the true
            // instruction is "bind a wallet", and which no script can branch on. Matching by type
            // here cannot weaken any other mapping: it fires only on this one code, and every
            // classification below still runs for everything else.
            if gateway.code() == dexdo_core::error_codes::E_WALLET_NOT_CONFIGURED.code() {
                return ErrorCode::WalletNotConfigured;
            }
        }
        if let Some(chain) = cause.downcast_ref::<dexdo_core::ChainError>() {
            match chain {
                dexdo_core::ChainError::Transport(_) => return ErrorCode::ChainTransport,
                dexdo_core::ChainError::Contract(_) | dexdo_core::ChainError::DuplicateSell(_) => {
                    return ErrorCode::ChainRevert;
                }
                dexdo_core::ChainError::AmbiguousSubmit(_) => return ErrorCode::AmbiguousSubmit,
                _ => {}
            }
        }
        if operation == OP_SUBSCRIPTION_STATUS
            && cause
                .downcast_ref::<SubscriptionStatusOrderNotFound>()
                .is_some()
        {
            return ErrorCode::OrderNotFound;
        }
        if cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest_error_is_transport)
        {
            return ErrorCode::ChainTransport;
        }
    }

    // Below this boundary, rules are best-effort classification of arbitrary failure text,
    // including third-party messages. Rewording a message here may change its emitted code.
    let msg = format!("{err:#}").to_ascii_lowercase();
    if operation == OP_NOTE_DEPLOY && msg.contains("network_generation_mismatch") {
        return ErrorCode::StaleClient;
    }
    if msg.contains("no liquidity") {
        return ErrorCode::NoLiquidity;
    }
    if msg.contains("no executable matching ask")
        || msg.contains("no matchable ask")
        || msg.contains("refusing multi-ask fill")
        || msg.contains("placeinferencebuy cannot target")
        || msg.contains("raw order-book matcher")
        || msg.contains("refusing to send escrow into the wrong deal")
        || (msg.contains("best ask price") && msg.contains("above buyer max_price_per_tick"))
    {
        return ErrorCode::NoLiquidity;
    }
    if msg.contains("incomplete quote") {
        return ErrorCode::IncompleteQuote;
    }
    if msg.contains("selected tokencontract") || msg.contains("refusing to move escrow") {
        return ErrorCode::ChainRevert;
    }
    if msg.contains("buyer place aborted: this note has withdrawn") {
        return ErrorCode::ChainRevert;
    }
    if msg.contains("buyer model-only preflight failed")
        || msg.contains("buyer target preflight failed")
    {
        return ErrorCode::ChainRevert;
    }
    if msg.contains("insufficient") || msg.contains("balance") || msg.contains("deposit") {
        return ErrorCode::InsufficientBalance;
    }
    // and this one is an ORDER defect rather than a wording one. The message the client
    // actually emits is "malformed handover: invalid bytes"; with the `invalid` rule first, the rule
    // written for exactly this condition (`runtime-machine-contract.md:1057`, "Handover is present
    // but malformed or not decryptable by this note") could never fire, and the operator was told
    // their command INPUT was invalid. Moved above the generic argument rule.
    if msg.contains("malformed handover") || msg.contains("handover decrypt failed") {
        return ErrorCode::HandoverDecryptFailed;
    }
    if msg.contains("requires exactly one")
        || msg.contains("required")
        || msg.contains("mutually exclusive")
        || msg.contains("pass --")
        || msg.contains("provide --")
        || msg.contains("invalid")
        || msg.contains("parse")
    {
        return ErrorCode::InvalidArgument;
    }
    if msg.contains("did not open the stream") || msg.contains("handover within") {
        return ErrorCode::HandoverTimeout;
    }
    if operation == OP_BUYER_START && msg.contains("bind") {
        return ErrorCode::EndpointBindFailed;
    }
    if msg.contains("readiness") || msg.contains("/v1/models") {
        return ErrorCode::EndpointReadinessFailed;
    }
    if msg.contains("challenge") || msg.contains("auth") {
        return ErrorCode::GatewayAuthFailed;
    }
    if msg.contains("gateway") || msg.contains("upstream") {
        return ErrorCode::GatewayConnectFailed;
    }
    if msg.contains("not recoverable") || msg.contains("after match_open_timeout") {
        return ErrorCode::NotRecoverableYet;
    }
    if msg.contains("disputed") {
        return ErrorCode::DisputedDeal;
    }
    if msg.contains("settlement") || msg.contains("streamstop") || msg.contains("cleanup") {
        return ErrorCode::SettlementFailed;
    }
    // an error that names a contract exit code IS a chain revert, even when the typed
    // `ChainError` was flattened into a string on its way out (the on-demand deal initializer does
    // that). Last resort, so every more specific rule above still wins.
    if msg.contains("exit_code=") {
        return ErrorCode::ChainRevert;
    }
    ErrorCode::Internal
}

pub(crate) fn reqwest_error_is_transport(error: &reqwest::Error) -> bool {
    error.is_connect()
        || error.is_timeout()
        || error.is_body()
        || error
            .status()
            .is_some_and(|status| status.is_server_error() || status.as_u16() == 429)
}

#[derive(Serialize)]
pub(crate) struct MachineError {
    pub(crate) schema: &'static str,
    pub(crate) operation: &'static str,
    pub(crate) code: &'static str,
    pub(crate) message: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cause: Option<String>,
    pub(crate) retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) network: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) frame_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) order_book: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) token_contract: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) deal_handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) retryable_after_unix: Option<u64>,
    /// re-audit item 8: the funding state the command had already reached when it failed.

    /// Present only when this run created a Vault -> Hot request or found one of its own still
    /// pending - the states in which a failure cannot tell an orchestrator whether money has left
    /// the Vault. Absent means no funding request of this run exists, which is itself the answer.

    /// It carries the same stable event the success object carries, and nothing else.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) funding_notice: Option<MachineFundingNotice>,
}

impl MachineError {
    pub(crate) fn new(operation: &'static str, code: ErrorCode) -> Self {
        Self {
            schema: ERROR_SCHEMA,
            operation,
            code: code.as_str(),
            message: code.safe_message(),
            cause: None,
            retryable: code.retryable(),
            network: None,
            frame_model: None,
            order_book: None,
            token_contract: None,
            deal_handle: None,
            failure_class: None,
            retryable_after_unix: None,
            funding_notice: None,
        }
    }

    pub(crate) fn with_cause(mut self, cause: impl Into<String>) -> Self {
        self.cause = Some(cause.into());
        self
    }

    /// Attach the public identity the failing command already had in hand.

    /// A cancel that loses its race must hand the orchestrator the deal it lost to -- the contract
    /// already reserves `token_contract`/`deal_handle` on the error object for exactly that, and a
    /// consumer driving the lifecycle from JSON alone has no other way to reach it.
    pub(crate) fn with_market(
        mut self,
        network: &str,
        frame_model: &str,
        order_book: &str,
    ) -> Self {
        self.network = Some(network.to_string());
        self.frame_model = Some(frame_model.to_string());
        self.order_book = Some(order_book.to_string());
        self
    }

    pub(crate) fn with_deal(mut self, token_contract: &str, deal_handle: &str) -> Self {
        self.token_contract = Some(token_contract.to_string());
        self.deal_handle = Some(deal_handle.to_string());
        self
    }
}

pub(crate) fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

pub(crate) fn print_short_error(operation: &'static str, code: ErrorCode) -> Result<()> {
    print_json(&MachineError::new(operation, code))
}

pub(crate) fn print_error(
    operation: &'static str,
    code: ErrorCode,
    err: &anyhow::Error,
) -> Result<()> {
    print_json(&machine_error(operation, code, err))
}

/// The exact envelope [`print_error`] writes, built rather than printed.

/// Separated so a regression can assert the FIELDS a machine consumer reads instead of scraping the
/// process's stdout, and so the one place that decides what a failure carries stays one place.
pub(crate) fn machine_error(
    operation: &'static str,
    code: ErrorCode,
    err: &anyhow::Error,
) -> MachineError {
    #[allow(unused_mut)]
    let mut error = MachineError::new(operation, code).with_cause(error_cause(err));
    // re-audit item 8: a failed money command carries the funding state it had already
    // reached. Read from the typed cause the funding wait attaches, so the fact survives any
    // rewording of the message it travels under.

    // `Error::downcast_ref` and NOT a `chain()` scan: the state is attached with `.context(..)`, so
    // that the original error keeps being an error and its own typed causes stay downcastable for
    // `classify_error`. A `chain()` element for a context layer is anyhow's `ContextError<C, E>`,
    // never the `C` inside it, so a chain scan cannot see it - measured, not assumed.
    // `Error::downcast_ref` walks the context chain itself and does find it.
    {
        error.funding_notice = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<FundingContext>())
            .map(FundingContext::notice);
        // The state wrapper renders as its own source's first line, so rendering the whole error
        // would print that line twice. When it is the outermost node, render from underneath it
        // instead: every layer below is still rendered, so nothing is dropped but the repeat.
        // Deliberately only when it IS outermost - skipping it from deeper down would drop
        // whatever wrapped it.
        if let Some(context) = err
            .chain()
            .next()
            .and_then(|cause| cause.downcast_ref::<FundingContext>())
        {
            error.cause = Some(error_cause(context.source_error()));
        }
    }
    error
}

fn error_cause(err: &anyhow::Error) -> String {
    sanitize_error_cause(&format!("{err:#}"))
}

fn sanitize_error_cause(cause: &str) -> String {
    let lower = cause.to_ascii_lowercase();
    const SENSITIVE_MARKERS: &[&str] = &[
        "owner_secret_key_hex",
        "private_key",
        "mnemonic",
        "bearer ",
        "api_key",
        "authorization",
        "prompt",
        "provider response",
        "deal_path",
    ];
    if SENSITIVE_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
    {
        return "sensitive error details redacted".to_string();
    }

    redact_local_paths(cause)
}

fn redact_local_paths(text: &str) -> String {
    fn is_boundary(previous: Option<char>) -> bool {
        previous.is_none_or(|ch| {
            ch.is_whitespace() || matches!(ch, '=' | '(' | '[' | '{' | '"' | '\'' | ',' | ';')
        })
    }

    fn path_end(text: &str, start: usize) -> usize {
        text[start..]
            .char_indices()
            .skip(1)
            .find_map(|(offset, ch)| {
                (ch.is_whitespace() || matches!(ch, '"' | '\'' | ',' | ';' | ')' | ']' | '}'))
                    .then_some(start + offset)
            })
            .unwrap_or(text.len())
    }

    fn starts_local_path(text: &str, start: usize) -> bool {
        let previous = text[..start].chars().next_back();
        if !is_boundary(previous) {
            return false;
        }
        let rest = &text[start..];
        if rest.starts_with('/') || rest.starts_with("\\\\") {
            return true;
        }
        let bytes = rest.as_bytes();
        bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/')
    }

    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while cursor < text.len() {
        if starts_local_path(text, cursor) {
            out.push_str("<redacted-path>");
            cursor = path_end(text, cursor);
            continue;
        }
        let ch = text[cursor..]
            .chars()
            .next()
            .expect("cursor stays on a character boundary");
        out.push(ch);
        cursor += ch.len_utf8();
    }
    out
}

pub(crate) fn now_unix() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow::anyhow!("system clock before epoch: {e}"))?
        .as_secs())
}

pub(crate) fn amount<T: ToString>(value: T) -> String {
    value.to_string()
}

/// The answer to "where does this market live", as one document.

/// `requested_model` and `registry_model` are both carried because they routinely differ: 4.0.36
/// registers a model WITHOUT its producer, so an operator asking for `openai/gpt-5.2-pro` is
/// answered about `gpt-5.2-pro`, and a consumer that only saw the address could not tell that the
/// name it holds is not the name the registry knows.
#[derive(Serialize)]
pub(crate) struct MarketsAddressResponse {
    pub(crate) schema: &'static str,
    pub(crate) network: String,
    pub(crate) requested_model: String,
    pub(crate) registry_model: String,
    pub(crate) model_hash: String,
    pub(crate) order_book: String,
}

#[derive(Serialize)]
pub(crate) struct MarketsResponse {
    pub(crate) schema: &'static str,
    pub(crate) network: String,
    pub(crate) generated_at_unix: u64,
    pub(crate) markets: Vec<MarketEntry>,
}

#[derive(Serialize)]
pub(crate) struct MarketEntry {
    pub(crate) frame_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model_flags: Option<dexdo_core::CanonicalModelFlags>,
    pub(crate) model_hash: String,
    pub(crate) order_book: String,
    pub(crate) root_model: Option<String>,
    pub(crate) active: bool,
    pub(crate) order_count: u128,
    pub(crate) ask_count: u128,
    pub(crate) depth_ticks: String,
    pub(crate) best_ask: Option<String>,
    pub(crate) min_liquidity: String,
    pub(crate) tick_size: String,
    pub(crate) source: String,
}

#[derive(Serialize)]
pub(crate) struct QuoteResponse {
    pub(crate) schema: &'static str,
    pub(crate) network: String,
    pub(crate) generated_at_unix: u64,
    pub(crate) frame_model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model_flags: Option<dexdo_core::CanonicalModelFlags>,
    pub(crate) model_hash: String,
    pub(crate) order_book: String,
    pub(crate) request: QuoteRequest,
    pub(crate) filled_ticks: String,
    pub(crate) total_without_fee: String,
    pub(crate) platform_fee: String,
    pub(crate) total_with_fee: String,
    pub(crate) complete: bool,
    pub(crate) no_liquidity: bool,
    pub(crate) fills: Vec<QuoteFillEntry>,
}

#[derive(Serialize)]
pub(crate) struct QuoteRequest {
    pub(crate) kind: &'static str,
    pub(crate) ticks: Option<String>,
    pub(crate) budget: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct QuoteFillEntry {
    pub(crate) order_id: String,
    pub(crate) token_contract: String,
    pub(crate) ticks: String,
    pub(crate) price_per_tick: String,
    pub(crate) cost_without_fee: String,
    pub(crate) platform_fee: String,
    pub(crate) cost_with_fee: String,
}

#[derive(Serialize)]
pub(crate) struct StatusResponse {
    pub(crate) schema: &'static str,
    pub(crate) network: String,
    pub(crate) generated_at_unix: u64,
    pub(crate) handle: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) token_contract: String,
    pub(crate) frame_model: Option<String>,
    pub(crate) state: String,
    pub(crate) active: bool,
    pub(crate) funded: bool,
    pub(crate) opened: bool,
    pub(crate) disputed: bool,
    pub(crate) probe_accepted: bool,
    pub(crate) accounting: StatusAccounting,
    pub(crate) next: StatusNext,
}

#[derive(Serialize)]
pub(crate) struct StatusAccounting {
    pub(crate) finalized_owed: String,
    pub(crate) buyer_locked: String,
    pub(crate) deposit: String,
    pub(crate) probe_tick: String,
    pub(crate) buyer_bond: String,
    pub(crate) buyer_bond_required: String,
    pub(crate) tokens_final: String,
    pub(crate) tokens_pending: String,
    pub(crate) probe_time_unix: Option<u64>,
    pub(crate) last_claim_time_unix: Option<u64>,
    pub(crate) dispute_time_unix: Option<u64>,
    pub(crate) funded_time_unix: Option<u64>,
}

#[derive(Serialize)]
pub(crate) struct StatusNext {
    pub(crate) action: String,
    pub(crate) retryable_after_unix: Option<u64>,
    pub(crate) command: String,
}

#[derive(Serialize)]
pub(crate) struct CloseResponse {
    pub(crate) schema: &'static str,
    pub(crate) network: String,
    pub(crate) generated_at_unix: u64,
    pub(crate) handle: Option<String>,
    pub(crate) role: String,
    pub(crate) token_contract: String,
    pub(crate) action: String,
    pub(crate) submitted: bool,
    pub(crate) terminal: bool,
    pub(crate) reason: Option<String>,
    pub(crate) state_before: String,
    pub(crate) state_after: String,
    pub(crate) last_observed_promotion: Option<super::deals::LastObservedPromotion>,
    pub(crate) tx: Option<Value>,
}

/// The one machine object all three `dexdo subscription` lifecycle commands emit.

/// `operation` says which command produced it; `action` says what that command did; `submitted`
/// says whether THIS invocation put a message on the chain. Those three together are what stops an
/// orchestrator from sending a second BUY after an ambiguous first one.

/// Every field is always present. `null` is the honest answer for a value that does not exist at
/// this moment -- no fill yet, no refund observed, no live deal to read -- and never a placeholder
/// standing in for one.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SubscriptionResponse {
    pub(crate) schema: &'static str,
    pub(crate) operation: &'static str,
    pub(crate) network: String,
    pub(crate) generated_at_unix: u64,
    /// What this invocation did: `placed`, `reconciled`, `read` or `cancelled`.
    pub(crate) action: &'static str,
    /// Did this invocation submit at least one chain message? `subscription status` sets this when
    /// it booked a due weekly settlement while reading.
    pub(crate) submitted: bool,
    pub(crate) frame_model: String,
    pub(crate) model_hash: String,
    pub(crate) order_book: String,
    pub(crate) note_addr: String,
    pub(crate) order_id: String,
    /// `resting`, `matched`, `cancelled`, `expired`, `terminal` or
    /// `absent_without_authenticated_fill`.

    /// The human line's separate `resting=` boolean is deliberately not mirrored here: it is
    /// `state == "resting"` in every case the CLI can produce, and a second spelling of one fact is
    /// a second thing that can drift.
    pub(crate) state: &'static str,
    pub(crate) terms: SubscriptionTerms,
    /// The fill, once one is provable. `null` while the order rests.
    pub(crate) matched: Option<SubscriptionMatched>,
    /// Did the book confirm the order left it? `null` while the order is still resting, and on
    /// outcomes that never asked -- a `place`, or a matched deal whose order left by filling.
    pub(crate) removal_confirmed: Option<bool>,
    /// The note credit this client OBSERVED. `null` outside `cancel`, and `null` on a backend that
    /// exposes no note balance to read it from.
    pub(crate) refund: Option<SubscriptionRefund>,
    /// Which of the two facts behind `state == "expired"` this client actually observed.
    /// `null` while the order is still in the book, and on outcomes expiry cannot explain.
    pub(crate) expiry: Option<SubscriptionExpiry>,
    /// Live TokenContract facts for a matched subscription. `null` when there is no matched deal to
    /// read, and after a terminal close destroys the account.
    pub(crate) live: Option<SubscriptionLive>,
}

/// The exact order terms this BUY carries, as the book holds them.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SubscriptionTerms {
    pub(crate) max_price_per_tick: String,
    pub(crate) ticks: String,
    pub(crate) deposit: String,
    pub(crate) buyer_bond: String,
    /// Total escrow committed: `deposit` + `buyer_bond`.
    pub(crate) escrow: String,
    /// Raw order-flag bitmask. A subscription BUY is `AON|SUBSCRIPTION`.
    pub(crate) flags: u8,
    pub(crate) deadline_unix: u64,
}

/// The deal one fill produced.

/// It carries no separate seller order id: the fill's own order id is asserted equal to this
/// buyer's order id before the record is ever persisted, so a second id field would be either
/// redundant or the foreign id removed from the preflight line.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SubscriptionMatched {
    pub(crate) token_contract: String,
    pub(crate) deal_handle: String,
    /// Matched volume in ticks.
    pub(crate) ticks: String,
    pub(crate) clearing_price: String,
    /// SHELL returned because the deal cleared below `max_price_per_tick`.
    pub(crate) price_improvement_refund: String,
}

/// The evidence behind an `expired` verdict, and -- when there is not enough of it -- the name of the
/// fact that is missing.

/// A passed `deadline_unix` proves an order is ELIGIBLE for expiry and nothing more. Two further
/// facts decide it, and the book announces them separately on purpose
/// (`InferenceOrderBook.sol:387-393`): that the book removed the row, and that the escrow came
/// back. `state` is `expired` only when both were observed. One of them alone leaves the weaker
/// state standing, with `missing_fact` naming what is still unknown -- because an orchestrator
/// deciding whether an order still holds its escrow must not have to infer that from a clock.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SubscriptionExpiry {
    /// The book itself took the row out -- its own expiry announcement, not the row's absence, which
    /// says nothing about WHICH way an order left the book.
    pub(crate) removal_observed: bool,
    /// The escrow came back: a refund this client read from the book, never one it derived from the
    /// order row's own `escrow` field.
    pub(crate) refund_observed: bool,
    /// The refunded amount, as the payer announced it. `null` until `refund_observed`.
    pub(crate) refunded: Option<String>,
    /// `removal` or `refund` -- which fact is still missing, and therefore why this is not `expired`.
    /// `null` when both were observed.
    pub(crate) missing_fact: Option<&'static str>,
}

/// A refund this client read, never one it computed from the order row.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SubscriptionRefund {
    /// `balance_after - balance_before`, both of which are reported here.
    pub(crate) observed: String,
    pub(crate) balance_before: String,
    pub(crate) balance_after: String,
}

/// Routing and capacity facts read from the matched deal's TokenContract.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SubscriptionLive {
    /// a settlement is owed for the recorded week. `status` reports this WITHOUT booking it,
    /// so a poller can learn that money is due without signing anything; `--settle` is what books
    /// it. Reported after any booking this invocation did, so it is the state as of the answer.
    pub(crate) settlement_due: bool,
    /// When the recorded week ran out -- the "since when" of `settlement_due`. `u64::MAX` when the
    /// deal is not a subscription or its term is over.
    pub(crate) recorded_week_expires_at_unix: u64,
    pub(crate) funded: bool,
    pub(crate) opened: bool,
    pub(crate) probe_accepted: bool,
    pub(crate) disputed: bool,
    pub(crate) terminal: bool,
    pub(crate) sub_weeks: u8,
    pub(crate) week_index: u8,
    pub(crate) period_start_unix: u64,
    pub(crate) week_base_tokens: String,
    pub(crate) tokens_per_week: String,
    pub(crate) tokens_final: String,
    pub(crate) tokens_pending: String,
    pub(crate) used_current_week: String,
    pub(crate) remaining_current_week: String,
    pub(crate) funded_tokens: String,
    pub(crate) tokens_paid: String,
    pub(crate) deposit: String,
    pub(crate) probe_tick: String,
    pub(crate) buyer_bond_held: String,
    pub(crate) buyer_bond_required: String,
    pub(crate) buyer_locked_total: String,
    pub(crate) seller_bond_held: String,
    pub(crate) seller_bond_required: String,
}

pub(crate) struct BuyerEventWriter {
    seq: u64,
    session_id: String,
    #[cfg(test)]
    captured: Option<std::sync::Arc<std::sync::Mutex<Vec<Value>>>>,
}

impl BuyerEventWriter {
    pub(crate) fn new() -> Self {
        let mut bytes = [0u8; 3];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self {
            seq: 0,
            session_id: format!("buyer-{}", hex::encode(bytes)),
            #[cfg(test)]
            captured: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn capturing() -> (Self, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut writer = Self::new();
        writer.captured = Some(captured.clone());
        (writer, captured)
    }

    pub(crate) fn event(
        &mut self,
        event: &'static str,
        operation: &'static str,
        fields: Value,
    ) -> Result<()> {
        self.seq = self.seq.saturating_add(1);
        let mut obj = self.envelope(BUYER_EVENT_SCHEMA, operation)?;
        obj.insert("event".to_string(), json!(event));
        merge_fields(&mut obj, fields);
        self.write(Value::Object(obj))
    }

    pub(crate) fn claim_observation(
        &mut self,
        observation: &dexdo::buyer::api::BuyerClaimObservation,
    ) -> Result<()> {
        self.seq = self.seq.saturating_add(1);
        let deal_handle = super::deals::make_handle_id(
            &observation.token_contract,
            super::deals::DealHandleRole::Buyer,
        );
        self.write(dexdo::runtime_events::buyer_claim_event(
            self.seq,
            now_unix()?,
            &self.session_id,
            OP_BUYER_RUNTIME,
            &deal_handle,
            observation,
        ))
    }

    pub(crate) fn error(
        &mut self,
        operation: &'static str,
        code: ErrorCode,
        fields: Value,
    ) -> Result<()> {
        self.seq = self.seq.saturating_add(1);
        let mut obj = self.envelope(ERROR_SCHEMA, operation)?;
        obj.insert("event".to_string(), json!("error"));
        obj.insert("code".to_string(), json!(code.as_str()));
        obj.insert("message".to_string(), json!(code.safe_message()));
        obj.insert("retryable".to_string(), json!(code.retryable()));
        merge_fields(&mut obj, fields);
        self.write(Value::Object(obj))
    }

    pub(crate) fn error_with_cause(
        &mut self,
        operation: &'static str,
        code: ErrorCode,
        cause: &anyhow::Error,
        mut fields: Value,
    ) -> Result<()> {
        if let Value::Object(obj) = &mut fields {
            obj.insert("cause".to_string(), json!(error_cause(cause)));
        }
        self.error(operation, code, fields)
    }

    fn envelope(
        &self,
        schema: &'static str,
        operation: &'static str,
    ) -> Result<Map<String, Value>> {
        let mut obj = Map::new();
        obj.insert("schema".to_string(), json!(schema));
        obj.insert("seq".to_string(), json!(self.seq));
        obj.insert("ts_unix".to_string(), json!(now_unix()?));
        obj.insert("session_id".to_string(), json!(self.session_id));
        obj.insert("operation".to_string(), json!(operation));
        Ok(obj)
    }

    fn write(&self, value: Value) -> Result<()> {
        #[cfg(test)]
        if let Some(captured) = &self.captured {
            captured
                .lock()
                .expect("buyer event capture lock poisoned")
                .push(value.clone());
        }
        print_json(&value)
    }
}

fn merge_fields(obj: &mut Map<String, Value>, fields: Value) {
    if let Value::Object(fields) = fields {
        for (k, v) in fields {
            obj.insert(k, v);
        }
    }
}

#[cfg(test)]
pub(crate) fn forbidden_machine_fragment(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    let forbidden = [
        "owner_secret_key_hex",
        "private_key",
        "mnemonic",
        "bearer ",
        "api_key",
        "authorization",
        "prompt",
        "provider response",
        "deal_path",
    ];
    forbidden
        .iter()
        .copied()
        .find(|needle| lower.contains(needle))
        .or_else(|| (redact_local_paths(text) != text).then_some("absolute local path"))
}

#[cfg(test)]
#[path = "typed_subscription_status_error_code_is_message_invariant.rs"]
mod typed_subscription_status_error_code_is_message_invariant;

#[cfg(test)]
#[path = "gateway_error_code_is_message_invariant.rs"]
mod gateway_error_code_is_message_invariant;

/// an unbound wallet is its own code, carrying its own remediation -- never `INTERNAL`.
#[cfg(test)]
#[path = "wallet_not_configured_error_code.rs"]
mod wallet_not_configured_error_code;

#[cfg(test)]
mod tests {
    use super::*;

    fn status_error(status: reqwest::StatusCode) -> anyhow::Error {
        let response: reqwest::Response = http::Response::builder()
            .status(status)
            .body(Vec::<u8>::new())
            .expect("build HTTP response")
            .into();
        anyhow::Error::new(
            response
                .error_for_status()
                .expect_err("status must produce reqwest error"),
        )
        .context("order-book getter failed")
    }

    #[test]
    fn stable_schema_constants_match_contract() {
        assert_eq!(MARKETS_SCHEMA, "dexdo.markets.v2");
        assert_eq!(QUOTE_SCHEMA, "dexdo.quote.v2");
        assert_eq!(BUYER_EVENT_SCHEMA, "dexdo.buyer.event.v2");
        assert_eq!(STATUS_SCHEMA, "dexdo.status.v3");
        assert_eq!(DEALS_SCHEMA, "dexdo.deals.v1");
        assert_eq!(CLOSE_SCHEMA, "dexdo.close.v1");
        assert_eq!(SUBSCRIPTION_SCHEMA, "dexdo.subscription.v2");
        assert_eq!(MODEL_REGISTRY_SCHEMA, "dexdo.model_registry.v1");
        assert_eq!(DOCTOR_SCHEMA, "dexdo.doctor.v1");
        assert_eq!(ERROR_SCHEMA, "dexdo.error.v1");
        assert_eq!(OP_DOCTOR, "doctor");
        assert_eq!(OP_SUBSCRIPTION_PLACE, "subscription_place");
        assert_eq!(OP_SUBSCRIPTION_STATUS, "subscription_status");
        assert_eq!(OP_SUBSCRIPTION_CANCEL, "subscription_cancel");
    }

    /// The three codes added are stable strings, and none of them is retryable: retrying an
    /// absent order, a lost cancel race or a contradictory pair of records repeats a money action
    /// against facts that will not change on their own.
    #[test]
    fn subscription_error_codes_are_stable_and_not_retryable() {
        for (code, text) in [
            (ErrorCode::OrderNotFound, "ORDER_NOT_FOUND"),
            (ErrorCode::OrderAlreadyMatched, "ORDER_ALREADY_MATCHED"),
            (ErrorCode::ContradictoryState, "CONTRADICTORY_STATE"),
        ] {
            assert_eq!(code.as_str(), text);
            assert!(!code.retryable(), "{text}");
            assert!(!code.safe_message().is_empty(), "{text}");
        }
    }

    /// The code for "you already have one of these waiting" is stable and not retryable.
    #[test]
    fn a_resting_buy_has_its_own_stable_code() {
        assert_eq!(ErrorCode::BuyAlreadyResting.as_str(), "BUY_ALREADY_RESTING");
        assert!(
            !ErrorCode::BuyAlreadyResting.retryable(),
            "a second run would place a duplicate, not a retry"
        );
        assert!(!ErrorCode::BuyAlreadyResting.safe_message().is_empty());
    }

    #[test]
    fn structured_error_is_stdout_safe() {
        let rendered =
            serde_json::to_string(&MachineError::new(OP_STATUS, ErrorCode::InvalidArgument))
                .unwrap();
        let value: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["schema"], ERROR_SCHEMA);
        assert_eq!(value["operation"], OP_STATUS);
        assert_eq!(value["code"], "INVALID_ARGUMENT");
        assert_eq!(value["retryable"], false);
        assert!(
            forbidden_machine_fragment(&rendered).is_none(),
            "{rendered}"
        );
    }

    #[test]
    fn required_runtime_error_codes_are_stable_and_structured() {
        let cases = [
            (ErrorCode::StaleClient, "STALE_CLIENT", false),
            (ErrorCode::NoLiquidity, "NO_LIQUIDITY", true),
            (ErrorCode::IncompleteQuote, "INCOMPLETE_QUOTE", true),
            (
                ErrorCode::InsufficientBalance,
                "INSUFFICIENT_BALANCE",
                false,
            ),
            (ErrorCode::HandoverTimeout, "HANDOVER_TIMEOUT", true),
            (ErrorCode::ChainTransport, "CHAIN_TRANSPORT", true),
            (ErrorCode::SettlementFailed, "SETTLEMENT_FAILED", true),
            (ErrorCode::NotRecoverableYet, "NOT_RECOVERABLE_YET", true),
            (ErrorCode::DisputedDeal, "DISPUTED_DEAL", false),
        ];
        for (code, name, retryable) in cases {
            let rendered = serde_json::to_string(&MachineError::new(OP_BUYER_START, code)).unwrap();
            let value: Value = serde_json::from_str(&rendered).unwrap();
            assert_eq!(value["schema"], ERROR_SCHEMA);
            assert_eq!(value["operation"], OP_BUYER_START);
            assert_eq!(value["code"], name);
            assert_eq!(value["retryable"], retryable);
            assert!(value["message"].as_str().is_some_and(|s| !s.is_empty()));
            assert!(
                forbidden_machine_fragment(&rendered).is_none(),
                "{rendered}"
            );
        }
    }

    #[test]
    fn required_runtime_errors_classify_before_generic_invalid_argument() {
        let cases = [
            (
                anyhow::anyhow!("buyer quote: no liquidity for required quote"),
                ErrorCode::NoLiquidity,
            ),
            (
                anyhow::anyhow!("incomplete quote: not enough depth for required ticks"),
                ErrorCode::IncompleteQuote,
            ),
            (
                anyhow::anyhow!("insufficient balance for required deposit"),
                ErrorCode::InsufficientBalance,
            ),
            (
                anyhow::anyhow!("handover within deadline failed"),
                ErrorCode::HandoverTimeout,
            ),
            (
                anyhow::Error::new(dexdo_core::ChainError::Transport(
                    "rpc disconnected".to_string(),
                )),
                ErrorCode::ChainTransport,
            ),
            (
                anyhow::anyhow!("settlement streamStop submission failed"),
                ErrorCode::SettlementFailed,
            ),
            (
                anyhow::anyhow!("not recoverable yet: after MATCH_OPEN_TIMEOUT"),
                ErrorCode::NotRecoverableYet,
            ),
            (anyhow::anyhow!("deal is disputed"), ErrorCode::DisputedDeal),
            (
                anyhow::anyhow!(
                    "buyer model-only preflight failed for InferenceOrderBook 0:book: no executable matching ask after skipping unreadable or already-used TokenContracts"
                ),
                ErrorCode::NoLiquidity,
            ),
            (
                anyhow::anyhow!(
                    "buyer model-only preflight failed for InferenceOrderBook 0:book: best ask price 11 is above buyer max_price_per_tick 10"
                ),
                ErrorCode::NoLiquidity,
            ),
            (
                anyhow::anyhow!(
                    format!("buyer explicit-token quote preflight: {}: buyer target preflight failed for InferenceOrderBook 0:book: refusing multi-ask fill", dexdo_core::params::current_network())
                ),
                ErrorCode::NoLiquidity,
            ),
            (
                anyhow::anyhow!(
                    "buyer target preflight failed for InferenceOrderBook 0:book: placeInferenceBuy cannot target a TokenContract; refusing to send escrow into the wrong deal"
                ),
                ErrorCode::NoLiquidity,
            ),
            (
                anyhow::anyhow!(
                    "buyer model-only preflight failed for InferenceOrderBook 0:book: raw order-book matcher would select order , but executable quote selected order "
                ),
                ErrorCode::NoLiquidity,
            ),
            (
                anyhow::anyhow!(
                    "selected TokenContract 0:tc is already used by chain state (funded); refusing to move escrow"
                ),
                ErrorCode::ChainRevert,
            ),
            (
                anyhow::anyhow!(
                    "invalid buy ticks: --ticks 1 is below the 2-tick stream minimum"
                ),
                ErrorCode::InvalidArgument,
            ),
        ];
        for (err, code) in cases {
            assert_eq!(classify_error(OP_BUYER_START, &err), code);
        }
    }

    #[test]
    fn classifier_does_not_map_our_own_chain_prefixed_errors_to_chain_transport() {
        let err = anyhow::anyhow!(format!(
            "{}: seller offer did not rest after accepted postSellOffer",
            dexdo_core::params::current_network()
        ));
        assert_eq!(classify_error(OP_BUYER_START, &err), ErrorCode::Internal);
    }

    #[test]
    fn buyer_context_labels_do_not_trigger_marker_classification() {
        for context in [
            "buyer model-only quote preflight",
            "buyer explicit-token quote preflight",
            "place model-only buy after pool preflight",
            "could not read a submit-safe/trustworthy order book for qwen",
            "lazy buyer initialization failed",
        ] {
            let err = anyhow::anyhow!("unclassified buyer failure").context(context);
            assert_eq!(
                classify_error(OP_BUYER_START, &err),
                ErrorCode::Internal,
                "context unexpectedly matched a classifier marker: {err:#}"
            );
        }
    }

    #[test]
    fn contract_revert_is_not_chain_transport() {
        let err = anyhow::Error::new(dexdo_core::ChainError::Contract(
            "ERR_ALREADY_OPEN exit_code=321".to_string(),
        ));
        assert_eq!(classify_error(OP_BUYER_START, &err), ErrorCode::ChainRevert);
    }

    #[test]
    fn ambiguous_submit_is_dedicated_and_terminal() {
        let err = anyhow::Error::new(dexdo_core::ChainError::AmbiguousSubmit(
            "invalid balance response left outcome unknown".to_string(),
        ));
        let code = classify_error(OP_BUYER_START, &err);
        assert_eq!(code, ErrorCode::AmbiguousSubmit);
        assert_eq!(code.as_str(), "AMBIGUOUS_SUBMIT");
        assert!(!code.retryable());
    }

    #[test]
    fn buyer_withdrawn_preflight_is_actionable_chain_revert_not_transport() {
        let err = anyhow::anyhow!(
            "buyer place aborted: this note has withdrawn and can no longer place buys \
             (deploy/use a fresh note); the chain rejects it with ERR_INVALID_STATE 151 because \
             PrivateNote._hasWithdrawn=true"
        );
        let code = classify_error(OP_BUYER_START, &err);
        assert_eq!(code, ErrorCode::ChainRevert);
        assert_ne!(code, ErrorCode::ChainTransport);
        assert_eq!(code.as_str(), "CHAIN_REVERT");
        assert!(!code.retryable());
    }

    #[test]
    fn duplicate_sell_refusal_is_exact_and_not_chain_transport() {
        let message = "this TokenContract already has a live resting SELL";
        let err = anyhow::Error::new(dexdo_core::ChainError::DuplicateSell(message.to_string()));
        assert_eq!(classify_error("seller_start", &err), ErrorCode::ChainRevert);
        assert_eq!(error_cause(&err), message);
    }

    #[test]
    fn transport_failure_is_chain_transport() {
        let err = anyhow::Error::new(dexdo_core::ChainError::Transport(
            "connect timed out at https://net-a.example/graphql".to_string(),
        ));
        assert_eq!(
            classify_error(OP_BUYER_START, &err),
            ErrorCode::ChainTransport
        );
        let rendered = serde_json::to_value(
            MachineError::new(OP_BUYER_START, ErrorCode::ChainTransport)
                .with_cause(error_cause(&err)),
        )
        .unwrap();
        assert_eq!(
            rendered["cause"],
            // `chain transport`, not a hard-coded network name: the variant belongs to the market type
            // and fires for a transport fault on WHATEVER chain the operator is on. The test name
            // already said `is_chain_transport`; only its expected string had the test network in
            // it, which is the wording removes.
            // The network is DERIVED, not spelled: it comes from the manifest DEXDO_MANIFEST
            // names, through the same call production uses. A test pinned to the literal would put
            // a network name back into the sources -- the very thing removes -- and would
            // fail for anyone running it against a different manifest.
            format!(
                "{} transport: connect timed out at https://net-a.example/graphql",
                dexdo_core::params::current_network()
            )
        );

        let wrapped = anyhow::Error::new(dexdo_core::ChainError::Transport(
            "connection reset by peer".to_string(),
        ))
        .context("buyer startup failed");
        assert_eq!(
            error_cause(&wrapped),
            format!(
                "buyer startup failed: {} transport: connection reset by peer",
                dexdo_core::params::current_network()
            )
        );
    }

    #[test]
    fn reqwest_http_4xx_is_not_chain_transport_but_5xx_and_429_are() {
        for status in [
            reqwest::StatusCode::BAD_REQUEST,
            reqwest::StatusCode::NOT_FOUND,
        ] {
            let error = status_error(status);
            assert_eq!(classify_error(OP_STATUS, &error), ErrorCode::Internal);
            let cause = error_cause(&error);
            assert!(cause.contains("order-book getter failed"), "{cause}");
            assert!(cause.contains(status.as_str()), "{cause}");
        }
        for status in [
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
        ] {
            let error = status_error(status);
            assert_eq!(classify_error(OP_STATUS, &error), ErrorCode::ChainTransport);
            let cause = error_cause(&error);
            assert!(cause.contains("order-book getter failed"), "{cause}");
            assert!(cause.contains(status.as_str()), "{cause}");
        }
    }

    #[test]
    fn buyer_error_serializes_jsonl_envelope() {
        let mut obj = Map::new();
        obj.insert("schema".to_string(), json!(ERROR_SCHEMA));
        obj.insert("seq".to_string(), json!(6));
        obj.insert("event".to_string(), json!("error"));
        obj.insert("ts_unix".to_string(), json!(1782910310u64));
        obj.insert("session_id".to_string(), json!("buyer-test"));
        obj.insert("operation".to_string(), json!(OP_BUYER_START));
        obj.insert(
            "code".to_string(),
            json!(ErrorCode::HandoverTimeout.as_str()),
        );
        obj.insert(
            "message".to_string(),
            json!(ErrorCode::HandoverTimeout.safe_message()),
        );
        obj.insert("retryable".to_string(), json!(true));
        let rendered = serde_json::to_string(&Value::Object(obj)).unwrap();
        let value: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["schema"], ERROR_SCHEMA);
        assert_eq!(value["event"], "error");
        assert_eq!(value["seq"], 6);
    }

    #[test]
    fn redaction_guard_rejects_secret_and_path_fragments() {
        assert_eq!(
            forbidden_machine_fragment(r#"{"owner_secret_key_hex":"abc"}"#),
            Some("owner_secret_key_hex")
        );
        assert_eq!(
            forbidden_machine_fragment(r#"{"deal_path":"/tmp/deal.json"}"#),
            Some("deal_path")
        );
        assert!(forbidden_machine_fragment(r#"{"deal_handle":"deal-0-abc"}"#).is_none());
    }

    #[test]
    fn machine_error_redacts_absolute_paths_without_a_root_allowlist() {
        for path in [
            "/app/private/note.key",
            "/data/dexdo/pool.json",
            "/workspace/deals/current.json",
            "/custom-root/runtime/state",
        ] {
            let cause = sanitize_error_cause(&format!("failed to read {path}"));
            assert_eq!(cause, "failed to read <redacted-path>");
            assert_eq!(
                forbidden_machine_fragment(&format!(r#"{{"cause":"{path}"}}"#)),
                Some("absolute local path")
            );
        }
    }

    #[test]
    fn machine_error_path_redaction_preserves_public_urls_and_relative_paths() {
        let cause = "request https://net-a.example/graphql failed in cache/state.json";
        assert_eq!(sanitize_error_cause(cause), cause);
        assert!(forbidden_machine_fragment(cause).is_none());
    }
}
