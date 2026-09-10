//! Secret-free deal history/export helpers.

use crate::cli::deals::DealHandle;
use crate::cli::deals::{DealHandleRole, DealStateKind, DealStateSummary};
use anyhow::Result;
use serde::Serialize;

pub(crate) const DEAL_AUDIT_VERSION: u32 = 1;

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

pub(crate) struct DealTermsAudit {
    pub(crate) tick_size: u128,
    pub(crate) price_per_tick: u128,
    pub(crate) max_ticks: u128,
}

#[derive(Serialize)]
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

#[derive(Serialize)]
pub(crate) struct AuditSource {
    pub(crate) kind: String,
    pub(crate) handle: Option<String>,
    pub(crate) contracts: String,
}

/// The addresses of one deal, as `dexdo export` reports them in its JSON format.

/// Issue: every address here is **written** canonically. This export is not one of the wire
/// schemas `runtime-machine-contract.md` pins to `0:<account_id>` - it is a current audit payload
/// this client produces - so it carries the same canonical form as `market.json`, the deal handles
/// and human output. The fields stay in the workchain form in memory, exactly like `DealHandle`
/// and `MarketManifest`; only what is emitted changes.
#[derive(Serialize)]
pub(crate) struct AuditDeal {
    pub(crate) role: Option<String>,
    pub(crate) network: Option<String>,
    #[serde(with = "dexdo_core::address::serde_self_dapp")]
    pub(crate) token_contract: String,
    #[serde(with = "dexdo_core::address::serde_canonical_opt")]
    pub(crate) actor_note: Option<String>,
    #[serde(with = "dexdo_core::address::serde_canonical_opt")]
    pub(crate) buyer_note: Option<String>,
    #[serde(with = "dexdo_core::address::serde_canonical_opt")]
    pub(crate) seller_note: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) model_hash: Option<String>,
    #[serde(with = "dexdo_core::address::serde_canonical_opt")]
    pub(crate) order_book: Option<String>,
    #[serde(with = "dexdo_core::address::serde_canonical_opt")]
    pub(crate) root_model: Option<String>,
    pub(crate) created_order_ids: Vec<String>,
    pub(crate) created_at_unix: Option<u64>,
}

#[derive(Serialize)]
pub(crate) struct AuditLifecycle {
    pub(crate) active: bool,
    pub(crate) state: String,
    pub(crate) funded: Option<bool>,
    pub(crate) opened: Option<bool>,
    pub(crate) disputed: Option<bool>,
    pub(crate) probe_accepted: Option<bool>,
    pub(crate) funded_at_unix: Option<u64>,
    pub(crate) probe_time_unix: Option<u64>,
    pub(crate) last_claim_time_unix: Option<u64>,
    pub(crate) dispute_time_unix: Option<u64>,
    pub(crate) stopped_at_unix: Option<u64>,
}

#[derive(Serialize)]
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
    pub(crate) probe_tick: Option<String>,
    pub(crate) buyer_bond: Option<String>,
    pub(crate) buyer_bond_required: Option<String>,
    pub(crate) tokens_final: Option<String>,
    pub(crate) tokens_pending: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct AuditActions {
    pub(crate) observed: Vec<String>,
    pub(crate) available_next_commands: Vec<String>,
    pub(crate) caveats: Vec<String>,
}

#[derive(Serialize)]
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

pub(crate) fn build_deal_audit(input: DealAuditBuild) -> Result<DealAuditExport> {
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
    let accounting = build_accounting(input.summary.as_ref(), input.deal_terms.as_ref())?;
    let actions = build_actions(
        role,
        &deal_ref,
        &input.token_contract,
        input.active,
        input.summary.as_ref(),
        handle.is_some(),
    );

    Ok(DealAuditExport {
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
            probe_time_unix: input
                .summary
                .as_ref()
                .and_then(|s| (s.probe_time != 0).then_some(s.probe_time)),
            last_claim_time_unix: input
                .summary
                .as_ref()
                .and_then(|s| (s.last_claim_time != 0).then_some(s.last_claim_time)),
            dispute_time_unix: input
                .summary
                .as_ref()
                .and_then(|s| (s.dispute_time != 0).then_some(s.dispute_time)),
            stopped_at_unix: None,
        },
        accounting,
        actions,
        requests: AuditRequests {
            served_request_count: None,
            finish_reason: None,
        },
        raw_onchain_state: input.state,
    })
}

