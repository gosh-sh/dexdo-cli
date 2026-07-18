use anyhow::Result;
use rand::RngCore;
use serde::Serialize;
use serde_json::{json, Map, Value};

pub(crate) const MARKETS_SCHEMA: &str = "dexdo.markets.v1";
pub(crate) const QUOTE_SCHEMA: &str = "dexdo.quote.v1";
pub(crate) const BUYER_EVENT_SCHEMA: &str = "dexdo.buyer.event.v1";
pub(crate) const STATUS_SCHEMA: &str = "dexdo.status.v1";
pub(crate) const CLOSE_SCHEMA: &str = "dexdo.close.v1";
pub(crate) const ERROR_SCHEMA: &str = "dexdo.error.v1";

pub(crate) const OP_MARKETS: &str = "markets";
pub(crate) const OP_QUOTE: &str = "quote";
pub(crate) const OP_BUYER_START: &str = "buyer_start";
pub(crate) const OP_BUYER_RUNTIME: &str = "buyer_runtime";
pub(crate) const OP_BUYER_SHUTDOWN: &str = "buyer_shutdown";
pub(crate) const OP_STATUS: &str = "status";
pub(crate) const OP_CLOSE: &str = "close";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorCode {
    InvalidArgument,
    FeatureUnavailable,
    #[allow(dead_code)]
    MarketNotFound,
    #[allow(dead_code)]
    MarketInactive,
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
    ChainRevert,
    AmbiguousSubmit,
    SettlementFailed,
    NotRecoverableYet,
    DisputedDeal,
    #[allow(dead_code)]
    PolicyFailClosed,
    Internal,
}

impl ErrorCode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::FeatureUnavailable => "FEATURE_UNAVAILABLE",
            Self::MarketNotFound => "MARKET_NOT_FOUND",
            Self::MarketInactive => "MARKET_INACTIVE",
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
            Self::ChainRevert => "CHAIN_REVERT",
            Self::AmbiguousSubmit => "AMBIGUOUS_SUBMIT",
            Self::SettlementFailed => "SETTLEMENT_FAILED",
            Self::NotRecoverableYet => "NOT_RECOVERABLE_YET",
            Self::DisputedDeal => "DISPUTED_DEAL",
            Self::PolicyFailClosed => "POLICY_FAIL_CLOSED",
            Self::Internal => "INTERNAL",
        }
    }

    pub(crate) fn retryable(self) -> bool {
        matches!(
            self,
            Self::MarketNotFound
                | Self::MarketInactive
                | Self::NoLiquidity
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
            Self::FeatureUnavailable => "requested feature is unavailable in this binary",
            Self::MarketNotFound => "market was not found",
            Self::MarketInactive => "market is inactive",
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
            Self::ChainRevert => "chain returned a non-success contract result",
            Self::AmbiguousSubmit => "money submit outcome is unknown and must not be retried",
            Self::SettlementFailed => "settlement submission failed",
            Self::NotRecoverableYet => "deal is not recoverable yet",
            Self::DisputedDeal => "deal is disputed and needs dispute resolution",
            Self::PolicyFailClosed => "runtime policy failed closed",
            Self::Internal => "internal invariant failed",
        }
    }
}

