//! - the interview asks for SHELL and the field holds raw ECC[2] units.

//! `buyer.failover.total_spend_cap_shells` is compared against `escrow * attempt` in raw ECC[2]
//! (`cli::buyer`, `result=total_spend_cap_reached`), the tests that write it write raw, and the
//! published buyer document states it in raw. The interview, though, asks "how much may be spent in
//! total, in SHELL?" and suggests `20`. Written through unconverted, that ceiling is twenty raw
//! units -- two hundredths of a microSHELL -- and the FIRST failover attempt crosses it, so a buyer
//! who answered the interview has no failover at all and is told their own limit stopped them.

//! These pin the conversion at the boundary where an answer becomes a value in the file, which is
//! `super::record_count`. Every one of them is red if that conversion is dropped.

use super::*;
use crate::cli::policy_questions::{Count, BUYER_COUNTS, SELLER_COUNTS};

/// The one count whose answer is money.
fn spend_cap_count() -> &'static Count {
    BUYER_COUNTS
        .iter()
        .find(|count| count.path == "buyer.failover.total_spend_cap_shells")
        .expect("the buyer interview asks for the total spend cap")
}

/// What the interview records for a stated number, read back as the loader reads it.
fn recorded(count: &Count, answer: u64) -> u64 {
    let mut value = serde_json::json!({});
    record_count(&mut value, count, answer).expect("the answer is recorded");
    get_path(&value, count.path)
        .and_then(Value::as_u64)
        .expect("the file holds an integer")
}

/// The prompt says SHELL, so the file gets that many SHELL in the raw ECC[2] units the field holds.

/// Pinned as a literal on both sides: twenty SHELL is twenty billion raw, and the bare `20` that
/// used to be written is a figure this must never produce again.
#[test]
fn a_spend_cap_stated_in_shell_is_recorded_in_the_raw_units_the_field_holds() {
    let count = spend_cap_count();
    assert_eq!(
        recorded(count, 20),
        20_000_000_000,
        "{}: the prompt asks in SHELL ({}), so 20 is twenty SHELL",
        count.path,
        count.unit
    );
    assert_ne!(
        recorded(count, 20),
        20,
        "{}: the number the operator typed is not the number the field holds",
        count.path
    );
    // The conversion is the one every SHELL figure on a command line goes through, so any amount an
    // operator could state lands on the same figure `--escrow` would.
    for stated in [1_u64, 7, 20, 246, 1_000] {
        assert_eq!(
            u128::from(recorded(count, stated)),
            dexdo_core::shell_amount_raw(&stated.to_string()).expect("a whole number of SHELL"),
            "{}: {stated} SHELL",
            count.path
        );
    }
}

/// The defect itself, in the runtime's own arithmetic.

/// `cli::buyer` refuses the next seller when `escrow * next_attempt > total_spend_cap_shells`, both
/// raw. With the suggestion the interview offers, the very first attempt must be affordable -- that
/// is the whole of what the question is for. Unconverted, `20` is crossed by any real escrow there
/// is, which is how "go to the next seller" became dead for everyone who answered the interview.
#[test]
fn the_suggested_cap_survives_the_first_failover_attempt() {
    let count = spend_cap_count();
    // A modest real buy: four ticks at two SHELL each, escrowed the way the book requires
    // (fee-inclusive, `dexdo_core::required_escrow_for_buy` -- the same figure `cli::buyer` puts in
    // `projected_spend`).
    let escrow = dexdo_core::required_escrow_for_buy(4, 2 * dexdo_core::PRICE_STEP);
    let first_attempt_projected_spend = escrow.saturating_mul(1);
    let cap = u128::from(recorded(count, count.suggested));
    assert!(
        first_attempt_projected_spend <= cap,
        "{}: the suggested answer ({} {}) records a cap of {cap} raw, which the first attempt's \
         {first_attempt_projected_spend} raw already crosses",
        count.path,
        count.suggested,
        count.unit
    );
}

/// The whole file the interview writes has to load, and the cap the loader hands the runtime has to
/// be the raw figure -- not the typed one. This drives the production reader rather than the JSON.
#[test]
fn the_policy_written_from_the_interview_loads_with_the_cap_in_raw_units() {
    let mut value = serde_json::json!({ "version": 1 });
    scaffold_roles(&mut value, PolicyRoleArg::Buyer);
    for question in crate::cli::policy_questions::BUYER_QUESTIONS {
        set_path(
            &mut value,
            question.path,
            Value::from(question.suggestion().value),
        );
    }
    for count in BUYER_COUNTS {
        record_count(&mut value, count, count.suggested).expect("the answer is recorded");
    }

    let problems = validate_value(&value, RuntimeRole::Buyer);
    assert!(
        problems.is_empty(),
        "the interview's own answers must produce a file that validates: {problems:?}"
    );
    let policy = buyer_runtime_policy_of(&value).expect("the interview's own answers load");
    assert_eq!(
        policy.total_spend_cap_shells, 20_000_000_000,
        "the runtime is handed twenty SHELL in raw units, not the bare 20 that was typed"
    );
    assert_eq!(
        policy.max_sellers_to_try, 3,
        "a count of sellers is not money and is written exactly as answered"
    );
}