fn build_accounting(
    summary: Option<&DealStateSummary>,
    terms: Option<&DealTermsAudit>,
) -> Result<AuditAccounting> {
    let finalized_owed = summary.map(|s| s.finalized_owed);
    let finalized_ticks =
        summary.map(|state| (state.tokens_final / dexdo_core::TICK_SIZE).to_string());
    // The export carries the same by-fact figures the machine answer of `dexdo status` reports,
    // read from the same summary. Money is SHELL there and SHELL here: an export that states them
    // raw would have one field of one deal differ by a billion between two of this client's own
    // answers.
    // Ticks, tokens and the tick size are counts and stay counts.
    let shell = |value: u128| dexdo_core::shell_amount(value);
    Ok(AuditAccounting {
        tick_size: terms.map(|t| t.tick_size.to_string()),
        price_per_tick: terms.map(|t| shell(t.price_per_tick)),
        max_ticks: terms.map(|t| t.max_ticks.to_string()),
        finalized_ticks,
        seller_owed: finalized_owed.map(shell),
        seller_received: None,
        buyer_locked: summary
            .map(DealStateSummary::buyer_locked)
            .transpose()?
            .map(shell),
        buyer_refund: None,
        burned_amount: None,
        deposit: summary.map(|s| shell(s.deposit)),
        probe_tick: summary.map(|s| shell(s.probe_tick)),
        buyer_bond: summary.map(|s| shell(s.buyer_bond)),
        buyer_bond_required: summary.map(|s| shell(s.buyer_bond_required)),
        tokens_final: summary.map(|s| s.tokens_final.to_string()),
        tokens_pending: summary.map(|s| s.tokens_pending.to_string()),
    })
}

