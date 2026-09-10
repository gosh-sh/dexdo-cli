//! What replaces's five checks once the manifest stops carrying a copy of the pins.

//! was this: the 4.0.36 manifest declared five contracts "carried over from 4.0.35
//! unchanged", all five artifacts had moved, and three of the five kept the same file SIZE so
//! `git show --stat` showed nothing. The manifest-pin check could not see it either, because both
//! of its sides were the manifest. The fix then was to hash the artifact and compare -- to give the
//! line a second source, one written by the compiler rather than by a person.

//! **This removes the person's side instead.** Every preflight that needs to know what a deployed
//! account is supposed to be now asks [`compiled_contract_hash`], which hashes the image this build
//! carries. There is no second copy to disagree with the first, so a recompiled artifact cannot be
//! left un-repinned: the expected value moved with it.'s defect is not detected here, it is
//! unrepresentable.

//! What still has to be held, and is held below: that the lookup really reads the image, that it
//! refuses a name this build does not carry rather than answering with someone else's number, and
//! that the reverse lookup and the forward one cannot drift apart.

use crate::chain::contracts_provision::{
    code_hash, compiled_contract_hash, compiled_contract_hashes, COMPILED_CONTRACT_IMAGES,
};

/// The expected value is the artifact's own hash -- computed here from the same bytes, by a second
/// route, so that a lookup which returned a constant would fail.
#[test]
fn every_expected_hash_is_the_hash_of_the_image_this_build_carries() {
    assert!(
        !COMPILED_CONTRACT_IMAGES.is_empty(),
        "this build carries no compiled artifacts at all, so nothing below is being checked"
    );
    for (name, image) in COMPILED_CONTRACT_IMAGES {
        let independently = code_hash(image)
            .unwrap_or_else(|error| panic!("hash the vendored {name} image: {error}"));
        let looked_up = compiled_contract_hash(name)
            .unwrap_or_else(|error| panic!("look up the {name} hash: {error}"));
        assert_eq!(
            looked_up, independently,
            "the {name} expected hash does not come from the {name} image"
        );
        assert_eq!(
            independently.len(),
            64,
            "the {name} image hashed to something that is not 32 bytes of hex: {independently}"
        );
    }
}

/// A name this build does not carry is REFUSED, not answered.

/// The shape that matters: the old lookup read a map and could return `None` for a contract the
/// manifest had simply forgotten, which's `..._a_missing_pin_is_refused_rather_than_skipped`
/// was written to catch. The same hole exists here if a caller ever asks for a contract this tree
/// does not vendor, and it must refuse by name rather than fall through to a default.
#[test]
fn a_contract_this_build_does_not_carry_is_refused_by_name() {
    let error = compiled_contract_hash("NotAContractThisTreeVendors")
        .expect_err("a contract with no vendored image must not resolve to a hash");
    let said = error.to_string();
    assert!(
        said.contains("NotAContractThisTreeVendors"),
        "the refusal does not name what was asked for: {said}"
    );
    assert!(
        said.contains("no compiled"),
        "the refusal does not say what is missing: {said}"
    );
}

/// The reverse lookup answers with the same numbers as the forward one, for exactly the same names.

/// Two callers read these: `oracle withdraw-fees` asks "is this destination one of ours, and
/// which?" by walking the pairs, while every account preflight asks the forward question by name. A
/// build where the two disagreed would let a destination be admitted as a contract whose identity
/// check would then refuse it.
#[test]
fn the_reverse_lookup_and_the_forward_one_cannot_drift_apart() {
    let pairs = compiled_contract_hashes();
    assert_eq!(
        pairs.len(),
        COMPILED_CONTRACT_IMAGES.len(),
        "the reverse lookup dropped an artifact: {} of {}",
        pairs.len(),
        COMPILED_CONTRACT_IMAGES.len()
    );
    for (name, hash) in &pairs {
        let forward = compiled_contract_hash(name)
            .unwrap_or_else(|error| panic!("forward lookup of {name}: {error}"));
        assert_eq!(
            *hash, forward,
            "reverse and forward lookups disagree about {name}"
        );
    }
    let mut names: Vec<&str> = pairs.iter().map(|(name, _)| *name).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(
        names.len(),
        before,
        "the reverse lookup reports one name twice, so a destination could match the wrong contract"
    );
}

/// EVERY pin of a row that is of THIS tree's generation equals the artifact vendored beside it.

