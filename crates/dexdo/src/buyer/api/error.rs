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
            Self::DealCapacityUnavailable => "retry_after_reconciliation",
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
    let busy = error.downcast_ref::<tonic::Status>().is_some_and(|status| {
        status
            .metadata()
            .get("dexdo-error-code")
            .is_some_and(|code| code == "deal_busy")
    });
    if busy {
        ConsumerError::new(ConsumerErrorCode::DealBusy,
            "the deal's concurrent request limit is reached; retry with backoff after an active request finishes")
    } else {
        ConsumerError::new(ConsumerErrorCode::DealCapacityUnavailable,
            format!("the deal has no delivery capacity for this request yet; retry after seller capacity reconciliation: {error}"))
    }
}
