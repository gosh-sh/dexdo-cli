//! item 1: the funding flow is reached by the commands that spend the Hot.

//! The mechanism was written before it had callers, and for a while it had none: `note deploy` and
//! `note topup` resolved a wallet from the binding and then fell back to their old
//! insufficient-balance refusals, so the Vault -> Hot request, the top-up instruction, the on-chain
//! wait and the reconciliation were all unreachable in production. That is not a behaviour a unit
//! test can catch - the code is perfectly correct and simply never runs - so the call site is pinned
//! here, the same way this file's neighbour pins the wallet turn of.

//! `run_note_deploy` reaches the chain before it reaches this point and `run_note_topup` needs a
//! live note read, so neither can be driven to the funding step offline; what CAN be proven offline
//! is that the call exists, that it is inside the wallet's turn, and that it is placed before the
//! refusal it is meant to replace.

/// The production half of `note_cmd.rs`, with its unit-test module cut off.
fn production_source() -> &'static str {
    let source = include_str!("note_cmd.rs");
    source
        .split_once("#[cfg(test)]\nmod tests")
        .expect("note_cmd unit-test module boundary")
        .0
}

/// The shared seam, not a hand-rolled slice.

/// This one ended at the next sibling `pub(crate) async fn`, with `unwrap_or(body.len())` behind
/// it: reordering `run_note_topup` to be the LAST such function in `note_cmd.rs` made "the body"
/// the whole remaining eleven thousand lines, and the guard below would have gone permanently
/// green without a line of it changing. It also kept comments, so commenting the call out left it
/// green too. `code_of` bounds by brace depth and drops both forms of comment; see

fn body_of(entry: &str) -> String {
    crate::cli::source_probe::code_of(production_source(), &format!("pub(crate) async fn {entry}"))
}

fn note_deploy_entry_body() -> String {
    crate::cli::source_probe::code_of(
        production_source(),
        "async fn run_note_deploy_with_wallet_store",
    )
}

fn note_deploy_resolved_body() -> String {
    crate::cli::source_probe::code_of(production_source(), "async fn run_note_deploy_resolved")
}

fn note_deploy_funding_body() -> String {
    let implementation = production_source()
        .split_once("impl NoteDeployResolvedOps for NoteDeployProductionOps")
        .expect("note deploy production operations")
        .1;
    crate::cli::source_probe::code_of(implementation, "async fn prepare_funding")
}

fn note_deploy_preflight_body() -> String {
    let implementation = production_source()
        .split_once("impl NoteDeployResolvedOps for NoteDeployProductionOps")
        .expect("note deploy production operations")
        .1;
    crate::cli::source_probe::code_of(implementation, "async fn preflight_doctor")
}

/// Both spenders call the shared mechanism. Neither may be the only one: a Hot short of SHELL is
/// equally unable to deploy a note and to top one up, and a funding flow wired into one command is a
/// funding flow the other silently does without.
#[test]
fn both_money_commands_call_the_shared_funding_mechanism_334() {
    assert_eq!(
        production_source()
            .matches("wallet_funding::fund_hot_for_money_command(")
            .count(),
        2,
        "note deploy AND note topup must both arrange the Hot's funding through the one mechanism"
    );
    assert!(
        note_deploy_funding_body().contains("wallet_funding::fund_hot_for_money_command("),
        "note deploy must call the shared funding mechanism after recovery routing"
    );
    assert!(
        body_of("run_note_topup").contains("wallet_funding::fund_hot_for_money_command("),
        "note topup must call the shared funding mechanism"
    );
}

/// The funding step runs INSIDE the wallet's turn.

/// It reads the Hot's balance, decides from that reading whether to ask the Vault for money, and
/// writes the journal. Two commands doing that concurrently against one Hot would both see the same
/// shortfall and both create a request for it. The turn that closes the race is the one
/// already gave the pair, and the specification's requirement is met by taking it first - not by
/// taking a second one here, which would serialize nothing the first does not.
#[test]
fn the_funding_step_runs_inside_the_wallet_turn_334() {
    let deploy = note_deploy_entry_body();
    let deploy_lock = deploy
        .find("acquire_funding_wallet_lock(")
        .expect("note deploy takes the funding wallet's turn");
    let routed = deploy
        .find("run_note_deploy_resolved(")
        .expect("note deploy enters recovery-routed operations");
    assert!(deploy_lock < routed);
    assert!(note_deploy_funding_body().contains("wallet_funding::fund_hot_for_money_command("));

    let topup = body_of("run_note_topup");
    let topup_lock = topup
        .find("acquire_funding_wallet_lock(")
        .expect("note topup takes the funding wallet's turn");
    let topup_funding = topup
        .find("wallet_funding::fund_hot_for_money_command(")
        .expect("note topup arranges funding");
    assert!(topup_lock < topup_funding);
}