/// **This is the offline binding nearly dropped.** It used to exist transitively: each row
/// field was compared to the manifest's `contract_hashes.<X>`, and that manifest value was compared
/// to the hash of the image. Removing the manifest copy removed both links at once, and a mistyped
/// digit in `rootpn` or `private_note` would then make every guard on that chain refuse correct
/// accounts with nothing offline to say so.

/// Held to rows OF THIS GENERATION ONLY, and that is not a softening. The artifacts are vendored
/// once and are of one generation; a row for a chain still on the previous one records that chain's
/// code and MUST differ from them -- during a staged rollout, demanding otherwise is demanding that
/// the rollout never happen. Which generation the artifacts are is not written down anywhere and is
/// not asked for here: it is DERIVED as the generation of the rows that match, and at least one row
/// is required to match, so the rule cannot pass by finding nothing.
#[test]
fn every_pin_of_this_generation_equals_the_artifact_vendored_beside_it() {
    use crate::chain::contracts_provision::GENERATION_PINS;

    type PinProjection = fn(&crate::chain::contracts_provision::GenerationPins) -> Option<String>;

    // (row field, the name its artifact is vendored under)
    let bound: &[(&str, PinProjection)] = &[
        ("SuperRoot", |row| Some(row.superroot.to_string())),
        ("RootPN", |row| Some(row.rootpn.to_string())),
        ("RootOracle", |row| Some(row.rootoracle.to_string())),
        ("PrivateNote", |row| Some(row.private_note.to_string())),
        ("TokenContract", |row| {
            row.token_contract_code.map(str::to_string)
        }),
        ("InferenceOrderBook", |row| {
            row.inference_orderbook.map(str::to_string)
        }),
    ];

    let matching: Vec<&str> = GENERATION_PINS
        .iter()
        .filter(|row| {
            pins_match_the_artifacts(
                &bound
                    .iter()
                    .map(|(name, read)| (*name, read(row)))
                    .collect::<Vec<_>>(),
            )
        })
        .map(|row| row.version)
        .collect();

    assert!(
        !matching.is_empty(),
        "no row matches the artifacts this tree vendors, so either the pins or the artifacts were \
         moved without the other -- and nothing else offline would have said so"
    );

    let generation = matching[0];
    for row in GENERATION_PINS {
        if row.version != generation {
            continue;
        }
        for (name, read) in bound {
            let Some(declared) = read(row) else { continue };
            let compiled = compiled_contract_hash(name)
                .unwrap_or_else(|error| panic!("hash the vendored {name}: {error}"));
            assert_eq!(
                declared, compiled,
                "the {generation} row is the generation this tree vendors, and its {name} pin is \
                 not the {name} artifact committed beside it"
            );
        }
    }
}

/// Whether a set of declared pins equals the artifacts this build vendors, evaluated rather than
/// asserted -- so the check above and the drive below run the SAME code on different inputs.
fn pins_match_the_artifacts(declared: &[(&str, Option<String>)]) -> bool {
    declared.iter().all(|(name, value)| match value {
        // A field this generation does not carry cannot disagree with anything.
        None => true,
        Some(value) => compiled_contract_hash(name).ok().as_ref() == Some(value),
    })
}

/// The binding FAILS on a moved pin, driven through the same function the check uses.

/// Not a restatement beside it: an earlier form of this drive compared a mutated string with the
/// real hash and never called the predicate at all, so a predicate rewritten backwards would have
/// left both green. That is the shape this file exists to refuse.
#[test]
fn the_artifact_binding_fails_when_a_pin_is_moved() {
    let real = compiled_contract_hash("RootPN").expect("vendored RootPN hashes");
    let honest = vec![("RootPN", Some(real.clone())), ("PrivateNote", None)];
    assert!(
        pins_match_the_artifacts(&honest),
        "the real pin must be accepted, or the drive below proves nothing"
    );

    let moved = format!(
        "{}{}",
        &real[..63],
        if real.ends_with('0') { '1' } else { '0' }
    );
    assert_ne!(moved, real, "the mutation must actually change the value");
    assert_eq!(moved.len(), 64, "the mutation must stay a code hash");
    assert!(
        !pins_match_the_artifacts(&[("RootPN", Some(moved))]),
        "a moved RootPN pin was accepted as the vendored artifact's hash"
    );

    assert!(
        !pins_match_the_artifacts(&[("NotAContractThisTreeVendors", Some(real))]),
        "a pin for a contract this build does not carry was accepted"
    );
}
