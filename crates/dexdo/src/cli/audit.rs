//! Secret-free deal history/export helpers (#162).
#![cfg_attr(not(feature = "shellnet"), allow(dead_code))]

use crate::cli::deals::{DealHandle, DealHandleRole, DealStateKind, DealStateSummary};
use serde::Serialize;

pub(crate) const DEAL_AUDIT_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub(crate) struct DealAuditBuild {
    pub(crate) generated_at_unix: u64,
    pub(crate) handle: Option<DealHandle>,
    pub(crate) role: Option<DealHandleRole>,
    pub(crate) token_contract: String,
    pub(crate) note_addr: Option<String>,
    pub(crate) contracts: String,
    pub(crate) active: bool,
    pub(crate) state: Option<serde_json::Value>,
    pub(crate) summary: Option<DealStateSummary>,
    pub(crate) onchain_model: Option<String>,
    pub(crate) onchain_model_hash: Option<String>,
    pub(crate) onchain_buyer_note: Option<String>,
    pub(crate) deal_terms: Option<DealTermsAudit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DealTermsAudit {
    pub(crate) tick_size: u128,
    pub(crate) price_per_tick: u128,
    pub(crate) max_ticks: u128,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct DealAuditExport {
    pub(crate) version: u32,
    pub(crate) generated_at_unix: u64,
    pub(crate) source: AuditSource,
    pub(crate) deal: AuditDeal,
    pub(crate) lifecycle: AuditLifecycle,
    pub(crate) accounting: AuditAccounting,
    pub(crate) actions: AuditActions,
    pub(crate) requests: AuditRequests,
    pub(crate) raw_onchain_state: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuditSource {
    pub(crate) kind: String,
    pub(crate) handle: Option<String>,
    pub(crate) contracts: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuditDeal {
    pub(crate) role: Option<String>,
    pub(crate) network: Option<String>,
    pub(crate) token_contract: String,
    pub(crate) actor_note: Option<String>,
    pub(crate) buyer_note: Option<String>,
    pub(crate) seller_note: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) model_hash: Option<String>,
    pub(crate) order_book: Option<String>,
    pub(crate) root_model: Option<String>,
    pub(crate) created_order_ids: Vec<String>,
    pub(crate) created_at_unix: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuditLifecycle {
    pub(crate) active: bool,
    pub(crate) state: String,
    pub(crate) funded: Option<bool>,
    pub(crate) opened: Option<bool>,
    pub(crate) disputed: Option<bool>,
    pub(crate) probe_accepted: Option<bool>,
    pub(crate) funded_at_unix: Option<u64>,
    pub(crate) last_advance_unix: Option<u64>,
    pub(crate) stopped_at_unix: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuditAccounting {
    pub(crate) tick_size: Option<String>,
    pub(crate) price_per_tick: Option<String>,
    pub(crate) max_ticks: Option<String>,
    pub(crate) finalized_ticks: Option<String>,
    pub(crate) seller_owed: Option<String>,
    pub(crate) seller_received: Option<String>,
    pub(crate) buyer_locked: Option<String>,
    pub(crate) buyer_refund: Option<String>,
    pub(crate) burned_amount: Option<String>,
    pub(crate) deposit: Option<String>,
    pub(crate) prepaid: Option<String>,
    pub(crate) frozen: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuditActions {
    pub(crate) observed: Vec<String>,
    pub(crate) available_next_commands: Vec<String>,
    pub(crate) caveats: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AuditRequests {
    pub(crate) served_request_count: Option<u64>,
    pub(crate) finish_reason: Option<String>,
}

pub(crate) fn history_handle_matches(
    h: &DealHandle,
    note: Option<&str>,
    model: Option<&str>,
) -> bool {
    if let Some(note) = note {
        if crate::cli::deals::normalize_addr(&h.note_addr)
            != crate::cli::deals::normalize_addr(note)
        {
            return false;
        }
    }
    if let Some(model) = model {
        let want = model.trim();
        if h.frame_model != want && h.model_hash.as_deref() != Some(want) {
            return false;
        }
    }
    true
}

pub(crate) fn build_deal_audit(input: DealAuditBuild) -> DealAuditExport {
    let handle = input.handle.as_ref();
    let role = input.role.or_else(|| handle.map(|h| h.role));
    let deal_ref = handle
        .map(|h| h.handle.clone())
        .unwrap_or_else(|| input.token_contract.clone());
    let actor_note = input
        .note_addr
        .clone()
        .or_else(|| handle.map(|h| h.note_addr.clone()));
    let buyer_note = input.onchain_buyer_note.clone().or_else(|| {
        (role == Some(DealHandleRole::Buyer))
            .then(|| actor_note.clone())
            .flatten()
    });
    let seller_note = handle
        .and_then(|h| h.market.as_ref().map(|m| m.seller_note.clone()))
        .or_else(|| {
            (role == Some(DealHandleRole::Seller))
                .then(|| actor_note.clone())
                .flatten()
        });
    let model = input
        .onchain_model
        .clone()
        .or_else(|| handle.map(|h| h.frame_model.clone()));
    let model_hash = input
        .onchain_model_hash
        .clone()
        .or_else(|| handle.and_then(|h| h.model_hash.clone()));
    let order_book = handle.and_then(|h| h.order_book.clone());
    let root_model = handle.and_then(|h| h.root_model.clone());
    let created_order_ids = handle
        .map(|h| {
            h.created_order_ids
                .iter()
                .map(|id| id.to_string())
                .collect()
        })
        .unwrap_or_default();
    let created_at_unix = handle.map(|h| h.created_at_unix);
    let network = handle.map(|h| h.network.clone());
    let source_kind = if handle.is_some() {
        "local_handle_plus_onchain".to_string()
    } else {
        "raw_token_contract_onchain".to_string()
    };
    let state_name = input
        .summary
        .as_ref()
        .map(|s| s.kind.as_str().to_string())
        .unwrap_or_else(|| {
            if input.active {
                "unknown".to_string()
            } else {
                "closed".to_string()
            }
        });
    let accounting = build_accounting(input.summary.as_ref(), input.deal_terms.as_ref());
    let actions = build_actions(
        role,
        &deal_ref,
        &input.token_contract,
        input.active,
        input.summary.as_ref(),
        handle.is_some(),
    );

    DealAuditExport {
        version: DEAL_AUDIT_VERSION,
        generated_at_unix: input.generated_at_unix,
        source: AuditSource {
            kind: source_kind,
            handle: handle.map(|h| h.handle.clone()),
            contracts: input.contracts,
        },
        deal: AuditDeal {
            role: role.map(|r| r.as_str().to_string()),
            network,
            token_contract: input.token_contract,
            actor_note,
            buyer_note,
            seller_note,
            model,
            model_hash,
            order_book,
            root_model,
            created_order_ids,
            created_at_unix,
        },
        lifecycle: AuditLifecycle {
            active: input.active,
            state: state_name,
            funded: input.summary.as_ref().map(|s| s.funded),
            opened: input.summary.as_ref().map(|s| s.opened),
            disputed: input.summary.as_ref().map(|s| s.disputed),
            probe_accepted: input.summary.as_ref().map(|s| s.probe_accepted),
            funded_at_unix: input.summary.as_ref().and_then(|s| s.funded_time),
            last_advance_unix: input
                .summary
                .as_ref()
                .and_then(|s| (s.last_advance != 0).then_some(s.last_advance)),
            stopped_at_unix: None,
        },
        accounting,
        actions,
        requests: AuditRequests {
            served_request_count: None,
            finish_reason: None,
        },
        raw_onchain_state: input.state,
    }
}

fn build_accounting(
    summary: Option<&DealStateSummary>,
    terms: Option<&DealTermsAudit>,
) -> AuditAccounting {
    let finalized_owed = summary.map(|s| s.finalized_owed);
    let finalized_ticks = finalized_owed.and_then(|owed| {
        let price = terms?.price_per_tick;
        (price != 0).then(|| (owed / price).to_string())
    });
    AuditAccounting {
        tick_size: terms.map(|t| t.tick_size.to_string()),
        price_per_tick: terms.map(|t| t.price_per_tick.to_string()),
        max_ticks: terms.map(|t| t.max_ticks.to_string()),
        finalized_ticks,
        seller_owed: finalized_owed.map(|v| v.to_string()),
        seller_received: None,
        buyer_locked: summary.map(|s| s.buyer_locked().to_string()),
        buyer_refund: None,
        burned_amount: None,
        deposit: summary.map(|s| s.deposit.to_string()),
        prepaid: summary.map(|s| s.prepaid.to_string()),
        frozen: summary.map(|s| s.frozen.to_string()),
    }
}

fn build_actions(
    role: Option<DealHandleRole>,
    deal_ref: &str,
    token_contract: &str,
    active: bool,
    summary: Option<&DealStateSummary>,
    has_handle: bool,
) -> AuditActions {
    let mut observed = Vec::new();
    let mut next = Vec::new();
    let mut caveats = vec![
        "seller_received, buyer_refund, burned_amount, served_request_count, finish_reason, and stopped_at_unix are null unless a durable local request/action log exists; this export does not invent them".to_string(),
        "amounts are emitted as decimal strings to preserve uint128 precision".to_string(),
    ];
    if !has_handle {
        caveats.push(
            "raw TokenContract export has no local handle context; only on-chain fields are authoritative"
                .to_string(),
        );
    }
    if !active {
        observed.push("token_contract_inactive_or_closed".to_string());
        return AuditActions {
            observed,
            available_next_commands: next,
            caveats,
        };
    }
    let Some(s) = summary else {
        observed.push("token_contract_active_state_unclassified".to_string());
        return AuditActions {
            observed,
            available_next_commands: next,
            caveats,
        };
    };
    observed.push(format!("state={}", s.kind.as_str()));
    if s.disputed {
        observed.push("dispute_open".to_string());
    }
    if s.kind == DealStateKind::Stopped {
        observed.push("buyer_stop_or_recover_observed".to_string());
    }
    if s.kind == DealStateKind::FundedButNeverOpened {
        observed.push("funded_never_opened".to_string());
    }

    match role {
        Some(DealHandleRole::Buyer) if s.disputed => {
            next.push(
                "wait for seller/arbitration dispute resolution; inspect with `dexdo status`"
                    .into(),
            );
        }
        Some(DealHandleRole::Buyer) if s.opened => {
            next.push(format!(
                "`dexdo close {deal_ref} --note-key <buyer-key>` (streamStop/recover, or streamReclaim after timeout)"
            ));
            next.push(format!(
                "`dexdo dispute --token-contract {token_contract} --note-addr <buyer-note> --note-key <buyer-key>` if fraud/substitution evidence exists"
            ));
        }
        Some(DealHandleRole::Buyer) if s.funded && !s.probe_accepted => {
            next.push(format!(
                "`dexdo close {deal_ref} --note-key <buyer-key>` or `dexdo reclaim --token-contract {token_contract} --note-addr <buyer-note> --note-key <buyer-key>` after MATCH_OPEN_TIMEOUT"
            ));
        }
        Some(DealHandleRole::Buyer) if s.kind == DealStateKind::Stopped => {
            next.push(
                "buyer side already stopped; seller can destroy/withdraw from the seller handle"
                    .into(),
            );
        }
        Some(DealHandleRole::Buyer) => {
            next.push("no buyer close action yet; inspect order state or wait for match".into());
        }
        Some(DealHandleRole::Seller) if s.disputed => {
            next.push(format!(
                "`dexdo release-dispute --token-contract {token_contract} --note-addr <seller-note> --note-key <seller-key>` if conceding the dispute"
            ));
        }
        Some(DealHandleRole::Seller) if s.kind == DealStateKind::Stopped => {
            if s.finalized_owed > 0 {
                next.push(format!(
                    "`dexdo withdraw-shell --token-contract {token_contract} --note-addr <seller-note> --note-key <seller-key>` to withdraw finalized seller proceeds"
                ));
            }
            next.push(format!(
                "`dexdo close {deal_ref} --note-key <seller-key>` (destroy/selfdestruct stopped TokenContract)"
            ));
        }
        Some(DealHandleRole::Seller) if s.opened => {
            next.push(
                "wait for buyer STOP/recover/reclaim; seller cannot destroy an opened deal".into(),
            );
        }
        Some(DealHandleRole::Seller) => {
            next.push(
                "seller has no destroy action until the deal is stopped and undisputed".into(),
            );
        }
        None => {
            next.push("pass a local handle for role-aware next actions; raw TokenContract role is unknown".into());
        }
    }

    AuditActions {
        observed,
        available_next_commands: next,
        caveats,
    }
}

pub(crate) fn render_markdown(export: &DealAuditExport) -> String {
    let mut out = String::new();
    out.push_str("# dexdo deal audit\n\n");
    line(&mut out, "generated_at_unix", export.generated_at_unix);
    line(&mut out, "source", &export.source.kind);
    if let Some(handle) = &export.source.handle {
        line(&mut out, "handle", handle);
    }
    line(&mut out, "contracts", &export.source.contracts);

    out.push_str("\n## Deal\n\n");
    optional_line(&mut out, "role", export.deal.role.as_deref());
    optional_line(&mut out, "network", export.deal.network.as_deref());
    line(&mut out, "token_contract", &export.deal.token_contract);
    optional_line(&mut out, "actor_note", export.deal.actor_note.as_deref());
    optional_line(&mut out, "buyer_note", export.deal.buyer_note.as_deref());
    optional_line(&mut out, "seller_note", export.deal.seller_note.as_deref());
    optional_line(&mut out, "model", export.deal.model.as_deref());
    optional_line(&mut out, "model_hash", export.deal.model_hash.as_deref());
    optional_line(&mut out, "order_book", export.deal.order_book.as_deref());
    optional_line(&mut out, "root_model", export.deal.root_model.as_deref());
    if !export.deal.created_order_ids.is_empty() {
        line(
            &mut out,
            "created_order_ids",
            export.deal.created_order_ids.join(","),
        );
    }
    optional_line(
        &mut out,
        "created_at_unix",
        export
            .deal
            .created_at_unix
            .map(|v| v.to_string())
            .as_deref(),
    );

    out.push_str("\n## Lifecycle\n\n");
    line(&mut out, "active", export.lifecycle.active);
    line(&mut out, "state", &export.lifecycle.state);
    optional_line(
        &mut out,
        "funded",
        export.lifecycle.funded.map(|v| v.to_string()).as_deref(),
    );
    optional_line(
        &mut out,
        "opened",
        export.lifecycle.opened.map(|v| v.to_string()).as_deref(),
    );
    optional_line(
        &mut out,
        "disputed",
        export.lifecycle.disputed.map(|v| v.to_string()).as_deref(),
    );
    optional_line(
        &mut out,
        "probe_accepted",
        export
            .lifecycle
            .probe_accepted
            .map(|v| v.to_string())
            .as_deref(),
    );
    optional_line(
        &mut out,
        "funded_at_unix",
        export
            .lifecycle
            .funded_at_unix
            .map(|v| v.to_string())
            .as_deref(),
    );
    optional_line(
        &mut out,
        "last_advance_unix",
        export
            .lifecycle
            .last_advance_unix
            .map(|v| v.to_string())
            .as_deref(),
    );
    optional_line(
        &mut out,
        "stopped_at_unix",
        export
            .lifecycle
            .stopped_at_unix
            .map(|v| v.to_string())
            .as_deref(),
    );

    out.push_str("\n## Accounting\n\n");
    optional_line(
        &mut out,
        "tick_size",
        export.accounting.tick_size.as_deref(),
    );
    optional_line(
        &mut out,
        "price_per_tick",
        export.accounting.price_per_tick.as_deref(),
    );
    optional_line(
        &mut out,
        "max_ticks",
        export.accounting.max_ticks.as_deref(),
    );
    optional_line(
        &mut out,
        "finalized_ticks",
        export.accounting.finalized_ticks.as_deref(),
    );
    optional_line(
        &mut out,
        "seller_owed",
        export.accounting.seller_owed.as_deref(),
    );
    optional_line(
        &mut out,
        "seller_received",
        export.accounting.seller_received.as_deref(),
    );
    optional_line(
        &mut out,
        "buyer_locked",
        export.accounting.buyer_locked.as_deref(),
    );
    optional_line(
        &mut out,
        "buyer_refund",
        export.accounting.buyer_refund.as_deref(),
    );
    optional_line(
        &mut out,
        "burned_amount",
        export.accounting.burned_amount.as_deref(),
    );
    optional_line(&mut out, "deposit", export.accounting.deposit.as_deref());
    optional_line(&mut out, "prepaid", export.accounting.prepaid.as_deref());
    optional_line(&mut out, "frozen", export.accounting.frozen.as_deref());

    out.push_str("\n## Actions\n\n");
    for item in &export.actions.observed {
        out.push_str(&format!("- observed: {item}\n"));
    }
    for item in &export.actions.available_next_commands {
        out.push_str(&format!("- next: {item}\n"));
    }
    for item in &export.actions.caveats {
        out.push_str(&format!("- caveat: {item}\n"));
    }

    out.push_str("\n## Requests\n\n");
    optional_line(
        &mut out,
        "served_request_count",
        export
            .requests
            .served_request_count
            .map(|v| v.to_string())
            .as_deref(),
    );
    optional_line(
        &mut out,
        "finish_reason",
        export.requests.finish_reason.as_deref(),
    );
    out
}

fn line(out: &mut String, key: &str, value: impl std::fmt::Display) {
    out.push_str(&format!("- {key}: {value}\n"));
}

fn optional_line(out: &mut String, key: &str, value: Option<&str>) {
    match value {
        Some(v) => line(out, key, v),
        None => line(out, key, "null"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::deals::{
        classify_deal_state, make_handle_id, DealEndpointInfo, DealHandle, DEAL_HANDLE_VERSION,
    };

    fn sample_handle() -> DealHandle {
        DealHandle {
            version: DEAL_HANDLE_VERSION,
            handle: make_handle_id("0:33"),
            role: DealHandleRole::Seller,
            network: "shellnet".into(),
            token_contract: "0:33".into(),
            note_addr: "0:seller".into(),
            frame_model: "qwen/qwen3-32b".into(),
            model_hash: Some(dexdo_core::model_hash_for("qwen/qwen3-32b")),
            order_book: Some("0:book".into()),
            root_model: Some("0:root".into()),
            market: None,
            contracts: "contracts/deployed.shellnet.json".into(),
            endpoint: Some(DealEndpointInfo {
                kind: "gateway".into(),
                value: "127.0.0.1:8443".into(),
            }),
            created_order_ids: vec![7, 8],
            created_at_unix: 10,
        }
    }

    #[test]
    fn history_filter_matches_note_and_model_or_hash() {
        let h = sample_handle();
        assert!(history_handle_matches(
            &h,
            Some("0:seller"),
            Some("qwen/qwen3-32b")
        ));
        assert!(history_handle_matches(
            &h,
            Some("0:SELLER"),
            h.model_hash.as_deref()
        ));
        assert!(!history_handle_matches(&h, Some("0:other"), None));
        assert!(!history_handle_matches(&h, None, Some("other/model")));
    }

    #[test]
    fn deal_audit_json_and_markdown_are_secret_free_and_compute_ticks() {
        let state = serde_json::json!({
            "funded": true,
            "opened": false,
            "probeAccepted": true,
            "disputed": false,
            "deposit": "1000",
            "prepaid": "0",
            "frozen": "0",
            "finalizedOwed": "3000",
            "fundedTime": "100",
            "lastAdvance": "120"
        });
        let summary = classify_deal_state(&state);
        let export = build_deal_audit(DealAuditBuild {
            generated_at_unix: 200,
            handle: Some(sample_handle()),
            role: Some(DealHandleRole::Seller),
            token_contract: "0:33".into(),
            note_addr: Some("0:seller".into()),
            contracts: "contracts/deployed.shellnet.json".into(),
            active: true,
            state: Some(state),
            summary: Some(summary),
            onchain_model: Some("qwen/qwen3-32b".into()),
            onchain_model_hash: Some(dexdo_core::model_hash_for("qwen/qwen3-32b")),
            onchain_buyer_note: Some("0:buyer".into()),
            deal_terms: Some(DealTermsAudit {
                tick_size: 1_000_000,
                price_per_tick: 1000,
                max_ticks: 8,
            }),
        });
        assert_eq!(export.accounting.finalized_ticks.as_deref(), Some("3"));
        assert_eq!(export.deal.buyer_note.as_deref(), Some("0:buyer"));
        let json = serde_json::to_string_pretty(&export).unwrap();
        let md = render_markdown(&export);
        assert!(json.contains("stopped_at_unix"), "{json}");
        assert!(md.contains("stopped_at_unix"), "{md}");
        for text in [&json, &md] {
            assert!(!text.contains("note_key"), "{text}");
            assert!(!text.to_ascii_lowercase().contains("secret"), "{text}");
            assert!(text.contains("qwen/qwen3-32b"), "{text}");
            assert!(text.contains("finalized"), "{text}");
        }
    }
}
