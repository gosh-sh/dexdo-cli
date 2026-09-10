//! One snapshot of chain figures through every line a person reads, checked for the unit.

//! `reports.rs::a_figure_read_off_the_chain_is_shown_in_shell` proves the unit for `dexdo status`
//! by hand-written expectations. This proves the *absence* of the other unit everywhere else, and
//! it does so for all the renderers at once: a figure the chain states as `4100000000` must never
//! reach the reader that way, whichever command printed it.

//! The check is one rule, stated once. Split the rendered line on whitespace, take every
//! `name=value` whose value is a bare integer, and refuse any that is a billion or more. A billion
//! is where the two units part company: one SHELL is `1_000_000_000` raw, so a money figure told in
//! raw units is at least that, and a money figure told in SHELL is a small number or carries a
//! decimal point. Counts and clock readings are the exception and are named below, one by one, with
//! the reason each is not money.

use dexdo_core::market::{
    DealChainState, ExecutableQuote, OrderBookOrder, QuoteFill, StreamSnapshot,
};

/// Money as the chain holds it: raw ECC[2]. Read as SHELL each is small; printed unconverted each
/// is nine digits longer, which is exactly what this test hunts for.
const DEPOSIT: u128 = 4_100_000_000; // 4.1 SHELL
const PROBE_TICK: u128 = 3_000_000_000; // 3
const BUYER_BOND: u128 = 6_000_000_000; // 6
const BUYER_LOCKED: u128 = 13_100_000_000; // 13.1
const PRICE_PER_TICK: u128 = 3_000_000_000; // 3 a tick
const COST_WITH_FEE: u128 = 6_150_000_000; // 6.15 for two ticks with the 250 bp fee
const BURNED: u128 = 1_000_000_000; // 1
const SELLER_RECEIVED: u128 = 6_150_000_000; // 6.15
const NOTE_BALANCE: u128 = 250_000_000_000; // 250 -- a note as `dexdo note deploy` leaves it

/// Counts and clock readings. Not money, so a large number in one of them is the truth rather than
/// an unconverted figure.

/// - `deadline`, `deadline_unix`, `timestamp`, `recorded_week_expires_at_unix` are seconds since
/// the epoch, which passed a billion in September 2001.
/// - `tokens_*`, `*_tokens`, `*_current_week` count model tokens: a million to the tick, so a
/// thousand-tick deal reaches a billion of them honestly.
const NOT_MONEY: &[&str] = &[
    "deadline",
    "deadline_unix",
    "timestamp",
    "recorded_week_expires_at_unix",
    "tokens_final",
    "tokens_pending",
    "tokens_paid",
    "tokens_per_week",
    "funded_tokens",
    "week_base_tokens",
    "used_current_week",
    "remaining_current_week",
    "tokens_spent",
    "delivered_tokens",
];

/// Every `name=value` in the line whose value is a bare integer of a billion or more, minus the
/// counts and clock readings named above. A non-empty answer is a figure shown in raw ECC[2].
fn raw_figures(line: &str) -> Vec<String> {
    line.split_whitespace()
        .filter_map(|token| token.split_once('='))
        .filter(|(name, _)| !NOT_MONEY.contains(name))
        .filter(|(_, value)| {
            value
                .parse::<u128>()
                .is_ok_and(|number| number >= dexdo_core::params::PRICE_STEP)
        })
        .map(|(name, value)| format!("{name}={value}"))
        .collect()
}

fn quote_fill() -> QuoteFill {
    QuoteFill {
        order_id: 12,
        token_contract: "0:3333333333333333333333333333333333333333333333333333333333333333"
            .parse()
            .expect("fixture deal contract"),
        ticks: 2,
        price_per_tick: PRICE_PER_TICK,
        cost_with_fee: COST_WITH_FEE,
    }
}

fn resting_ask() -> OrderBookOrder {
    OrderBookOrder {
        order_id: 12,
        owner_note: "0:6666666666666666666666666666666666666666666666666666666666666666"
            .to_string(),
        token_contract: Some(
            "0:3333333333333333333333333333333333333333333333333333333333333333"
                .parse()
                .expect("fixture deal contract"),
        ),
        is_buy: false,
        price_per_tick: PRICE_PER_TICK,
        ticks: 100,
        escrow: BUYER_BOND,
        deadline: 1_782_910_310,
        flags: 0,
        timestamp: 1_782_906_710,
    }
}

fn stream_snapshot() -> StreamSnapshot {
    StreamSnapshot {
        seller_locked: BUYER_BOND,
        buyer_locked: BUYER_LOCKED,
        buyer_lead: PROBE_TICK,
        tokens_final: 2_000_000,
        seller_received: SELLER_RECEIVED,
        buyer_refunded: DEPOSIT,
        burned: BURNED,
        closed: true,
    }
}

