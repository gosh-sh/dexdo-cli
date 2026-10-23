//! Read-only diagnostics for the local serving gate. These records never drive money operations.
use super::*;
use axum::response::IntoResponse;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ClosureCategory {
    PlannedShutdown,
    NoDelivery,
    ChainStateUnavailable,
    DealNotOpen,
    TerminalObserved,
    SessionStop,
    VerificationBail,
    DeadGateway,
    EmptyStream,
    SellerStallsMidStream,
    Recovery,
}

impl ClosureCategory {
    pub(super) fn failure_class(class: &str) -> Self {
        match class {
            "dead_gateway" => Self::DeadGateway,
            "empty_stream" => Self::EmptyStream,
            "seller_stalls_mid_stream" => Self::SellerStallsMidStream,
            _ => Self::Recovery,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(super) struct ServingClosure {
    pub closure_id: String,
    pub category: ClosureCategory,
    pub policy_action: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RecoveryState {
    #[default]
    NotAttempted,
    Pending,
    AttemptedUnconfirmed,
    SubmissionAmbiguous,
    TerminalConfirmed,
    Disputed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum KnownChainState {
    Terminal,
    Disputed,
}

#[derive(Default)]
pub(super) struct ServingDiagnostics {
    pub closure: Option<ServingClosure>,
    /// A known verification verdict outlives cancellation of an earlier recovery attempt.
    pub verification_bailed: bool,
    /// Attempt bookkeeping is independent of authoritative deal knowledge.
    pub recovery_state: RecoveryState,
    pub known_chain_state: Option<KnownChainState>,
}

impl ServingDiagnostics {
    pub(super) fn record_chain_state(&mut self, state: KnownChainState) {
        // The same TokenContract cannot reopen after a strict terminal fact. A dispute, however,
        // can be followed by terminal settlement, including while an older attempt is returning.
        if self.known_chain_state != Some(KnownChainState::Terminal) {
            self.known_chain_state = Some(state);
        }
    }
}

impl SessionSettle {
    pub(super) fn close_local_api(
        &self,
        category: ClosureCategory,
        policy_action: Option<&str>,
    ) -> bool {
        if category == ClosureCategory::TerminalObserved {
            self.close_usage_quota();
        }
        let mut diagnostics = self
            .diagnostics
            .lock()
            .expect("serving diagnostics lock poisoned");
        diagnostics.verification_bailed |= category == ClosureCategory::VerificationBail;
        if diagnostics.closure.is_some() {
            return false;
        }
        let closure = ServingClosure {
            closure_id: consumer_request_id(),
            category,
            policy_action: policy_action.map(str::to_string),
        };
        tracing::warn!(
            closure_id = %closure.closure_id,
            closure_category = ?category,
            policy_action,
            token_contract = %display_token_contract(&self.token_contract),
            "consumer API: local serving closed"
        );
        diagnostics.closure = Some(closure);
        self.closed_tx.send_replace(true);
        true
    }

    pub(super) fn recovery_pending(&self) -> RecoveryState {
        let mut diagnostics = self
            .diagnostics
            .lock()
            .expect("serving diagnostics lock poisoned");
        std::mem::replace(&mut diagnostics.recovery_state, RecoveryState::Pending)
    }

    pub(super) fn recovery_failed(&self, error: &dexdo_core::ChainError) {
        self.diagnostics
            .lock()
            .expect("serving diagnostics lock poisoned")
            .recovery_state = if matches!(error, dexdo_core::ChainError::AmbiguousSubmit(_)) {
            RecoveryState::SubmissionAmbiguous
        } else {
            RecoveryState::AttemptedUnconfirmed
        };
    }

    /// Snapshot only: no chain reads, BUY, STOP or retry. Terminal knowledge comes from existing
    /// authoritative settlement/reconciliation paths; a local closure alone proves none of them.
    pub(super) fn closed_rejection(&self, request_id: &str, anthropic: bool) -> Option<Response> {
        let diagnostics = self
            .diagnostics
            .lock()
            .expect("serving diagnostics lock poisoned");
        let closure = diagnostics.closure.as_ref()?;
        let settled = self.is_settled();
        let terminal_action = self.terminal_action();
        let state = match diagnostics.known_chain_state {
            Some(KnownChainState::Terminal) => RecoveryState::TerminalConfirmed,
            Some(KnownChainState::Disputed) => RecoveryState::Disputed,
            None if settled => {
                if terminal_action == Some(SessionTerminalAction::StreamDispute) {
                    RecoveryState::Disputed
                } else {
                    RecoveryState::TerminalConfirmed
                }
            }
            None if self.recovery_submit_may_have_landed.load(Ordering::SeqCst) => {
                RecoveryState::SubmissionAmbiguous
            }
            None => diagnostics.recovery_state,
        };
        // Accounting uncertainty overrides local policy closure, not authoritative terminal facts.
        if !matches!(
            state,
            RecoveryState::TerminalConfirmed | RecoveryState::Disputed
        ) {
            if let Some(error) = self.unresolved_usage_rejection() {
                return Some(error.response(http::StatusCode::SERVICE_UNAVAILABLE, anthropic));
            }
        }
        let terminal_action = match state {
            RecoveryState::TerminalConfirmed => Some(match terminal_action {
                None | Some(SessionTerminalAction::StreamDispute) => {
                    SessionTerminalAction::ObservedTerminal
                }
                Some(action) => action,
            }),
            RecoveryState::Disputed => {
                terminal_action.filter(|action| *action == SessionTerminalAction::StreamDispute)
            }
            _ => None,
        };
        let (action, message) = match state {
            RecoveryState::Pending => ("wait_for_reconciliation", "Deal serving is closed; recovery is pending."),
            RecoveryState::SubmissionAmbiguous | RecoveryState::AttemptedUnconfirmed =>
                ("read_only_reconciliation", "Deal serving is closed; inspect chain facts before any recovery operation."),
            RecoveryState::TerminalConfirmed => ("new_purchase", "This deal is terminal; request a new purchase explicitly."),
            RecoveryState::NotAttempted if closure.category == ClosureCategory::PlannedShutdown =>
                ("resume_same_deal", "Deal serving stopped for shutdown; resume the preserved deal after checking its chain state."),
            _ => ("operator_action", "Deal serving is closed; operator action is required before continuing."),
        };
        let mut body = serde_json::json!({"error": {
            "type": "invalid_request_error",
            "code": "deal_session_closed",
            "message": message,
            "request_id": request_id,
            "token_contract": self.token_contract,
            "closure": closure,
            "recovery_state": state,
            "recovery_action": action,
            "terminal_action": terminal_action.map(SessionTerminalAction::event_action),
        }});
        if anthropic {
            body["type"] = serde_json::json!("error");
        }
        Some((http::StatusCode::GONE, axum::Json(body)).into_response())
    }
}
