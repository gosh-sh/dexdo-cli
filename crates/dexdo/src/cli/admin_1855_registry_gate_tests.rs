//! - the seller lists only a name the registry confirmed, and only flags it can spell.

//! Two axes, and they are separate on purpose because the owner stated them separately: the
//! **registry** decides the base name, and the **flag grammar** decides how a capability tail is
//! written. A test that conflated them would pass while one of the two did nothing, so each axis is
//! asserted on an input the other one cannot judge.

//! What these guard, measured on `dev` at 56e27c06 before the change:

//! - `dexdo seller` asked the catalog NOTHING on its default path. The one registry block in
//! `run_seller_with_deal_gas_overhead` sits behind `if let Some(policy) = registry_policy...`, and
//! `RegistryValidationPolicy::disabled()` sets `seller_check_model_registry: false`, so without an
//! explicit `--model-registry-validation <config.json>` the policy is `None`. The only other name
//! gate on that path is `require_model_name`, which refuses an empty or space-padded name -- not a
//! membership test. Meanwhile the seller's own `post_offer` deploys the order book out of the note
//! when it is absent, so the command was creating a market under an unchecked name.
//! - No path in the tree turned a malformed flag tail into a refusal. All three production callers
//! of `parse_canonical_model_id` discard its error (`if let Ok`, `.ok()`, `.ok()?`), so
//! `qwen--qwen3--32b--toolz` was a name like any other.

use super::{ensure_model_flags_are_canonical, model_resolution_result, ModelResolutionCaller};

/// The registry axis: a spelling the catalog holds differently is refused, and the refusal is the
/// seller's own.


/// is named so one edit fixes the file. Section 10 is this exact input, measured on mainnet on
/// 31 August 2026 -- a market was deployed at `sha256("qwen--qwen3.8--27b--fp8")` while the catalog
/// held `Qwen3.8-27B`, and no buyer taking the name from the registry could reach it.
#[test]
fn the_seller_refuses_a_spelling_the_registry_holds_differently() {
    let error = model_resolution_result(
        ModelResolutionCaller::Seller,
        "qwen--qwen3.8--27b--fp8",
        false,
        Ok("Qwen3.8-27B".to_string()),
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("seller refuses before deploying anything"),
        "the refusal must name the command the operator ran: {message}"
    );
    assert!(
        message.contains("Qwen3.8-27B"),
        "the refusal must name the registered spelling: {message}"
    );
    assert!(
        message.contains("fp8"),
        "a flagged name loses its flags on the way to the catalog, so the refusal has to say which \
         ones rather than hand over a name that means a different market: {message}"
    );
}

/// THE LIVE CASE, and it is the reason this is a money path rather than a tidiness one.

/// Observed on a live campaign run, in `live_520_strict_reference_buyer_serves_model_response`: the
/// seller listed the market as `qwen--qwen3.6--27b` and the BUYER is what refused, because the buyer
/// resolves the name and the seller did not. The buyer's own words, from that run:

/// ```text
/// "failure_class":"content_identity_preflight","missing_or_unset":"registry_model_name",
/// "cause":"the ModelRegistry holds this model as `Qwen3.6-27B`, and `qwen--qwen3.6--27b` is a
/// different name to the chain. A market listed under a name the registry does not hold is one no
/// buyer resolving through the registry can reach, so escrow placed against it may find no seller
/// at all. Write `Qwen3.6-27B`"
/// ```

/// "escrow placed against it may find no seller at all" is the cost, stated by the refusal itself.
/// After this change the SELLER refuses first, before it posts, and the buyer never gets the chance
/// to place that escrow.

/// Note which axis catches it: `qwen--qwen3.6--27b` has three base parts and no flag tail, so the
/// flag grammar has nothing to say about it -- this is the registry axis alone, and the assertion
/// below pins that the two do not get confused.
#[test]
fn the_live_520_seller_spelling_is_refused_before_the_offer_is_posted() {
    let listed = "qwen--qwen3.6--27b";
    ensure_model_flags_are_canonical(ModelResolutionCaller::Seller, listed, false).expect(
        "no flag tail here: this is a base-name spelling, and the flag gate must not claim it",
    );
    let error = model_resolution_result(
        ModelResolutionCaller::Seller,
        listed,
        false,
        Ok("Qwen3.6-27B".to_string()),
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("seller refuses before deploying anything"),
        "the seller is the side that must refuse, and it must say so: {message}"
    );
    assert!(
        message.contains("Qwen3.6-27B") && message.contains("qwen--qwen3.6--27b"),
        "the refusal names both what was written and what the registry holds, so one edit fixes \
         models.json: {message}"
    );
}

/// And the opt-out does not reach that arm.


