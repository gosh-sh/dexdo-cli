//! the client states a funding wallet's gas floor BEFORE it commits a spend.

//! The wallet this came from is on mainnet and is stuck. It holds `8_750_000_000_000` raw ECC[2]
//! SHELL -- 8 750 SHELL, a rich wallet -- against `14_022_000` raw native, `0.014022 vmshell`. It
//! cannot mint, cannot send, and cannot convert its own held SHELL into gas, because that
//! conversion is itself a message it has to pay for. Nothing this client ships reaches a wallet in
//! that state; the cure is an ordinary external transfer that anyone holding SHELL can make.

//! So the defect is not that the wallet was allowed to spend. It is that the client knew what its
//! own messages cost, knew what the wallet held, and said nothing about the gap until the spend had
//! already failed on chain. The fix is that the two commands which spend a funding wallet state the
//! floor with its numbers before they commit anything, and refuse a spend the wallet demonstrably
//! cannot pay for -- rather than deciding on the operator's behalf how much of his own money he
//! ought to keep back.

//! What is pinned here: the floor is the one derivation rather than two; the statement names every
//! figure needed to act on it; and both spending commands make it. The two command bodies open a
//! chain client before they reach any of this, so the wiring half is pinned by shape the way
//! `note_funding_wiring_334` pins its own -- correct arithmetic that no command calls is exactly
//! the failure this file exists to prevent.

use dexdo_core::params::{
    funding_wallet_native_floor_notice, funding_wallet_native_shortfall_raw,
    FUNDING_WALLET_NATIVE_FLOOR_RAW, NOTE_DEPLOY_SUBMIT_NATIVE_VALUE, NOTE_DEPLOY_WALLET_SUBMITS,
    WALLET_SUBMIT_NATIVE_FEE_BOUND_RAW,
};

/// The mainnet operator multisig, exactly as it was read for and.
const STUCK_NATIVE_RAW: u128 = 14_022_000;
const STUCK_SHELL_RAW: u128 = 8_750_000_000_000;

/// The floor is what this wallet's own outgoing messages cost, from constants and receipts.

/// Pinned as a number as well as an expression. The expression alone would follow any later edit to
/// its summands without complaint, and the number is the one measured against the live
/// wallet -- `507_002_000` raw, `0.507002 vmshell`.
#[test]
fn the_floor_is_two_submits_of_attached_value_plus_fee() {
    assert_eq!(
        FUNDING_WALLET_NATIVE_FLOOR_RAW,
        NOTE_DEPLOY_WALLET_SUBMITS
            * (NOTE_DEPLOY_SUBMIT_NATIVE_VALUE + WALLET_SUBMIT_NATIVE_FEE_BOUND_RAW)
    );
    assert_eq!(FUNDING_WALLET_NATIVE_FLOOR_RAW, 507_002_000);
}

/// One derivation, not two.

/// `vault_to_hot_native_value` is the funding gate's floor and was where this product used to be
/// spelled out. It now reads the constant. Two copies of the same arithmetic in two modules is how
/// the floor a gate waits for and the floor a command reports come to disagree, and an operator
/// told two different numbers for the same thing believes neither.
#[test]
fn the_funding_gate_and_the_spending_commands_share_one_floor() {
    assert_eq!(
        crate::cli::wallet_funding::vault_to_hot_native_value(),
        FUNDING_WALLET_NATIVE_FLOOR_RAW
    );
    let source = include_str!("wallet_funding.rs");
    let start = source
        .find("pub(crate) fn vault_to_hot_native_value()")
        .expect("vault_to_hot_native_value present");
    let body = &source[start..];
    let body = &body[..body.find("\n}\n").unwrap_or(body.len())];
    assert!(
        !body.contains('*'),
        "the gate must read the floor, not multiply its own copy of it: {body}"
    );
}

/// The stuck wallet is short, and the shortfall is the figure measured.
#[test]
fn the_stuck_mainnet_wallet_is_short_by_a_named_amount() {
    assert_eq!(
        funding_wallet_native_shortfall_raw(STUCK_NATIVE_RAW),
        Some(492_980_000),
        "507_002_000 - 14_022_000, the figure  read off the chain and never printed"
    );
    // At and above the floor there is nothing to say, and saying nothing is the whole of the
    // healthy path: this is a statement about gas, not a tax on a working wallet.
    assert_eq!(
        funding_wallet_native_shortfall_raw(FUNDING_WALLET_NATIVE_FLOOR_RAW),
        None
    );
    assert_eq!(
        funding_wallet_native_shortfall_raw(FUNDING_WALLET_NATIVE_FLOOR_RAW + 1),
        None
    );
    assert_eq!(
        funding_wallet_native_shortfall_raw(FUNDING_WALLET_NATIVE_FLOOR_RAW - 1),
        Some(1),
        "one raw unit below the floor is short one raw unit -- the boundary is not rounded away"
    );
    // A wallet at zero is short the whole floor rather than panicking on a money path.
    assert_eq!(
        funding_wallet_native_shortfall_raw(0),
        Some(FUNDING_WALLET_NATIVE_FLOOR_RAW)
    );
}

