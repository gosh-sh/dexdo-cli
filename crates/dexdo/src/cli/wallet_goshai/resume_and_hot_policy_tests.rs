//! follow-up items 5 and 6: the resume the error message promises, and the exact Hot policy.

//! Two defects from the post-merge audit, held shut here.

//! **The resume did not resume.** The activation-timeout error tells the operator that the phrase
//! and the draft are already stored and that running the command again "continues waiting without
//! asking for the phrase again". It did not: the store minted a fresh binding id and the flow always
//! called `prepare_onboarding`, so every timeout cost another paste of a 12-word recovery phrase --
//! which is exactly the pressure that makes somebody keep it in a file or the clipboard.

//! **The Hot policy was under-enforced.** Membership of the derived key was checked, but not the
//! shape the owner froze: exactly two distinct pubkey custodians, `requiredTxnConfirms=1` and
//! `requiredDataConfirms=2`. Each missing check names somebody who can take the money, so each is
//! refused here by its own violating shape rather than by one blanket assertion.

use super::files;
use super::{resume_onboarding, verify_active_hot, ActiveHotFacts, HotRefusal};

// A self-dApp address: the two 64-hex halves are equal, which is what makes it a wallet rather than
// a contract inside somebody's DApp.
const HOT_ACCOUNT: &str = "5cbb90f8a1d3e4f2c6b7a8091d2e3f4a5b6c7d8e9f0a1b2c3d4e5f60718293a4";

/// The user's custodian key, as the chain reports it.
const USER_KEY: &str = "1111111111111111111111111111111111111111111111111111111111111111";
/// The Gosh.ai service custodian: the key the sub-wallet's address is derived from.
const SERVICE_KEY: &str = "2222222222222222222222222222222222222222222222222222222222222222";
/// A third party who, at `reqConfirms=1`, could spend the Hot alone.
const THIRD_KEY: &str = "3333333333333333333333333333333333333333333333333333333333333333";

fn hot_address() -> String {
    format!("{HOT_ACCOUNT}::{HOT_ACCOUNT}")
}

fn address() -> dexdo_core::CanonicalAddress {
    dexdo_core::CanonicalAddress::parse(&hot_address()).expect("fixture address")
}

/// `getCustodians()` in the shape the COMPILED ABI declares: a `tuple[]` whose `owner_pubkey` is an
/// `optional(uint256)`.
fn custodians_with(keys: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "custodians": keys
            .iter()
            .enumerate()
            .map(|(index, key)| serde_json::json!({
                "owner_pubkey": key,
                "owner_address": serde_json::Value::Null,
                "index": index,
            }))
            .collect::<Vec<_>>()
    })
}

/// The Gosh.ai Hot exactly as the specification freezes it. Every refusal below is one field of this
/// changed, so nothing passes or fails for a second reason.
fn compliant_facts() -> ActiveHotFacts {
    ActiveHotFacts {
        code_hash: dexdo_core::canonical_multisig::CODE_HASH.to_string(),
        version: dexdo_core::canonical_multisig::VERSION.to_string(),
        contract_name: dexdo_core::canonical_multisig::CONTRACT_NAME.to_string(),
        required_txn_confirms: 1,
        required_data_confirms: 2,
        custodians: custodians_with(&[SERVICE_KEY, USER_KEY]),
    }
}

/// The baseline: the frozen shape is accepted, so every refusal below is caused by the one field it
/// changes and not by the fixture being wrong.
#[test]
fn the_frozen_gosh_ai_hot_shape_is_accepted() {
    assert_eq!(
        verify_active_hot(&address(), &compliant_facts(), USER_KEY),
        Ok(())
    );
}

/// Invariant 1: `requiredDataConfirms = 2`.

/// Below two, ONE custodian can rewrite the custodian set by itself -- the power to hand the wallet
/// to somebody else. `0` and `1` are both refused; the loop also covers a wallet that merely reports
/// something unexpected.
#[test]
fn a_hot_that_lets_fewer_than_two_custodians_rewrite_the_custodian_set_is_refused() {
    for required in [0, 1, 3] {
        let mut facts = compliant_facts();
        facts.required_data_confirms = required;
        assert_eq!(
            verify_active_hot(&address(), &facts, USER_KEY),
            Err(HotRefusal::DataThresholdNotTwo {
                required_data_confirms: required
            }),
            "requiredDataConfirms={required} must be refused, never warned about"
        );
    }
}

/// Invariant 2: exactly two DISTINCT pubkey custodians.

/// A third custodian is a third party, and at `reqConfirms=1` they can empty the Hot alone.
#[test]
fn a_hot_carrying_a_third_custodian_is_refused() {
    let mut facts = compliant_facts();
    facts.custodians = custodians_with(&[SERVICE_KEY, USER_KEY, THIRD_KEY]);
    assert_eq!(
        verify_active_hot(&address(), &facts, USER_KEY),
        Err(HotRefusal::CustodianCountNotTwo {
            distinct_custodians: 3
        })
    );
}