/// not confirm this model", not about "I got the spelling wrong". A model confirmed under another
/// spelling is a confirmed model and a typo. Letting the flag through here would make it the way to
/// leave a wrong spelling on chain -- the outcome it exists to prevent.
#[test]
fn the_opt_out_does_not_rescue_a_wrong_spelling_on_the_seller_path() {
    let error = model_resolution_result(
        ModelResolutionCaller::Seller,
        "qwen--qwen3.8--27b--fp8",
        true,
        Ok("Qwen3.8-27B".to_string()),
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("Qwen3.8-27B"),
        "--allow-unverified-model must not turn a typo into a listing: {error}"
    );
}

/// A name the catalog does not carry at all: refused, and the refusal states what the listing would
/// have cost. The cost is the seller's real one -- its `post_offer` deploys the book when absent.
#[test]
fn the_seller_refuses_a_name_the_registry_does_not_carry() {
    let error = model_resolution_result(
        ModelResolutionCaller::Seller,
        "acme--vision--v1",
        false,
        Err(anyhow::anyhow!("model is not registered")),
    )
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("seller refuses before deploying anything"),
        "{message}"
    );
    assert!(
        message.contains("order book") && message.contains("never trade"),
        "the refusal must state the spend it prevented: {message}"
    );
}

/// The opt-out still does what it is for: an unconfirmed name lists, deliberately and explicitly.
#[test]
fn the_opt_out_still_lists_an_unconfirmed_name() {
    let outcome = model_resolution_result(
        ModelResolutionCaller::Seller,
        "acme--vision--v1",
        true,
        Err(anyhow::anyhow!("model is not registered")),
    )
    .expect("--allow-unverified-model is the explicit escape hatch and must stay open");
    assert!(
        outcome.is_none(),
        "nothing was confirmed, so there is no registered name to carry: {outcome:?}"
    );
}

/// A name the catalog confirms byte for byte lists. Without this the suite would be satisfied by a
/// gate that refuses everything.
#[test]
fn a_name_the_registry_confirms_byte_for_byte_lists() {
    let outcome = model_resolution_result(
        ModelResolutionCaller::Seller,
        "Qwen3-32B",
        false,
        Ok("Qwen3-32B".to_string()),
    )
    .expect("the registered spelling must list");
    assert_eq!(outcome.as_deref(), Some("Qwen3-32B"));
}

/// The FLAG axis: a token that is not a flag token.

/// This verdict needs no chain, and it is reached without one -- the gate runs before the registry is
/// read, so the refusal names the offending token instead of the generic "does not resolve" a
/// catalog miss would produce.
#[test]
fn a_flag_token_that_is_not_a_flag_token_is_refused() {
    let error = ensure_model_flags_are_canonical(
        ModelResolutionCaller::Seller,
        "qwen--qwen3--32b--toolz",
        false,
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("toolz"),
        "the refusal must name the token that is wrong: {message}"
    );
    assert!(
        message.contains("seller refuses before deploying anything"),
        "{message}"
    );
}

/// Order is part of the grammar: `producer--model--version[--unit][--w<N>k][--<N>p][--tools]
/// [--think][--<precision>]`. `tools` is slot 3 and a window is slot 1, so this pair is backwards.
#[test]
fn flags_written_out_of_canonical_order_are_refused() {
    let error = ensure_model_flags_are_canonical(
        ModelResolutionCaller::Seller,
        "qwen--qwen3--32b--tools--w8k",
        false,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("w8k"),
        "the refusal must name the token that is out of place: {error}"
    );
}

/// One slot, one occurrence. Two windows are two answers to the same question.
#[test]
fn a_slot_written_twice_is_refused() {
    let error = ensure_model_flags_are_canonical(
        ModelResolutionCaller::Seller,
        "qwen--qwen3--32b--w8k--w16k",
        false,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("w16k"),
        "the refusal must name the repeat: {error}"
    );
}

/// A packing spelling is not a capability flag, and the grammar says so by name rather than by
/// falling through to "unknown token".
#[test]
fn a_packing_spelling_is_refused_as_a_flag() {
    let error = ensure_model_flags_are_canonical(
        ModelResolutionCaller::Seller,
        "qwen--qwen3--32b--awq",
        false,
    )
    .unwrap_err();
    assert!(error.to_string().contains("awq"), "{error}");
}

/// The opt-out reaches the FLAG axis, and the directive is what scopes it there.

/// Section 5 names exactly one thing `--allow-unverified-model` does not cover -- a model the registry
/// confirmed under another spelling -- and `the_opt_out_does_not_rescue_a_wrong_spelling_on_the_seller_path`
/// above pins that arm shut. Everything else it covers, and its stated purpose is trading a model the
/// catalog does not confirm: a new one, or the operator's OWN. An own name is a name this grammar has
/// no authority over, and `acme--vision--v1--internal` is indistinguishable in code from a canonical
/// name with one bad flag; refusing it under the flag would be the shape gate returning.

