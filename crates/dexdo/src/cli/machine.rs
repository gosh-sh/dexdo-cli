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
    MarketNotFound,
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
    SettlementFailed,
    NotRecoverableYet,
    DisputedDeal,
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
            Self::SettlementFailed => "settlement submission failed",
            Self::NotRecoverableYet => "deal is not recoverable yet",
            Self::DisputedDeal => "deal is disputed and needs dispute resolution",
            Self::PolicyFailClosed => "runtime policy failed closed",
            Self::Internal => "internal invariant failed",
        }
    }
}

pub(crate) fn classify_error(operation: &str, err: &anyhow::Error) -> ErrorCode {
    let msg = err.to_string().to_ascii_lowercase();
    if msg.contains("unavailable: build with") {
        return ErrorCode::FeatureUnavailable;
    }
    if msg.contains("no liquidity") {
        return ErrorCode::NoLiquidity;
    }
    if msg.contains("no executable matching ask")
        || msg.contains("no matchable ask")
        || msg.contains("executable quote depth has no matching")
    {
        return ErrorCode::NoLiquidity;
    }
    if msg.contains("incomplete quote") || msg.contains("not enough") {
        return ErrorCode::IncompleteQuote;
    }
    if msg.contains("selected tokencontract") || msg.contains("refusing to move escrow") {
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
    if msg.contains("shellnet") || msg.contains("transport") || msg.contains("rpc") {
        return ErrorCode::ChainTransport;
    }
    ErrorCode::Internal
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct MachineError {
    pub(crate) schema: &'static str,
    pub(crate) operation: &'static str,
    pub(crate) code: &'static str,
    pub(crate) message: &'static str,
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
}

pub(crate) fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

pub(crate) fn print_short_error(operation: &'static str, code: ErrorCode) -> Result<()> {
    print_json(&MachineError::new(operation, code))
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
}

impl BuyerEventWriter {
    pub(crate) fn new() -> Self {
        let mut bytes = [0u8; 3];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self {
            seq: 0,
            session_id: format!("buyer-{}", hex::encode(bytes)),
        }
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
        print_json(&Value::Object(obj))
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
        print_json(&Value::Object(obj))
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
}

fn merge_fields(obj: &mut Map<String, Value>, fields: Value) {
    if let Value::Object(fields) = fields {
        for (k, v) in fields {
            obj.insert(k, v);
        }
    }
}

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
                anyhow::anyhow!("shellnet rpc transport disconnected"),
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
                    "buyer target preflight failed for InferenceOrderBook 0:book: placeInferenceBuy cannot target a TokenContract; refusing to send escrow into the wrong deal"
                ),
                ErrorCode::ChainRevert,
            ),
            (
                anyhow::anyhow!(
                    "selected TokenContract 0:tc is already used by chain state (funded); refusing to move escrow"
                ),
                ErrorCode::ChainRevert,
            ),
        ];
        for (err, code) in cases {
            assert_eq!(classify_error(OP_BUYER_START, &err), code);
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
