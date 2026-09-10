//! the DECLARED route of `oracle withdraw-fees`, driven through the boundaries production
//! drives it through.

//! The route promises that a destination the operator already declared with `dexdo wallet onboard`
//! is admitted without a custodian proof. It compared SPELLINGS, so for a binding written by
//! `wallet onboard manual` from a legacy `0:<hex>` multisig address -- the documented spelling --
//! it could not match, ever.

//! What the three tests in `oracle_withdraw_destination_1465_tests.rs` could not see is why this
//! file exists: they hand BOTH sides of the comparison the same hand-written literal, so the
//! spelling gap between the two production paths never appears. Here neither side is typed. `--to`
//! goes through the flag's real `value_parser` and then through the same rendering the preflight
//! uses, and the declared address is built by the same call `wallet onboard manual` makes. Every
//! test below asserts, before it asserts anything else, that the two sides really are spelled
//! differently -- otherwise it would be green for the wrong reason.

use super::{
    admit_oracle_withdraw_destination, OracleWithdrawDestinationKind,
    OracleWithdrawDestinationProof,
};

/// A real 64-hex account id. `0:aaaa` -- four hex characters -- is not an address at all, and
/// neither `CanonicalAddress::parse` nor the SDK's `Address::parse` accepts one.
const ACCOUNT: &str = "9f3c1d2e4b5a60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f9";
/// A different account, for the direction this comparison must MISS.
const OTHER_ACCOUNT: &str = "1122334455667788990011223344556677889900112233445566778899001122";

/// `--to` as the production code sees it, starting from what the operator typed.

/// Two production steps, neither retyped by hand:

/// * `args.rs:2348` -- clap's `value_parser = dexdo_core::address::arg_to_chain_param`;
/// * `oracle.rs` `preflight_oracle_fee_destination` -- `display_self_dapp(&to.with_workchain())`.

/// The middle step, `parse_oracle_read_address`, is the SDK's `Address::parse`, and its
/// `with_workchain()` renders `0:<account_id>` -- character for character what
/// `CanonicalAddress::legacy()` renders for the two spellings this flag's help advertises. That
/// equivalence is what lets this run in the default build, where the SDK type is not compiled at
/// all; where it IS compiled, the line below asserts it instead of assuming it.
fn to_display_as_production_sees_it(typed_by_the_operator: &str) -> String {
    let arg = dexdo_core::address::arg_to_chain_param(typed_by_the_operator)
        .expect("the flag's value_parser accepts what its help advertises");
    let workchain = dexdo_core::CanonicalAddress::parse(&arg)
        .expect("the value_parser emits an address")
        .legacy();
    assert_eq!(
        dexdo_core::Address::parse(&arg)
            .expect("parse_oracle_read_address")
            .with_workchain(),
        workchain,
        "the substitute for `parse_oracle_read_address` no longer renders what production renders"
    );
    dexdo_core::address::display_self_dapp(&workchain)
}

/// `hot_address` exactly as `wallet onboard manual` writes it: `verify_manual_hot_wallet`
/// (`crates/dexdo/src/cli/wallet_manual.rs`) is
/// `CanonicalAddress::parse(--multisig-address).to_string()`, and nothing else touches the value
/// between there and `binding.json`.
fn hot_address_as_wallet_onboard_manual_writes_it(multisig_address: &str) -> String {
    dexdo_core::CanonicalAddress::parse(multisig_address)
        .expect("--multisig-address")
        .to_string()
}

fn oracle_key() -> String {
    "11".repeat(32)
}

/// The reported case, end to end through both boundaries.