/// So the pair of assertions here is the whole rule: bites by default, yields to the explicit flag.
#[test]
fn the_flag_gate_bites_by_default_and_yields_to_the_explicit_opt_out() {
    let own_name = "acme--vision--v1--internal";
    assert!(
        ensure_model_flags_are_canonical(ModelResolutionCaller::Seller, own_name, false).is_err(),
        "on the default path a tail that is not the canonical flag grammar is refused"
    );
    ensure_model_flags_are_canonical(ModelResolutionCaller::Seller, own_name, true).expect(
        "the operator has said the catalog is not the authority on this name; the flag grammar is \
         not either",
    );
}

/// THE CONTROL, and it is the one that matters most here.

/// removed a client-side shape gate that required three `--` parts of EVERY name, because the
/// 4.0.36 catalog does not use that shape: it seeds `Qwen/Qwen3-32B` as `Qwen3-32B`, and names like
/// `qwen3.8-max` carry no `--` at all. Those names were refused before the catalog was asked, and
/// the operator was told to rewrite a name that was already correct.

/// This test is what makes the two claims separable: the flag gate above fires, and here the same
/// gate passes every shape a registered name can have. A gate that refused these would be that
/// removed shape check returning under a new name, and the suite would say so.
#[test]
fn the_flag_gate_judges_no_name_that_claims_no_flags() {
    for name in [
        // What the live catalog actually holds.
        "Qwen3-32B",
        "qwen3.8-max",
        "Qwen3.8-27B",
        // Provider slugs and local labels, which the candidate walk handles and this gate must not.
        "qwen/qwen3-32b",
        "dexdo-mock",
        // The bare three-part base: a flag tail is what this gate reads, and there is none here.
        "qwen--qwen3--32b",
        "a--b",
    ] {
        assert!(
            ensure_model_flags_are_canonical(ModelResolutionCaller::Seller, name, false).is_ok(),
            "`{name}` claims no capability flags, so the flag grammar has nothing to say about it; \
             refusing it is the  shape gate returning"
        );
    }
}

/// The two axes are separate, and this is the input that proves it: well-formed flags PASS the flag
/// gate, and it is the registry that then refuses the name.

/// If the two were one rule, one of these two assertions would have to fail.
#[test]
fn well_formed_flags_pass_the_flag_gate_and_the_registry_is_what_refuses_them() {
    let flagged = "qwen--qwen3--32b--w8k--tools";
    ensure_model_flags_are_canonical(ModelResolutionCaller::Seller, flagged, false)
        .expect("`w8k` then `tools` is slot 1 then slot 3: canonical, and the grammar says so");
    let error = model_resolution_result(
        ModelResolutionCaller::Seller,
        flagged,
        false,
        Ok("Qwen3-32B".to_string()),
    )
    .unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("w8k") && message.contains("tools"),
        "the registry axis refuses this one, and it has to say the flags are the reason rather than \
         advise a name that lists a different, unflagged book: {message}"
    );
}

/// The wiring: `dexdo seller` asks, and it does not ask only when a config file switches it on.

/// The ordering is the whole property. `if let Some(policy) = registry_policy.as_ref()` is the
/// opt-in block the old check lived inside; a gate that appears BEFORE it in the body is a gate that
/// block cannot be holding. And both must precede the gateway coming up and the pool that posts the
/// offer, because everything from there on is a write.
#[test]
fn the_seller_asks_the_registry_before_it_lists_and_not_only_behind_a_config_file() {
    let body = crate::cli::source_probe::code_of(
        include_str!("seller.rs"),
        "pub(crate) async fn run_seller_with_deal_gas_overhead",
    );
    let gate = body
        .find("ensure_model_resolves(")
        .expect("the seller must resolve the model name the way provision and deploy-market do");
    let opt_in = body
        .find("if let Some(policy) = registry_policy.as_ref()")
        .expect("run_seller still has the role-scoped registry block");
    assert!(
        gate < opt_in,
        "the registry gate is at {gate} and the opt-in policy block at {opt_in}: the gate is inside \
         the block that is `None` unless --model-registry-validation was passed, which is the \
         default path having no check at all"
    );
    for later in ["start_gateway_with_note", "run_seller_pool("] {
        let at = body
            .find(later)
            .unwrap_or_else(|| panic!("run_seller still calls {later}"));
        assert!(
            gate < at,
            "`{later}` runs at {at} and the registry gate only at {gate}: the listing would be \
             under way before anyone asked whether a buyer could resolve its name"
        );
    }
    // And the comment stripping actually removed something from THIS body, or the finds above
    // prove nothing about code: a call that is commented out is not a call, and `admin.rs` measured
    // exactly that failure on its own guards.
    assert!(
        !body.contains("// "),
        "comment lines must be gone from the scanned body"
    );
}