pub(crate) fn classify_error(operation: &str, err: &anyhow::Error) -> ErrorCode {
    let msg = format!("{err:#}").to_ascii_lowercase();
    for cause in err.chain() {
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
        if cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest_error_is_transport)
        {
            return ErrorCode::ChainTransport;
        }
    }
    if msg.contains("unavailable: build with") {
        return ErrorCode::FeatureUnavailable;
    }
    if msg.contains("no liquidity") {
        return ErrorCode::NoLiquidity;
    }
    if msg.contains("no executable matching ask")
        || msg.contains("no matchable ask")
        || msg.contains("executable quote depth has no matching")
        || msg.contains("refusing multi-ask fill")
        || msg.contains("placeinferencebuy cannot target")
        || msg.contains("raw order-book matcher")
        || msg.contains("refusing to send escrow into the wrong deal")
        || (msg.contains("best ask price") && msg.contains("above buyer max_price_per_tick"))
    {
        return ErrorCode::NoLiquidity;
    }
    if msg.contains("incomplete quote") || msg.contains("not enough") {
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
        || msg.contains("placeinferencebuy cannot target")
        || msg.contains("refusing to send escrow into the wrong deal")
    {
        return ErrorCode::ChainRevert;
    }
    if msg.contains("insufficient") || msg.contains("balance") || msg.contains("deposit") {
        return ErrorCode::InsufficientBalance;
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
    if msg.contains("malformed handover") || msg.contains("handover decrypt failed") {
        return ErrorCode::HandoverDecryptFailed;
    }
    if operation == OP_BUYER_START && (msg.contains("address in use") || msg.contains("bind")) {
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

#[derive(Debug, Clone, Serialize)]
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
        }
    }

    pub(crate) fn with_cause(mut self, cause: impl Into<String>) -> Self {
        self.cause = Some(cause.into());
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
    print_json(&MachineError::new(operation, code).with_cause(error_cause(err)))
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
    const UNIX_ROOTS: &[&str] = &[
        "/tmp/", "/home/", "/media/", "/root/", "/var/", "/opt/", "/users/", "/etc/", "/usr/",
        "/srv/", "/run/", "/mnt/",
    ];

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

    fn starts_local_path(text: &str, lower: &str, start: usize) -> bool {
        let previous = text[..start].chars().next_back();
        if !is_boundary(previous) {
            return false;
        }
        let rest = &lower[start..];
        if UNIX_ROOTS.iter().any(|root| rest.starts_with(root)) || rest.starts_with("\\\\") {
            return true;
        }
        let bytes = &text.as_bytes()[start..];
        bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/')
    }

    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while cursor < text.len() {
        if starts_local_path(text, &lower, cursor) {
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

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MarketsResponse {
    pub(crate) schema: &'static str,
    pub(crate) network: String,
    pub(crate) generated_at_unix: u64,
    pub(crate) markets: Vec<MarketEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MarketEntry {
    pub(crate) frame_model: String,
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

#[derive(Debug, Clone, Serialize)]
pub(crate) struct QuoteResponse {
    pub(crate) schema: &'static str,
    pub(crate) network: String,
    pub(crate) generated_at_unix: u64,
    pub(crate) frame_model: String,
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

#[derive(Debug, Clone, Serialize)]
pub(crate) struct QuoteRequest {
    pub(crate) kind: &'static str,
    pub(crate) ticks: Option<String>,
    pub(crate) budget: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct QuoteFillEntry {
    pub(crate) order_id: String,
    pub(crate) token_contract: String,
    pub(crate) ticks: String,
    pub(crate) price_per_tick: String,
    pub(crate) cost_without_fee: String,
    pub(crate) platform_fee: String,
    pub(crate) cost_with_fee: String,
}

#[derive(Debug, Clone, Serialize)]
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

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusAccounting {
    pub(crate) finalized_owed: String,
    pub(crate) buyer_locked: String,
    pub(crate) deposit: String,
    pub(crate) prepaid: String,
    pub(crate) frozen: String,
    pub(crate) last_advance_unix: Option<u64>,
    pub(crate) funded_time_unix: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StatusNext {
    pub(crate) action: String,
    pub(crate) retryable_after_unix: Option<u64>,
    pub(crate) command: String,
}

#[derive(Debug, Clone, Serialize)]
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
    pub(crate) tx: Option<Value>,
}

pub(crate) struct BuyerEventWriter {
    seq: u64,
    session_id: String,
    #[cfg(all(test, feature = "shellnet"))]
    captured: Option<std::sync::Arc<std::sync::Mutex<Vec<Value>>>>,
}

impl BuyerEventWriter {
    pub(crate) fn new() -> Self {
        let mut bytes = [0u8; 3];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self {
            seq: 0,
            session_id: format!("buyer-{}", hex::encode(bytes)),
            #[cfg(all(test, feature = "shellnet"))]
            captured: None,
        }
    }

    #[cfg(all(test, feature = "shellnet"))]
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
        #[cfg(all(test, feature = "shellnet"))]
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

#[allow(dead_code)]
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
        "/tmp/",
        "/home/",
        "/media/",
        "\\users\\",
    ];
    forbidden
        .iter()
        .copied()
        .find(|needle| lower.contains(needle))
}

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
        assert_eq!(MARKETS_SCHEMA, "dexdo.markets.v1");
        assert_eq!(QUOTE_SCHEMA, "dexdo.quote.v1");
        assert_eq!(BUYER_EVENT_SCHEMA, "dexdo.buyer.event.v1");
        assert_eq!(STATUS_SCHEMA, "dexdo.status.v1");
        assert_eq!(CLOSE_SCHEMA, "dexdo.close.v1");
        assert_eq!(ERROR_SCHEMA, "dexdo.error.v1");
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
                    "buyer explicit-token quote preflight: shellnet: buyer target preflight failed for InferenceOrderBook 0:book: refusing multi-ask fill"
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
    fn classifier_does_not_map_our_own_shellnet_prefixed_errors_to_chain_transport() {
        let err =
            anyhow::anyhow!("shellnet: seller offer did not rest after accepted postSellOffer");
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
            "connect timed out at https://shellnet.ackinacki.org/graphql".to_string(),
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
            "shellnet transport: connect timed out at https://shellnet.ackinacki.org/graphql"
        );

        let wrapped = anyhow::Error::new(dexdo_core::ChainError::Transport(
            "connection reset by peer".to_string(),
        ))
        .context("buyer startup failed");
        assert_eq!(
            error_cause(&wrapped),
            "buyer startup failed: shellnet transport: connection reset by peer"
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
}