/// The statement names every number an operator needs, and never renders one of them as nothing.

/// is the lesson: it shipped a funding refusal that rendered one of its two shortfall figures
/// and not the other, so a mainnet operator was told he was "short nothing" and then left blocked
/// on a deposit whose size the client never named. Asserted against the real wallet, by value.
#[test]
fn the_statement_names_the_floor_the_shortfall_and_the_shell_that_cannot_pay_for_it() {
    let notice = funding_wallet_native_floor_notice(STUCK_NATIVE_RAW, STUCK_SHELL_RAW)
        .expect("a wallet under the floor must produce a statement");
    for expected in [
        "holds 14022000 raw native vmshell", // what it has
        "floor of 507002000 raw", // what it needs
        "2 submits x (100000000 attached + 153501000 fee)", // where the floor comes from
        "short 492980000 raw native", // the gap, as a number
        "8750000000000 raw ECC[2] SHELL cannot pay for this", // why being rich does not help
        "Send at least 492980000 raw native vmshell", // what to do about it
        "flag-16 form", // and how, in the form that arrives as gas
    ] {
        assert!(
            notice.contains(expected),
            "the statement must contain `{expected}`: {notice}"
        );
    }
    assert!(
        !notice.contains("short nothing"),
        "the  shape must not come back: {notice}"
    );
    // Above the floor there is nothing to state, so no caller can print an empty complaint.
    assert_eq!(
        funding_wallet_native_floor_notice(FUNDING_WALLET_NATIVE_FLOOR_RAW, STUCK_SHELL_RAW),
        None
    );
}

/// `note topup` reads BOTH balances off its one account read, and states the floor before it spends.

/// It used to read one. The ECC[2] check answered "is there SHELL to send" and nothing answered
/// "can this wallet pay to send it", so a wallet rich in SHELL and out of gas passed the preflight
/// and failed on chain -- in the one state this client cannot get it out of again.
#[test]
fn note_topup_states_the_floor_before_it_commits() {
    let production = production_note_cmd();
    let start = production
        .find("async fn note_topup_preflight_wallet_ecc")
        .expect("note topup keeps its preflight");
    let body = &production[start..];
    let body = &body[..body.find("\n}\n").unwrap_or(body.len())];

    assert!(
        body.contains("funding_wallet_native_floor_notice("),
        "the preflight must state the gas floor through the shared statement: {body}"
    );
    assert!(
        body.contains("acc.balance"),
        "the preflight must read the NATIVE balance, not only the ECC[2] one: {body}"
    );
    let ecc = body
        .find("ecc_balance(SHELL_CURRENCY_ID)")
        .expect("the preflight still checks the SHELL it is about to send");
    let floor = body
        .find("funding_wallet_native_floor_notice(")
        .expect("floor statement present");
    assert!(
        ecc < floor,
        "both balances come off the same read; the floor is stated after the amount is known: {body}"
    );
    assert!(
        body.contains("No wallet POST was submitted"),
        "a refusal on this path must say that nothing was spent: {body}"
    );
}

/// `accumulator sell` states the floor before the first irreversible deposit.

/// A lot cannot be cancelled or withdrawn, and `sell` is the command that converts a wallet's
/// spendable SHELL away, so it is the one place where a wallet can walk itself into holding money
/// it cannot move. The statement goes after the plan is known and before anything is persisted or
/// sent.
#[test]
fn accumulator_sell_states_the_floor_before_the_first_lot() {
    let source = include_str!("accumulator.rs");
    let start = source
        .find("async fn run_sell(args: AccumulatorSellArgs)")
        .expect("run_sell present");
    let end = source[start..]
        .find("async fn run_buy(")
        .map(|offset| start + offset)
        .expect("run_sell end marker present");
    let body = &source[start..end];

    let floor = body
        .find("funding_wallet_native_floor_notice(")
        .expect("run_sell must state the gas floor");
    let persist = body
        .find("persist_pending(")
        .expect("run_sell records its plan before sending");
    let send = body.find("send_ecc(").expect("run_sell sends its lots");
    assert!(
        floor < persist && persist < send,
        "the floor must be stated before anything irreversible is written or sent: {body}"
    );
    assert!(
        body.contains("No lot was submitted"),
        "a refusal on this path must say that no deposit was made: {body}"
    );
    assert!(
        body.contains("not a limit on what you may convert"),
        "the statement must not read as the client deciding how much the operator may spend: {body}"
    );
}

/// The production half of `note_cmd.rs`, with its unit-test module cut off.

/// The same seam `note_funding_wiring_334` uses, for the same reason: a marker matched inside the
/// test module would let a fixture stand in for the production call site.
fn production_note_cmd() -> &'static str {
    include_str!("note_cmd.rs")
        .split_once("#[cfg(test)]\nmod tests")
        .expect("note_cmd unit-test module boundary")
        .0
}