/// Invariant 3: the second, Gosh.ai service custodian must be there.

/// A wallet whose only pubkey custodian is the user's key is not the managed sub-wallet this
/// provider is defined to hand over, so whatever the operator is about to fund is not what they
/// think they are funding.
#[test]
fn a_hot_holding_only_the_users_key_is_refused_for_want_of_the_service_custodian() {
    let mut facts = compliant_facts();
    facts.custodians = custodians_with(&[USER_KEY]);
    assert_eq!(
        verify_active_hot(&address(), &facts, USER_KEY),
        Err(HotRefusal::ServiceCustodianMissing {
            derived_public_key: USER_KEY.to_string()
        })
    );
}

/// The same key listed twice is ONE custodian with one signature. Counting entries rather than
/// distinct keys would let a duplicate satisfy "exactly two" while the user's key sits alone on the
/// wallet -- so this must land on the missing-service refusal, not on `Ok`.
#[test]
fn a_duplicated_custodian_does_not_count_as_two() {
    let mut facts = compliant_facts();
    facts.custodians = custodians_with(&[USER_KEY, USER_KEY]);
    assert_eq!(
        verify_active_hot(&address(), &facts, USER_KEY),
        Err(HotRefusal::ServiceCustodianMissing {
            derived_public_key: USER_KEY.to_string()
        })
    );
}

/// The new checks must not have displaced the ones that were already there: each earlier refusal
/// still fires on its own shape, in its own order. Without this, adding a check ahead of the
/// membership test would silently turn "this phrase does not control this address" into a count
/// complaint, and the operator would be told the wrong thing about their own wallet.
#[test]
fn the_new_checks_did_not_reorder_the_existing_refusals() {
    let mut wrong_txn = compliant_facts();
    wrong_txn.required_txn_confirms = 2;
    assert_eq!(
        verify_active_hot(&address(), &wrong_txn, USER_KEY),
        Err(HotRefusal::ThresholdNotOne {
            required_txn_confirms: 2
        }),
        "a Vault-style spending threshold is still its own refusal"
    );

    // Two custodians, neither of them ours: the answer is "these two halves do not belong
    // together", not "the count is wrong" -- the count is right.
    let mut not_ours = compliant_facts();
    not_ours.custodians = custodians_with(&[SERVICE_KEY, THIRD_KEY]);
    assert!(
        matches!(
            verify_active_hot(&address(), &not_ours, USER_KEY),
            Err(HotRefusal::CustodianMissing { .. })
        ),
        "membership is still answered before the count"
    );

    // An address-only custodian carries no pubkey, so the list is empty rather than wrong.
    let mut address_only = compliant_facts();
    address_only.custodians = serde_json::json!({
        "custodians": [{
            "owner_pubkey": serde_json::Value::Null,
            "owner_address": "0:1111111111111111111111111111111111111111111111111111111111111111",
            "index": 0,
        }]
    });
    assert_eq!(
        verify_active_hot(&address(), &address_only, USER_KEY),
        Err(HotRefusal::NoCustodians)
    );
}

// ---------------------------------------------------------------------------------------------
// Item 5, first half: the resume the error message promises
// ---------------------------------------------------------------------------------------------

/// A 12-word phrase built from the wordlist by index, so no fixture drifts.
fn phrase() -> String {
    bip39::Mnemonic::from_entropy(&[0x11; 16], bip39::Language::English)
        .expect("12-word fixture phrase")
        .phrase()
        .to_string()
}

/// The scripted stand-in for the terminal used by the FIRST attempt only. The whole point of the
/// resume is that the second attempt never reaches one of these.
struct OneShotPrompt(Vec<String>);

impl super::HiddenPrompt for OneShotPrompt {
    fn read_hidden(&mut self, _prompt: &'static str) -> anyhow::Result<zeroize::Zeroizing<String>> {
        if self.0.is_empty() {
            anyhow::bail!("the flow asked for the hidden wallet string again");
        }
        Ok(zeroize::Zeroizing::new(self.0.remove(0)))
    }
}

/// A stand-in for the TVM-SDK derivation, which only exists in a chain build.

/// Deterministic and phrase-DEPENDENT, which is the property the resume test needs: a resume that
/// read the wrong file, or no file at all, cannot produce the same key by accident.
fn derive_stub(phrase: &str) -> anyhow::Result<String> {
    let mut bytes = phrase.as_bytes().to_vec();
    bytes.resize(32, 0);
    Ok(hex::encode(&bytes[..32]))
}