/// A count of things is not converted. `max_sellers_to_try` and `max_open_deals` are counts, and a
/// conversion applied to them would ask for three sellers and record three billion.
#[test]
fn a_count_of_things_is_written_exactly_as_answered() {
    for count in SELLER_COUNTS.iter().chain(BUYER_COUNTS) {
        if count.path == "buyer.failover.total_spend_cap_shells" {
            continue;
        }
        assert_eq!(
            recorded(count, count.suggested),
            count.suggested,
            "{}: {} is a count, not money",
            count.path,
            count.unit
        );
    }
}

/// Exactly the prompts that ask for SHELL are the ones converted.

/// The two live apart on purpose -- the wording belongs to the question and the unit to the field --
/// so this is what keeps them the same fact. A money question added to the interview without a line
/// in `count_is_stated_in_shell` would be recorded a billion times small, silently, and this is the
/// test that says so.
#[test]
fn every_prompt_that_asks_for_shell_is_a_prompt_that_is_converted() {
    let mut money = 0;
    for count in SELLER_COUNTS.iter().chain(BUYER_COUNTS) {
        assert_eq!(
            count.unit.contains("SHELL"),
            count_is_stated_in_shell(count.path),
            "{}: the prompt says \"{}\" and the conversion says {}",
            count.path,
            count.unit,
            count_is_stated_in_shell(count.path)
        );
        money += usize::from(count_is_stated_in_shell(count.path));
    }
    assert_eq!(
        money, 1,
        "the interview asks for exactly one money figure today"
    );
}

/// Rules files written before this change hold raw figures, and nothing here re-reads them.

/// Two halves. The loader is untouched: a file holding `1000000000` still hands the runtime
/// 1 000 000 000. And the interview never revisits an answered field -- it asks only where
/// `field_valid` says the value is missing or unusable, which is the same guard `ask_the_rules`
/// runs before every count -- so a raw figure already on disk is left exactly as it is rather than
/// being read as SHELL and multiplied again.
#[test]
fn a_file_written_before_this_change_keeps_its_raw_figures() {
    let mut value = serde_json::json!({ "version": 1 });
    scaffold_roles(&mut value, PolicyRoleArg::Buyer);
    for question in crate::cli::policy_questions::BUYER_QUESTIONS {
        set_path(
            &mut value,
            question.path,
            Value::from(question.suggestion().value),
        );
    }
    set_path(
        &mut value,
        "buyer.failover.max_sellers_to_try",
        Value::from(3),
    );
    // What an operator following the published buyer document wrote by hand: raw ECC[2].
    set_path(
        &mut value,
        "buyer.failover.total_spend_cap_shells",
        Value::from(1_000_000_000_u64),
    );

    let policy = buyer_runtime_policy_of(&value).expect("a hand-written file still loads");
    assert_eq!(
        policy.total_spend_cap_shells, 1_000_000_000,
        "the reader is untouched: a raw figure on disk stays raw"
    );
    assert!(
        field_valid(
            get_path(&value, "buyer.failover.total_spend_cap_shells"),
            FieldKind::IntegerAtLeast(1)
        ),
        "the interview's own skip-if-answered guard sees this field as answered, so it is never \
         asked for again and never converted a second time"
    );
}

/// A figure that is really a stale raw one is refused by name rather than multiplied again.

/// `24600000000` is what the published buyer document shows for this field, and an operator pasting
/// it into a prompt that asks for SHELL means 24.6 SHELL. Read as SHELL it is more than the largest
/// note that exists holds, which is exactly the refusal `--escrow` and `--budget` already give -- so
/// the interview gives it too, for free, by going through the same parser.
#[test]
fn a_raw_figure_typed_at_a_shell_prompt_is_refused_by_name() {
    let count = spend_cap_count();
    let mut value = serde_json::json!({});
    let error = record_count(&mut value, count, 24_600_000_000)
        .expect_err("a figure no note could carry is not a cap")
        .to_string();
    assert!(error.contains(count.path), "{error}");
    assert!(error.contains("raw ECC[2] units"), "{error}");
    assert!(
        get_path(&value, count.path).is_none(),
        "nothing is written when the figure is refused"
    );
}