/// The operator bound their multisig with the legacy spelling the flag's help advertises, then
/// withdrew to that same wallet. Before this refused, and the refusal advised them to bind
/// the address with `dexdo wallet onboard` -- which is what they had done.
#[test]
fn a_manually_onboarded_hot_is_admitted_when_to_names_the_same_account() {
    let to_display = to_display_as_production_sees_it(&format!("0:{ACCOUNT}"));
    let hot = hot_address_as_wallet_onboard_manual_writes_it(&format!("0:{ACCOUNT}"));

    assert_ne!(
        hot, to_display,
        "the two production paths no longer disagree on spelling, so this test would pass without \
         proving anything -- re-derive it before deleting it"
    );

    let proof = admit_oracle_withdraw_destination(
        &to_display,
        &OracleWithdrawDestinationKind::SupportedWallet,
        &oracle_key(),
        // Deliberately empty: the DECLARED route exists for the operator whose oracle key is NOT a
        // custodian of their payout wallet, and it must admit without one.
        &[],
        &[("hot_address", hot)],
    )
    .expect("a destination the operator declared for this network is admitted");
    assert!(
        matches!(
            proof,
            OracleWithdrawDestinationProof::Declared("hot_address")
        ),
        "admitted by the wrong route: {proof:?}"
    );
}

/// The same account, whichever of the two advertised spellings either side happens to carry.

/// A multisig is a self-DApp account, so an operator who binds it canonically writes
/// `<account>::<account>`, while one who binds it legacy stores `<DEXDO_DAPP_ID>::<account>`. Both
/// name one account, and one account is one destination.
#[test]
fn either_advertised_spelling_on_either_side_names_one_destination() {
    let canonical_self_dapp = format!("{ACCOUNT}::{ACCOUNT}");
    let legacy = format!("0:{ACCOUNT}");

    for (typed, bound) in [
        (legacy.as_str(), canonical_self_dapp.as_str()),
        (canonical_self_dapp.as_str(), legacy.as_str()),
        (legacy.as_str(), legacy.as_str()),
        (canonical_self_dapp.as_str(), canonical_self_dapp.as_str()),
    ] {
        let to_display = to_display_as_production_sees_it(typed);
        let hot = hot_address_as_wallet_onboard_manual_writes_it(bound);
        let proof = admit_oracle_withdraw_destination(
            &to_display,
            &OracleWithdrawDestinationKind::SupportedWallet,
            &oracle_key(),
            &[],
            &[("vault_address", hot)],
        )
        .unwrap_or_else(|error| {
            panic!("typed {typed} against a binding written from {bound}: {error}")
        });
        assert!(
            matches!(
                proof,
                OracleWithdrawDestinationProof::Declared("vault_address")
            ),
            "typed {typed} against a binding written from {bound}: {proof:?}"
        );
    }
}

/// The direction that must MISS, because an admission rule that admits everything is not a rule.

/// A different account is not the declared one however it is spelled, and the refusal it gets is
/// the one its classification earns -- not a declared admission.
#[test]
fn a_different_account_is_not_admitted_by_the_declared_route() {
    let to_display = to_display_as_production_sees_it(&format!("0:{OTHER_ACCOUNT}"));
    let hot = hot_address_as_wallet_onboard_manual_writes_it(&format!("0:{ACCOUNT}"));
    assert_ne!(ACCOUNT, OTHER_ACCOUNT);

    let refusal = admit_oracle_withdraw_destination(
        &to_display,
        &OracleWithdrawDestinationKind::SupportedWallet,
        &oracle_key(),
        &[],
        &[("hot_address", hot)],
    )
    .expect_err("an account the operator never declared must not be admitted as declared")
    .to_string();
    assert!(
        refusal.contains("not one of its custodians"),
        "a foreign account must fall through to the custodian question: {refusal}"
    );
}

/// A declaration that is not an address admits nothing. It cannot be shown to be the same account,
/// and this function's job is to admit -- never to refuse on something it failed to read.
#[test]
fn an_unreadable_declaration_admits_nothing() {
    let to_display = to_display_as_production_sees_it(&format!("0:{ACCOUNT}"));
    let refusal = admit_oracle_withdraw_destination(
        &to_display,
        &OracleWithdrawDestinationKind::NotFound,
        &oracle_key(),
        &[],
        &[("hot_address", "not-an-address".to_string())],
    )
    .expect_err("a declaration that is not an address must not admit")
    .to_string();
    assert!(
        refusal.contains("names no account on this network"),
        "{refusal}"
    );
}
