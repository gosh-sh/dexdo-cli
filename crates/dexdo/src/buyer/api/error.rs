//! Stable causes for pre-answer refusals. Messages remain human-readable diagnostics.
use super::*;
use axum::response::IntoResponse;

pub(super) fn bytes_rejection(
    error: axum::extract::rejection::BytesRejection,
    anthropic: bool,
) -> Response {
    let status = error.status();
    if status != http::StatusCode::PAYLOAD_TOO_LARGE {
        return error.into_response();
    }
    let mut body = serde_json::json!({"error": {
        "type": "invalid_request_error",
        "message": format!("request body exceeds the maximum of {BUYER_API_REQUEST_BODY_MAX_BYTES} bytes"),
    }});
    if anthropic {
        body["type"] = serde_json::json!("error");
    }
    (status, axum::Json(body)).into_response()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumerErrorCode {
    DealUnavailable,
    DealStateUnavailable,
    OrderUnavailable,
    DealNotOpened,
    PurchaseReconciliationRequired,
    InvalidBuyTicks,
    DealBudgetExhausted,
    RenewalPending,
    SubscriptionBudgetExhausted,
    SubscriptionStateUnavailable,
    SubscriptionTerminal,
    VerificationBudgetInsufficient,
    VerificationGrantConsumed,
    DealBusy,
    DealCapacityUnavailable,
    OnchainDealLimit,
    UpstreamRetryable,
    UpstreamUsageUnresolved,
    UnsupportedApiPath,
    SellerUpdateRequired,
}

impl ConsumerErrorCode {
    fn recovery_action(self) -> &'static str {
        match self {
            Self::OrderUnavailable | Self::VerificationGrantConsumed => "retry",
            Self::DealBusy
            | Self::RenewalPending
            | Self::DealStateUnavailable
            | Self::UpstreamRetryable => "retry_with_backoff",
            Self::SubscriptionBudgetExhausted => "wait_for_next_cycle",
            Self::DealBudgetExhausted => "new_purchase",
            Self::DealNotOpened
            | Self::PurchaseReconciliationRequired
            | Self::SubscriptionStateUnavailable => "read_only_reconciliation",
            Self::DealCapacityUnavailable | Self::OnchainDealLimit => "retry_after_reconciliation",
            Self::UpstreamUsageUnresolved => "read_only_reconciliation",
            Self::InvalidBuyTicks | Self::UnsupportedApiPath => "change_request",
            Self::SellerUpdateRequired => "wait_for_seller_update",
            Self::DealUnavailable
            | Self::VerificationBudgetInsufficient
            | Self::SubscriptionTerminal => "operator_action",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumerError {
    pub code: ConsumerErrorCode,
    pub message: String,
}

impl ConsumerError {
    pub(super) fn new(code: ConsumerErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub(super) fn body(&self, anthropic: bool) -> serde_json::Value {
        let mut body = serde_json::json!({"error": {
            "type": "invalid_request_error",
            "code": self.code,
            "message": self.message,
            "recovery_action": self.code.recovery_action(),
        }});
        if anthropic {
            body["type"] = serde_json::json!("error");
        }
        body
    }

    pub(super) fn response(&self, status: http::StatusCode, anthropic: bool) -> Response {
        let status = if self.code == ConsumerErrorCode::UpstreamUsageUnresolved {
            http::StatusCode::SERVICE_UNAVAILABLE
        } else {
            status
        };
        (status, axum::Json(self.body(anthropic))).into_response()
    }
}

impl fmt::Display for ConsumerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConsumerError {}

// Existing weekly-state calculations return diagnostics; failures must never be called exhaustion.
impl From<String> for ConsumerError {
    fn from(message: String) -> Self {
        Self::new(ConsumerErrorCode::SubscriptionStateUnavailable, message)
    }
}

impl DealInitError {
    pub fn with_code(code: ConsumerErrorCode, message: impl Into<String>) -> Self {
        Self {
            error: ConsumerError::new(code, message),
            reconciliation: None,
        }
    }

    pub fn code(&self) -> ConsumerErrorCode {
        self.error.code
    }

    pub(super) fn response(&self, anthropic: bool) -> Response {
        let mut body = self.error.body(anthropic);
        if let Some(reconciliation) = self.reconciliation() {
            body["error"]["submit_reconciliation"] = serde_json::json!(reconciliation);
        }
        // Keep each adapter's existing status policy; this change adds machine causes.
        let status = if !anthropic && self.code() == ConsumerErrorCode::InvalidBuyTicks {
            http::StatusCode::BAD_REQUEST
        } else {
            http::StatusCode::SERVICE_UNAVAILABLE
        };
        (status, axum::Json(body)).into_response()
    }
}

impl ApiState {
    pub(super) fn budget_rejection(&self, mut error: ConsumerError, anthropic: bool) -> Response {
        if error.code == ConsumerErrorCode::DealBudgetExhausted && self.renewal_enabled {
            error.code = ConsumerErrorCode::RenewalPending;
            error.message =
                "active deal budget exhausted; waiting for renewal handover".to_string();
        }
        error.response(http::StatusCode::SERVICE_UNAVAILABLE, anthropic)
    }
}

/// Older sellers have no typed metadata: preserve their generic reconciliation guidance.
pub(super) fn capacity_rejection(error: &anyhow::Error) -> ConsumerError {
    let status = error.downcast_ref::<tonic::Status>();
    let busy = status.is_some_and(|status| {
        status
            .metadata()
            .get("dexdo-error-code")
            .is_some_and(|code| code == "deal_busy")
    });
    let onchain = status.is_some_and(|status| {
        status
            .metadata()
            .get("dexdo-limit")
            .is_some_and(|value| value == "onchain_deal_limit")
    });
    if onchain {
        ConsumerError::new(ConsumerErrorCode::OnchainDealLimit,
            "the deal's unclaimed billable backlog reached its on-chain throughput limit; retry after the next claim is reconciled")
    } else if busy {
        ConsumerError::new(ConsumerErrorCode::DealBusy,
            "the deal's concurrent request limit is reached; retry with backoff after an active request finishes")
    } else {
        ConsumerError::new(ConsumerErrorCode::DealCapacityUnavailable,
            format!("the deal has no delivery capacity for this request yet; retry after seller capacity reconciliation: {error}"))
    }
}

pub(super) fn capacity_response(error: &anyhow::Error, anthropic: bool) -> Response {
    let refusal = capacity_rejection(error);
    let mut response = refusal.response(http::StatusCode::TOO_MANY_REQUESTS, anthropic);
    if refusal.code == ConsumerErrorCode::OnchainDealLimit {
        response
            .headers_mut()
            .insert("dexdo-limit", "onchain_deal_limit".parse().unwrap());
    }
    if let Some(delay) = error
        .downcast_ref::<tonic::Status>()
        .and_then(|status| status.metadata().get("dexdo-retry-after-ms"))
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|delay| *delay > 0)
    {
        retry_headers(response.headers_mut(), delay);
    }
    response
}

pub(super) fn retry_headers(headers: &mut http::HeaderMap, milliseconds: u64) {
    headers.insert(
        http::header::RETRY_AFTER,
        milliseconds.div_ceil(1000).max(1).into(),
    );
    headers.insert("retry-after-ms", milliseconds.into());
}

/// Preserve provider hints, filling only a missing seconds header or a wholly missing hint.
pub(super) fn provider_retry_headers(headers: &mut http::HeaderMap, body: &[u8]) {
    if headers.contains_key(http::header::RETRY_AFTER) {
        return;
    }
    if let Some(milliseconds) = headers.get("retry-after-ms") {
        if let Some(milliseconds) = milliseconds
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        {
            headers.insert(
                http::header::RETRY_AFTER,
                milliseconds.div_ceil(1000).max(1).into(),
            );
        }
        return;
    }
    if let Ok(body) = serde_json::from_slice::<serde_json::Value>(body) {
        let error = &body["error"];
        if ["code", "type"].into_iter().any(|field| {
            matches!(
                error[field].as_str(),
                Some(
                    "insufficient_quota"
                        | "credit_balance_exhausted"
                        | "billing_error"
                        | "organization_spend_limit_exceeded"
                        | "project_spend_limit_exceeded"
                        | "organization_usage_limit_exceeded"
                        | "usage_limit_exceeded"
                )
            )
        }) || error["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("You have reached your API usage limits:"))
        {
            return;
        }
    }
    retry_headers(
        headers,
        dexdo_core::params::CAPACITY_RETRY_DEFAULT.as_millis() as u64,
    );
}