/// The deal as `getState()` answers it: raw decimal strings, decoded by the production decoder.
/// Nothing in this test writes a figure the renderer will print -- every one of them comes off a
/// chain-shaped payload the same way it does in a live run.
fn deal_state() -> DealChainState {
    DealChainState::decode_getter(&serde_json::json!({
        "funded": true,
        "opened": true,
        "probeAccepted": true,
        "disputed": false,
        "deposit": DEPOSIT.to_string(),
        "probeTick": PROBE_TICK.to_string(),
        "finalizedOwed": SELLER_RECEIVED.to_string(),
        "tokensFinal": "2000000",
        "tokensPending": "3000000",
        "probeTime": "1787000000",
        "lastClaimTime": "1787000100",
        "disputeTime": "0",
        "fundedTime": "1787000000",
    }))
    .expect("the state getter answers in raw decimal strings")
}

/// Every renderer a person reads, driven off one snapshot, checked by one rule.

/// The renderers are named here rather than reached through their commands on purpose: a command
/// needs a chain, and a test that needs a chain is a test that does not run in the gate. What is
/// under test is the line, and the line is what these functions return.
#[test]
fn no_human_line_shows_a_figure_in_raw_units() {
    let mut lines: Vec<(&str, String)> = Vec::new();

    lines.push((
        "quote fill (dexdo quote)",
        crate::cli::market_views::quote_fill_line(&quote_fill()),
    ));

    lines.push((
        "book row (dexdo orders show)",
        crate::cli::orders::render_order_line(
            &resting_ask(),
            1_782_906_800,
            crate::cli::orders::EscrowRead::Authoritative,
        ),
    ));

    let selection = crate::cli::buyer::BuyerQuoteSelection {
        order_book: "0:1111111111111111111111111111111111111111111111111111111111111111",
        escrow: COST_WITH_FEE,
        quote: ExecutableQuote {
            filled_ticks: 2,
            total_with_fee: COST_WITH_FEE,
            complete: true,
            fills: vec![quote_fill()],
        },
        resting_buy: false,
        quoted_order: Some(resting_ask()),
    };
    lines.push((
        "buyer preflight (dexdo buyer start)",
        crate::cli::buyer::render_buyer_human_preflight(
            "openai--gpt-4.1--mini",
            &selection,
            2,
            PRICE_PER_TICK,
            COST_WITH_FEE,
            NOTE_BALANCE,
        ),
    ));

    let state = deal_state();
    let snapshot = stream_snapshot();
    lines.push((
        "seller policy: the deal ran",
        crate::cli::seller_policy::unsafe_lifecycle_reason(&state),
    ));
    lines.push((
        "seller policy: money still held",
        crate::cli::seller_policy::money_or_locks_reason(&snapshot),
    ));
    lines.push((
        "seller policy: nothing held",
        crate::cli::seller_policy::unopened_no_money_reason(&state, &snapshot),
    ));

    for (role, who) in [
        (crate::cli::deals::DealHandleRole::Buyer, "buyer"),
        (crate::cli::deals::DealHandleRole::Seller, "seller"),
    ] {
        let accounting = crate::cli::dashboard::accounting_for(role, &dashboard_by_fact());
        // The dashboard answers in fields rather than in a line; the same rule reads them once they
        // are named, which is how the browser shows them.
        let named = format!(
            "shell_paid={} shell_locked={} buyer_bond={} buyer_bond_required={} \
             shell_refunded={} shell_burned={} finalized_owed={} ticks_spent={} tokens_spent={} \
             delivered_ticks={} delivered_tokens={}",
            shown(&accounting.shell_paid),
            shown(&accounting.shell_locked),
            shown(&accounting.buyer_bond),
            shown(&accounting.buyer_bond_required),
            shown(&accounting.shell_refunded),
            shown(&accounting.shell_burned),
            shown(&accounting.finalized_owed),
            shown(&accounting.ticks_spent),
            shown(&accounting.tokens_spent),
            shown(&accounting.delivered_ticks),
            shown(&accounting.delivered_tokens),
        );
        lines.push(("dashboard accounting", named));
        assert!(
            !named_is_empty(&accounting),
            "the {who} dashboard reported nothing, so the check above read nothing"
        );
    }

    // The subscription line is built only with the chain types its facts come from, so it joins the
    // others when the acceptance matrix runs the suite -- which is what that matrix
    // runs. Its record and its live facts are built from their own on-disk and getter shapes.
    {
        let record: crate::cli::buyer::BuyerSubscriptionOrderRecord =
            serde_json::from_value(serde_json::json!({
                "frame_model": "openai--gpt-4.1--mini",
                "model_hash": "0x2f0f",
                "order_book": "0:1111111111111111111111111111111111111111111111111111111111111111",
                "order_id": 12u64,
                "max_price_per_tick": PRICE_PER_TICK,
                "ticks": 2u64,
                "deposit": DEPOSIT,
                "buyer_bond": BUYER_BOND,
                "escrow": BUYER_LOCKED,
                "flags": 0u8,
                "deadline": 1_782_910_310u64,
                "fill_cursor": {"since_unix": 1_782_906_710u64, "last_seen_created_at": null,
                                "seen_token_contracts_at_last_seen": []},
                "phase": "resting",
                "matched": null,
            }))
            .expect("the subscription record is read from its own on-disk shape");
        let snapshot: dexdo_core::OrderBookSnapshot = serde_json::from_value(serde_json::json!({
            "frame_model": "openai--gpt-4.1--mini",
            "model_hash": "0x2f0f",
            "order_book": "0:1111111111111111111111111111111111111111111111111111111111111111",
            "stats": null,
            "orders": [],
        }))
        .expect("the book snapshot is read from its own wire shape");
        let facts: crate::cli::buyer::SubscriptionDealFacts =
            serde_json::from_value(serde_json::json!({
                "state": deal_state(),
                "subscription": {"deal_flags": 0u8, "sub_weeks": 4u8, "week_index": 1u8,
                                 "tokens_per_week": 4_000_000u64, "funded_tokens": 16_000_000u64,
                                 "tokens_paid": 4_000_000u64, "period_start": 1_782_906_710u64,
                                 "week_base_tokens": 4_000_000u64},
                "seller_bond": {"bond_funded": true, "bond_held": BUYER_BOND,
                                "bond_required": BUYER_BOND},
                "buyer_bond": {"bond_held": BUYER_BOND, "bond_required": BUYER_BOND},
                "model_name": "openai--gpt-4.1--mini",
                "model_hash": "0x2f0f",
                "buyer_note": "0:6666666666666666666666666666666666666666666666666666666666666666",
            }))
            .expect("the subscription facts are read from their getter shape");
        let quota: crate::cli::buyer::SubscriptionQuotaView =
            serde_json::from_value(serde_json::json!({
                "claimed_current_week": 2_000_000u64,
                "remaining_current_week": 2_000_000u64,
                "buyer_locked_total": BUYER_LOCKED,
            }))
            .expect("the weekly quota is read from its own shape");

        lines.push((
            "subscription line (dexdo buyer subscriptions)",
            crate::cli::buyer::render_subscription_record(
                &snapshot,
                &record,
                "0:6666666666666666666666666666666666666666666666666666666666666666",
                true,
                Some((&facts, &quota)),
            )
            .expect("the subscription line renders"),
        ));
    }

    for (what, line) in &lines {
        let raw = raw_figures(line);
        assert!(
            raw.is_empty(),
            "{what} shows a figure in raw ECC[2] units: {raw:?}\nline: {line}"
        );
    }

    // The control. Without it the rule above would pass on a renderer that prints no figures at
    // all, and every one of these lines does print them.
    let printed: usize = lines
        .iter()
        .map(|(_, line)| line.matches('=').count())
        .sum();
    assert!(
        printed >= 60,
        "the renderers under test printed only {printed} fields; the check has stopped reaching them"
    );
    assert!(
        lines.iter().any(|(_, line)| line.contains("=4.1")),
        "no line showed the deposit as 4.1 SHELL, so nothing proves the figures arrived at all"
    );
}

fn shown(value: &Option<String>) -> String {
    value.clone().unwrap_or_else(|| "-".to_string())
}

fn named_is_empty(accounting: &crate::cli::dashboard::DashboardAccounting) -> bool {
    accounting.shell_locked.is_none() && accounting.shell_burned.is_none()
}

fn dashboard_by_fact() -> crate::cli::dashboard::DashboardByFact {
    crate::cli::dashboard::DashboardByFact {
        seller_locked: Some(BUYER_BOND),
        buyer_locked: Some(BUYER_LOCKED),
        buyer_bond: Some(BUYER_BOND),
        buyer_bond_required: Some(BUYER_BOND),
        tokens_final: Some(2_000_000),
        seller_received: Some(SELLER_RECEIVED),
        buyer_refunded: Some(DEPOSIT),
        burned: Some(BURNED),
        closed: Some(true),
    }
}