/// The funding step comes before the refusal it exists to replace.

/// `note topup`'s insufficient-balance preflight is step 7 of the specification - the re-read
/// immediately before the spend - and it stays. What must not happen is the command refusing on a
/// short balance BEFORE it has offered the operator the funding flow their binding entitles them
/// to, which is exactly what it did while the mechanism had no callers.
#[test]
fn note_topup_arranges_funding_before_it_refuses_on_a_short_balance_334() {
    let body = body_of("run_note_topup");
    let funding = body
        .find("wallet_funding::fund_hot_for_money_command(")
        .expect("note topup arranges funding");
    let refusal = body
        .find("note_topup_preflight_wallet_ecc(")
        .expect("note topup keeps its preflight");
    assert!(
        funding < refusal,
        "the funding flow must be offered before the insufficient-balance refusal, or the refusal \
         is all an operator with a bound wallet ever sees"
    );
}

/// An explicit wallet skips the durable binding and reaches the shared entrypoint with its facts.

/// `None` here means only that no durable provider binding selected the Hot. The shared entrypoint
/// receives the already-resolved Hot and the manifest network, and the explicit path there selects
/// the ephemeral Manual flow without persisting a binding or inferring a provider from the address.
#[test]
fn an_explicit_wallet_skips_durable_binding_but_reaches_manual_route_334() {
    for entry in ["run_note_deploy", "run_note_topup"] {
        let body = if entry == "run_note_deploy" {
            note_deploy_entry_body()
        } else {
            body_of(entry)
        };
        let end = body
            .find(if entry == "run_note_deploy" {
                "run_note_deploy_resolved("
            } else {
                "wallet_funding::fund_hot_for_money_command("
            })
            .expect("funding route");
        let head = &body[..end];
        assert!(
            head.contains("let funding_binding = match args.multisig_address.as_deref() {"),
            "{entry} must decide the funding binding from whether an address was passed"
        );
        assert!(
            head.contains("Some(_) => None,"),
            "{entry} must not load a durable binding when the explicit wallet won"
        );
        let funding = if entry == "run_note_deploy" {
            note_deploy_funding_body()
        } else {
            body[end..].to_string()
        };
        assert!(
            (funding.contains("binding: self.funding_binding,")
                && funding.contains("resolved_hot_address: self.resolved_hot_address,")
                && funding.contains("network: self.funding_network,"))
                || (funding.contains("binding: funding_binding.as_ref(),")
                    && funding.contains("resolved_hot_address: &funding_wallet.address,")
                    && funding.contains("network: &funding_network,")),
            "{entry} must pass the resolved explicit Hot and manifest network to the shared \
             entrypoint that selects the ephemeral Manual flow"
        );
    }
}

/// This client's own clock is established as sane BEFORE the funding flow compares one.

/// The reconciliation reads the chain clock to decide, and the local clock to report. Both readings
/// are worthless if this machine's clock is wrong, and a skew check that runs after the decision it
/// is supposed to protect is not a protection at all. `note deploy` already ran the check before it
/// reached the funding step; `note topup` had one only inside its submit helper, which is a later
/// moment entirely. The same helper serves both - there is no second notion of a sane clock.
#[test]
fn both_commands_check_the_clock_before_the_funding_flow_reads_one_334() {
    assert!(
        note_deploy_preflight_body().contains("chain_clock_skew_preflight("),
        "run_note_deploy checks this machine's clock against the chain"
    );
    let deploy = note_deploy_resolved_body();
    let deploy_skew = deploy
        .find("ops.preflight_doctor().await?")
        .expect("note deploy runs its clock-bearing preflight");
    let deploy_funding = deploy
        .find("ops.prepare_funding(funding_route).await?")
        .expect("note deploy arranges funding");
    assert!(deploy_skew < deploy_funding);

    let topup = body_of("run_note_topup");
    let topup_skew = topup
        .find("chain_clock_skew_preflight(")
        .expect("run_note_topup checks this machine's clock against the chain");
    let topup_funding = topup
        .find("wallet_funding::fund_hot_for_money_command(")
        .expect("run_note_topup arranges funding");
    assert!(
        topup_skew < topup_funding,
        "run_note_topup must prove its clock before the funding flow reads one, or the \
         reconciliation decides against a clock nothing has checked"
    );
}

/// The documented override reaches the mechanism from both commands.
#[test]
fn the_funding_timeout_override_is_passed_through_334() {
    assert_eq!(
        production_source().matches("args.funding_timeout,").count(),
        2,
        "`--funding-timeout` must reach the wait from both commands, or the documented override \
         changes nothing"
    );
}