/// One timed-out attempt, as the operator leaves it: phrase and draft on disk, no binding.
fn timed_out_attempt(data_dir: &std::path::Path, binding_id: &str) -> super::PreparedOnboarding {
    let mut prompt = OneShotPrompt(vec![format!("{} {}", hot_address(), phrase())]);
    let mut notices = Vec::new();
    super::prepare_onboarding(
        data_dir,
        crate::cli::wallet::test_network_a(),
        binding_id,
        &mut prompt,
        &mut notices,
        &derive_stub,
    )
    .expect("the first attempt stores the phrase and the draft")
}

/// THE DEFECT. A second `wallet onboard gosh-ai` after a timeout continues the attempt already on
/// disk instead of asking for the recovery phrase again.

/// Both halves are asserted, because either alone would still leave the promise broken:

/// - the STORE must hand the flow the id the first attempt reserved (`find_resumable`), or the flow
/// would resume under a fresh id and `commit_onboarded` would refuse the binding at the last step;
/// - the FLOW must rebuild its facts from that directory without a prompt (`resume_onboarding`).
#[test]
fn a_second_onboarding_after_a_timeout_resumes_instead_of_asking_for_the_phrase_again() {
    let dir = tempfile::tempdir().expect("temp dir");
    let binding_id = "a".repeat(32);
    let first = timed_out_attempt(dir.path(), &binding_id);

    // The unfinished attempt is discoverable, and it is the one the store reserved.
    let found = files::find_resumable(
        &dir.path().join("wallet"),
        &crate::cli::wallet::test_network_a(),
    );
    assert_eq!(
        found.as_deref(),
        Some(binding_id.as_str()),
        "the timed-out attempt must be the one a second onboard continues"
    );

    // And resuming it asks for nothing: `resume_onboarding` takes no prompt at all, and it
    // reproduces exactly what the first attempt proved.
    let resumed = resume_onboarding(
        dir.path(),
        crate::cli::wallet::test_network_a(),
        &binding_id,
        &derive_stub,
    )
    .expect("the stored attempt is readable")
    .expect("a stored attempt is a resumable attempt");

    assert_eq!(
        resumed.hot_address.to_string(),
        first.hot_address.to_string(),
        "the resumed attempt must wait for the same Hot"
    );
    assert_eq!(
        resumed.derived_public_key, first.derived_public_key,
        "the custodian key must be re-derived from the stored phrase, not asked for again"
    );
    assert_eq!(
        resumed.paths.seed_file, first.paths.seed_file,
        "the resumed attempt must keep using the phrase already stored"
    );
}

/// A draft written for one chain must never be continued by a command configured for the other:
/// the phrase would be proved against an address on a network nobody asked about.
#[test]
fn a_draft_from_another_network_is_not_resumed() {
    let dir = tempfile::tempdir().expect("temp dir");
    let binding_id = "b".repeat(32);
    timed_out_attempt(dir.path(), &binding_id);

    assert_eq!(
        files::find_resumable(
            &dir.path().join("wallet"),
            &crate::cli::wallet::test_network_b()
        ),
        None,
        "a draft for one network is not an attempt on another"
    );
    assert!(
        resume_onboarding(
            dir.path(),
            crate::cli::wallet::test_network_b(),
            &binding_id,
            &derive_stub
        )
        .expect("a mismatched draft is not an error")
        .is_none(),
        "the flow must fall back to a fresh attempt rather than resume across networks"
    );
}

/// Nothing to resume is not an error: a first-ever onboarding must fall through to the normal path.
#[test]
fn an_untouched_data_directory_has_nothing_to_resume() {
    let dir = tempfile::tempdir().expect("temp dir");
    assert_eq!(
        files::find_resumable(
            &dir.path().join("wallet"),
            &crate::cli::wallet::test_network_a()
        ),
        None
    );
    assert!(resume_onboarding(
        dir.path(),
        crate::cli::wallet::test_network_a(),
        &"c".repeat(32),
        &derive_stub
    )
    .expect("an absent draft is not an error")
    .is_none());
}

/// A FINISHED attempt must not look unfinished.

/// Without retiring the draft, a later `wallet onboard` would adopt the id of the binding that is
/// already active and write over the wallet the operator is using. `run_selected` calls
/// `discard_draft_in` after the commit; this is that contract.
#[test]
fn a_committed_attempt_is_no_longer_resumable() {
    let dir = tempfile::tempdir().expect("temp dir");
    let binding_id = "d".repeat(32);
    let prepared = timed_out_attempt(dir.path(), &binding_id);

    files::discard_draft_in(&prepared.paths.binding_dir);

    assert_eq!(
        files::find_resumable(
            &dir.path().join("wallet"),
            &crate::cli::wallet::test_network_a()
        ),
        None,
        "a committed attempt must never be adopted by a later onboarding"
    );
    assert!(
        prepared.paths.seed_file.is_file(),
        "retiring the draft must not remove the committed binding's own secret"
    );
}
