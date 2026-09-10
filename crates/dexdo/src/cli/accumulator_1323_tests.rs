//! the `dexdo accumulator` parse surface and the plan it hands the money path.

//! These drive the REAL command entry - `Cli::try_parse_from` into the same argument structs the
//! handlers receive, and then the same planner the handlers call - rather than asserting on a
//! helper. Before this feature existed every one of them failed at the parse: `accumulator` was not
//! a subcommand.

//! What they are guarding is narrow and specific. The accumulator's sell side accepts ONLY four
//! exact lot sizes, refuses anything else AFTER `tvm.accept()` (so a wrong figure is not a cheap
//! bounce), and offers no cancel and no timeout once a deposit lands. Every refusal below is
//! therefore a refusal to send, not a cosmetic validation.

use super::*;
use dexdo_core::accumulator::{BuyPlan, SellPlan};

fn sell_args(args: &[&str]) -> AccumulatorSellArgs {
    let mut argv = vec!["dexdo", "accumulator", "sell"];
    argv.extend_from_slice(args);
    let cli = Cli::try_parse_from(argv).expect("parse `dexdo accumulator sell`");
    let Command::Accumulator(accumulator) = cli.command else {
        panic!("expected `dexdo accumulator`");
    };
    match accumulator.command {
        AccumulatorCommand::Sell(args) => args,
        _ => panic!("expected `accumulator sell`"),
    }
}

#[test]
fn both_directions_are_reachable_from_the_command_line() {
    // The whole point of: the seller's SHELL->eccUSDC leg and the operator's eccUSDC->SHELL
    // leg are both `dexdo` commands rather than hand work.
    for argv in [
        vec!["dexdo", "accumulator", "sell", "--usdc", "10"],
        vec!["dexdo", "accumulator", "buy", "--usdc", "10"],
        vec!["dexdo", "accumulator", "status"],
        vec!["dexdo", "accumulator", "lots"],
        vec!["dexdo", "accumulator", "claim"],
    ] {
        let cli = Cli::try_parse_from(argv.clone())
            .unwrap_or_else(|e| panic!("{argv:?} must parse: {e}"));
        assert!(
            matches!(cli.command, Command::Accumulator(_)),
            "{argv:?} must reach the accumulator handler"
        );
    }
}

#[test]
fn the_documented_154_example_decomposes_exactly_as_the_contract_docs_say() {
    // "A 154 eccUSDC buy matches 1x100 + 5x10 + 4x1 lots." Ten separate deposits, 15,400 SHELL.
    let args = sell_args(&["--usdc", "154"]);
    let plan = SellPlan::for_whole_usdc(u128::from(args.usdc.expect("--usdc"))).expect("plan");
    assert_eq!(plan.denomination_counts(), vec![(100, 1), (10, 5), (1, 4)]);
    assert_eq!(plan.lot_count(), 10);
    assert_eq!(plan.shell_committed_raw, 15_400_000_000_000);
    assert_eq!(plan.usdc_expected_raw, 154_000_000);
}

#[test]
fn a_sell_must_say_how_much_and_cannot_default_to_converting_everything() {
    // No amount and no `--all` is refused by the parser. A sell that quietly defaulted to the whole
    // balance would be an irreversible deposit nobody asked for: lots cannot be cancelled.
    assert!(Cli::try_parse_from(["dexdo", "accumulator", "sell"]).is_err());
    // And the two ways of saying it are mutually exclusive, so an amount can never be ambiguous.
    assert!(
        Cli::try_parse_from(["dexdo", "accumulator", "sell", "--usdc", "10", "--all"]).is_err()
    );
    assert!(Cli::try_parse_from(["dexdo", "accumulator", "sell", "--all"]).is_ok());
}

#[test]
fn a_sell_below_one_lot_is_refused_rather_than_rounded() {
    // 99.9 SHELL does not reach the 100 SHELL minimum lot. The planner refuses; it does not round
    // up into a lot the wallet cannot fund, and does not round down to a zero-lot no-op that would
    // look like success.
    let refusal = SellPlan::for_available_shell(99_900_000_000)
        .expect_err("below one lot must refuse")
        .to_string();
    assert!(refusal.contains("smallest lot is 1 eccUSDC"), "{refusal}");
    assert!(refusal.contains("100.000000000 SHELL"), "{refusal}");
}

#[test]
fn a_sell_the_wallet_cannot_fund_is_refused_before_anything_is_submitted() {
    let plan = SellPlan::for_whole_usdc(100).expect("plan");
    let refusal = plan
        .require_funded(plan.shell_committed_raw - 1)
        .expect_err("underfunded must refuse")
        .to_string();
    assert!(refusal.contains("nothing was submitted"), "{refusal}");
}

#[test]
fn a_buy_of_zero_is_refused_at_the_planner_the_handler_uses() {
    let refusal = BuyPlan::for_whole_usdc(0)
        .expect_err("zero must refuse")
        .to_string();
    assert!(refusal.contains("whole eccUSDC"), "{refusal}");
}

#[test]
fn naming_a_wallet_requires_naming_its_key() {
    // The same rule the other spending commands follow: an address without a key cannot sign, and
    // the binding must not be silently substituted for a wallet the operator typed by hand.
    assert!(Cli::try_parse_from([
        "dexdo",
        "accumulator",
        "sell",
        "--usdc",
        "10",
        "--multisig-address",
        "0:ab"
    ])
    .is_err());
    assert!(Cli::try_parse_from([
        "dexdo",
        "accumulator",
        "sell",
        "--usdc",
        "10",
        "--multisig-address",
        "0:ab",
        "--multisig-private-key",
        "k.hex"
    ])
    .is_ok());
    // And the two key forms never combine.
    assert!(Cli::try_parse_from([
        "dexdo",
        "accumulator",
        "sell",
        "--usdc",
        "10",
        "--multisig-address",
        "0:ab",
        "--multisig-private-key",
        "k.hex",
        "--multisig-seed-file",
        "s.txt"
    ])
    .is_err());
}

#[test]
fn claiming_one_lot_needs_both_halves_of_its_identity() {
    // A lot is identified by (denomination, order id) and by nothing else, so half of that pair
    // names no lot at all.
    assert!(Cli::try_parse_from(["dexdo", "accumulator", "claim", "--denom", "10"]).is_err());
    assert!(Cli::try_parse_from(["dexdo", "accumulator", "claim", "--order-id", "7"]).is_err());
    assert!(Cli::try_parse_from([
        "dexdo",
        "accumulator",
        "claim",
        "--denom",
        "10",
        "--order-id",
        "7"
    ])
    .is_ok());
}

#[test]
fn the_lot_scan_starts_at_the_first_order_id_by_default() {
    // Recovery reads the chain, and the default must not silently skip older lots: order ids are
    // 1-based and a lot rests until a buyer matches it, which can be long after it was created.
    let cli = Cli::try_parse_from(["dexdo", "accumulator", "lots"]).expect("parse");
    let Command::Accumulator(accumulator) = cli.command else {
        panic!("expected `dexdo accumulator`");
    };
    let AccumulatorCommand::Lots(args) = accumulator.command else {
        panic!("expected `accumulator lots`");
    };
    assert_eq!(args.from_order_id, 1);
}