fn build_actions(
    role: Option<DealHandleRole>,
    deal_ref: &str,
    token_contract: &str,
    active: bool,
    summary: Option<&DealStateSummary>,
    has_handle: bool,
) -> AuditActions {
    let token_contract = dexdo_core::address::display_self_dapp(token_contract);
    // none of these next actions can be a command line. Every one of them signs, so its
    // handler demands a `--note-key` this export deliberately never sees -- an audit export is
    // secret-free -- and an argv template papering over that with `<buyer-key>` is not argv at
    // all: a shell reads `<buyer-key>` as a redirection and never hands the token to `dexdo`. So
    // each action names its command and states the inputs the operator supplies. With no stored
    // handle the deal reference is a raw TokenContract, which carries neither the role nor the
    // note the close handler requires below clap, so those are stated too. The manifest comes from
    // `DEXDO_MANIFEST`; no follow-up may invent the removed `--contracts` flag.
    let raw_buyer = (!has_handle).then_some("buyer");
    let raw_seller = (!has_handle).then_some("seller");
    let public_deal_ref = if has_handle {
        deal_ref.to_string()
    } else {
        token_contract.clone()
    };
    let close_as_buyer =
        crate::cli::commands::close_guidance(&public_deal_ref, raw_buyer, "buyer", None);
    let close_as_seller =
        crate::cli::commands::close_guidance(&public_deal_ref, raw_seller, "seller", None);
    let settlement = |command: &str, actor: &str, what: &str| {
        format!(
            "{what}: run `dexdo {command}` with --token-contract {}, the {actor} --note-addr and \
             the {actor} --note-key{}",
            crate::cli::support::shell_arg(&token_contract),
            // The manifest is not repeated into a suggested line any more: the flag is gone
            // and `DEXDO_MANIFEST` is already in the shell that got here.
            ""
        )
    };
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
            next.push(format!("explicit buyer STOP: {close_as_buyer}"));
            next.push(settlement(
                "dispute",
                "buyer",
                "if fraud/substitution evidence exists",
            ));
        }
        Some(DealHandleRole::Buyer) if s.funded && !s.probe_accepted => {
            next.push(format!(
                "after MATCH_OPEN_TIMEOUT, {close_as_buyer}; or {}",
                settlement("reclaim", "buyer", "reclaim the escrow")
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
            next.push(settlement(
                "release-dispute",
                "seller",
                "if conceding the dispute",
            ));
        }
        Some(DealHandleRole::Seller) if s.kind == DealStateKind::Stopped => {
            if s.finalized_owed > 0 {
                next.push(settlement(
                    "withdraw-shell",
                    "seller",
                    "to withdraw finalized seller proceeds",
                ));
            }
            next.push(format!(
                "destroy/selfdestruct the stopped TokenContract: {close_as_seller}"
            ));
        }
        Some(DealHandleRole::Seller) if s.opened && !s.probe_accepted => {
            next.push(
                "keep `dexdo seller` running: after the first delivered canonical tick it calls TokenContract.acceptProbe() after PROBE_WINDOW"
                    .into(),
            );
            next.push(format!(
                "to call TokenContract.sellerStop() if the seller must stop, {close_as_seller}"
            ));
        }
        Some(DealHandleRole::Seller) if s.opened => {
            next.push(
                "keep `dexdo seller` running: it calls TokenContract.claimTokens(cumulativeTokens) for delivered output and TokenContract.finalize() for mature claims; the subscription keeper also calls TokenContract.settleWeek() at crossed week boundaries"
                    .into(),
            );
            next.push(format!(
                "to call TokenContract.sellerStop() if the seller must stop, {close_as_seller}"
            ));
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
    line(
        &mut out,
        "token_contract",
        dexdo_core::address::display_self_dapp(&export.deal.token_contract),
    );
    optional_line(
        &mut out,
        "actor_note",
        export
            .deal
            .actor_note
            .as_deref()
            .map(dexdo_core::address::display)
            .as_deref(),
    );
    optional_line(
        &mut out,
        "buyer_note",
        export
            .deal
            .buyer_note
            .as_deref()
            .map(dexdo_core::address::display)
            .as_deref(),
    );
    optional_line(
        &mut out,
        "seller_note",
        export
            .deal
            .seller_note
            .as_deref()
            .map(dexdo_core::address::display)
            .as_deref(),
    );
    optional_line(&mut out, "model", export.deal.model.as_deref());
    optional_line(&mut out, "model_hash", export.deal.model_hash.as_deref());
    optional_line(
        &mut out,
        "order_book",
        export
            .deal
            .order_book
            .as_deref()
            .map(dexdo_core::address::display)
            .as_deref(),
    );
    optional_line(
        &mut out,
        "root_model",
        export
            .deal
            .root_model
            .as_deref()
            .map(dexdo_core::address::display)
            .as_deref(),
    );
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
        "probe_time_unix",
        export
            .lifecycle
            .probe_time_unix
            .map(|v| v.to_string())
            .as_deref(),
    );
    optional_line(
        &mut out,
        "last_claim_time_unix",
        export
            .lifecycle
            .last_claim_time_unix
            .map(|v| v.to_string())
            .as_deref(),
    );
    optional_line(
        &mut out,
        "dispute_time_unix",
        export
            .lifecycle
            .dispute_time_unix
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
    optional_line(
        &mut out,
        "probe_tick",
        export.accounting.probe_tick.as_deref(),
    );
    optional_line(
        &mut out,
        "buyer_bond",
        export.accounting.buyer_bond.as_deref(),
    );
    optional_line(
        &mut out,
        "buyer_bond_required",
        export.accounting.buyer_bond_required.as_deref(),
    );
    optional_line(
        &mut out,
        "tokens_final",
        export.accounting.tokens_final.as_deref(),
    );
    optional_line(
        &mut out,
        "tokens_pending",
        export.accounting.tokens_pending.as_deref(),
    );

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
            handle: make_handle_id("0:33", DealHandleRole::Seller),
            role: DealHandleRole::Seller,
            network: "net-a".into(),
            token_contract: "0:33".into(),
            note_addr: "0:seller".into(),
            frame_model: "qwen/qwen3-32b".into(),
            model_hash: Some(dexdo_core::model_hash_for("qwen/qwen3-32b")),
            order_book: Some("0:book".into()),
            root_model: Some("0:root".into()),
            market: None,
            contracts: "manifest/deployed.manifest.json".into(),
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
        let three_ticks_tokens = 3 * dexdo_core::TICK_SIZE;
        let state = serde_json::json!({
            "funded": true,
            "opened": false,
            "probeAccepted": true,
            "disputed": false,
            "deposit": "1000",
            "probeTick": "0",
            "finalizedOwed": "3000",
            "tokensFinal": three_ticks_tokens.to_string(),
            "tokensPending": three_ticks_tokens.to_string(),
            "probeTime": "110",
            "lastClaimTime": "120",
            "disputeTime": "0",
            "fundedTime": "100",
        });
        let summary = classify_deal_state(
            &state,
            dexdo_core::DealBuyerBond {
                bond_held: 0,
                bond_required: 0,
            },
        )
        .unwrap();
        let export = build_deal_audit(DealAuditBuild {
            generated_at_unix: 200,
            handle: Some(sample_handle()),
            role: Some(DealHandleRole::Seller),
            token_contract: "0:33".into(),
            note_addr: Some("0:seller".into()),
            contracts: "manifest/deployed.manifest.json".into(),
            active: true,
            state: Some(state),
            summary: Some(summary),
            onchain_model: Some("qwen/qwen3-32b".into()),
            onchain_model_hash: Some(dexdo_core::model_hash_for("qwen/qwen3-32b")),
            onchain_buyer_note: Some("0:buyer".into()),
            deal_terms: Some(DealTermsAudit {
                tick_size: dexdo_core::TICK_SIZE,
                price_per_tick: 1000,
                max_ticks: 8,
            }),
        })
        .unwrap();
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

    #[test]
    fn returned_two_price_seller_bond_does_not_invent_finalized_ticks() {
        let state = serde_json::json!({
            "funded": true,
            "opened": false,
            "probeAccepted": false,
            "disputed": false,
            "deposit": "0",
            "probeTick": "0",
            "finalizedOwed": "2000000000",
            "tokensFinal": "0",
            "tokensPending": "0",
            "probeTime": "0",
            "lastClaimTime": "100",
            "disputeTime": "0",
            "fundedTime": "100",
        });
        let summary = classify_deal_state(
            &state,
            dexdo_core::DealBuyerBond {
                bond_held: 0,
                bond_required: 0,
            },
        )
        .unwrap();
        let accounting = build_accounting(
            Some(&summary),
            Some(&DealTermsAudit {
                tick_size: dexdo_core::TICK_SIZE,
                price_per_tick: dexdo_core::PRICE_STEP,
                max_ticks: 2,
            }),
        )
        .unwrap();

        // The export states money in SHELL, like `dexdo status --json` reading the same fields.
        assert_eq!(accounting.seller_owed.as_deref(), Some("2"));
        assert_eq!(accounting.price_per_tick.as_deref(), Some("1"));
        assert_eq!(accounting.tokens_final.as_deref(), Some("0"));
        assert_eq!(accounting.finalized_ticks.as_deref(), Some("0"));
    }

    /// every "next action" this export hands an operator names a command it cannot complete
    /// -- an audit export is secret-free, so it never holds the `--note-key` each of these
    /// handlers demands below clap. The guarantee asserted here is therefore the *name-only* one:
    /// each command span must be exactly a command path, so a later edit that grows an argv
    /// template (`--note-key <buyer-key>`, which a shell reads as a redirection and never delivers)
    /// fails this test rather than reaching an operator.

    /// The inputs the operator has to supply are asserted to be stated in the prose around the
    /// name, including the `--role`/`--note-addr` that a raw TokenContract target does not carry
    /// and the manifest this export was built against.

    /// The count below is a floor on **spans that were actually classified** -- one per backticked
    /// command name found in an emitted action -- not on lines of output, comments or fixtures:
    /// `checked` is only incremented inside the loop that ran the assertion on a real
    /// `build_actions` result. A test fixture, a doc comment or an unrelated string cannot raise
    /// it, because nothing outside that loop touches it.
    #[test]
    fn printed_audit_actions_name_commands_they_cannot_complete() {
        use crate::cli::support::printed_commands::assert_emitted_commands_name_only;
        let summary = DealStateSummary {
            kind: DealStateKind::Probe,
            funded: true,
            opened: true,
            disputed: false,
            probe_accepted: false,
            deposit: 0,
            probe_tick: 0,
            buyer_bond: 0,
            buyer_bond_required: 0,
            finalized_owed: 7,
            tokens_final: 0,
            tokens_pending: 0,
            funded_time: Some(1),
            probe_time: 1,
            last_claim_time: 1,
            dispute_time: 0,
        };
        let mut checked = 0usize;
        let mut signing_checked = 0usize;
        for has_handle in [true, false] {
            let deal_ref = if has_handle {
                "seller-0:33 with space"
            } else {
                "0:33"
            };
            for role in [DealHandleRole::Buyer, DealHandleRole::Seller] {
                for (opened, funded, disputed, kind) in [
                    (true, true, false, DealStateKind::Probe),
                    (true, true, true, DealStateKind::Probe),
                    (false, true, false, DealStateKind::FundedButNeverOpened),
                    (false, false, false, DealStateKind::Stopped),
                ] {
                    let mut summary = summary.clone();
                    summary.opened = opened;
                    summary.funded = funded;
                    summary.disputed = disputed;
                    summary.kind = kind;
                    let actions = build_actions(
                        Some(role),
                        deal_ref,
                        "0:33",
                        true,
                        Some(&summary),
                        has_handle,
                    );
                    for action in actions.available_next_commands {
                        if !action.contains("`dexdo ") {
                            continue;
                        }
                        let context =
                            format!("audit next action (has_handle={has_handle}, {role:?})");
                        // What an action must state depends on the command it names. The ones that
                        // move money sign, so the key has to be named. A raw TokenContract
                        // carries neither role nor note, so a `close` rendered without a stored
                        // handle states those too; a stored handle already carries them. The
                        // read-only lines (`dexdo status`) and the "keep it running" lines
                        // (`dexdo seller`) demand none of that, and requiring it of them would be
                        // asserting something untrue rather than something stronger.
                        const SIGNING: [&str; 5] = [
                            "dexdo close",
                            "dexdo dispute",
                            "dexdo reclaim",
                            "dexdo release-dispute",
                            "dexdo withdraw-shell",
                        ];
                        let mut required: Vec<&str> = Vec::new();
                        if SIGNING.iter().any(|command| action.contains(command)) {
                            required.push("--note-key");
                            // `--contracts` used to be required here. removed the flag: the
                            // manifest arrives in DEXDO_MANIFEST, so a printed command that named
                            // it would name a flag the parser rejects.
                            if !has_handle && action.contains("dexdo close") {
                                required.extend(["--role", "--note-addr"]);
                            }
                            signing_checked += 1;
                        }
                        assert_emitted_commands_name_only(&action, &context, &required);
                        checked += 1;
                    }
                }
            }
        }
        assert!(
            checked >= 8,
            "only {checked} audit command spans reached the name-only assertion; the floor counts \
             classified spans from real build_actions output, so guidance that stopped being \
             emitted cannot be made up for by anything else in this file"
        );
        // The interesting half of this test is the signing actions -- they are the ones that must
        // state a key, a manifest and, for a raw target, a role and note. If none were produced,
        // every assertion above passed on read-only prose and proved nothing.
        assert!(
            signing_checked > 0,
            "no money-moving action was produced, so the inputs this test exists to require were \
             never asserted against anything"
        );
    }

    #[test]
    fn seller_open_actions_name_only_current_contract_methods() {
        let mut summary = DealStateSummary {
            kind: DealStateKind::Probe,
            funded: true,
            opened: true,
            disputed: false,
            probe_accepted: false,
            deposit: 0,
            probe_tick: 0,
            buyer_bond: 0,
            buyer_bond_required: 0,
            finalized_owed: 0,
            tokens_final: 0,
            tokens_pending: 0,
            funded_time: Some(1),
            probe_time: 1,
            last_claim_time: 1,
            dispute_time: 0,
        };
        let probe = build_actions(
            Some(DealHandleRole::Seller),
            "deal-seller",
            "0:tc",
            true,
            Some(&summary),
            true,
        )
        .available_next_commands
        .join("\n");
        assert!(probe.contains("TokenContract.acceptProbe()"), "{probe}");
        assert!(probe.contains("TokenContract.sellerStop()"), "{probe}");
        assert!(!probe.contains("advance"), "{probe}");

        summary.kind = DealStateKind::Streaming;
        summary.probe_accepted = true;
        let streaming = build_actions(
            Some(DealHandleRole::Seller),
            "deal-seller",
            "0:tc",
            true,
            Some(&summary),
            true,
        )
        .available_next_commands
        .join("\n");
        for method in [
            "TokenContract.claimTokens(cumulativeTokens)",
            "TokenContract.finalize()",
            "TokenContract.settleWeek()",
            "TokenContract.sellerStop()",
        ] {
            assert!(streaming.contains(method), "missing {method}: {streaming}");
        }
        assert!(!streaming.contains("advance"), "{streaming}");
    }
}
