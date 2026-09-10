//! the READER of `binding.json` validates the id, so a record that names nothing can never
//! resolve as the funding wallet.

//! # The defect these hold shut

//! The stranded-binding fix guards the WRITE: one commit point refuses a binding whose id is not
//! the one the attempt reserved. Nothing guarded the READ. `load_active` checked `version` and
//! nothing else, so a `binding.json` whose id was empty, or was not the shape the store mints, or
//! named a `bindings/<id>/` directory that did not exist, deserialized cleanly and was handed
//! straight to `resolve_funding_wallet` as the wallet to spend from. The corrupt binding a live
//! `dexdo wallet onboard manual` had already produced on a real machine still loaded after the
//! write was guarded, because guarding the write cannot reach a file that is already on disk.

//! The id is the only route from the active record to that binding's secrets. In `manual` the key
//! is an external path the operator owns, so a broken id strands a reference. In `gosh-ai` the
//! recovery phrase is generated INTO `bindings/<id>/`, so a broken id strands the phrase, and with
//! it the funds.

//! # Why these assertions are what they are

//! Every case drives the REAL writer (`commit_active`) and then the REAL reader, rather than
//! hand-writing a file: a check that only ever sees hand-built JSON proves nothing about the path
//! the shipped binary takes. And each refusal is asserted to name the file and the remediation,
//! because an operator holding a corrupt binding needs to know which file and what to run -- the
//! whole point of refusing rather than resolving.

use super::*;
use std::path::{Path, PathBuf};

fn store() -> (tempfile::TempDir, WalletStore) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = WalletStore::at(dir.path().join("wallet"));
    (dir, store)
}

/// A binding carrying whatever id the case is about. Every other field is valid, so the id is the
/// only thing under test.
fn binding_with(id: &str) -> WalletBinding {
    WalletBinding {
        network: crate::cli::wallet::test_network_a(),
        version: BINDING_VERSION,
        id: id.to_string(),
        provider: WalletProvider::Manual,
        hot_address: "4::0000000000000000000000000000000000000000000000000000000000000001"
            .to_string(),
        vault_address: None,
        hot_key_file: Some(PathBuf::from("/tmp/does-not-matter.key")),
        vault_key_file: None,
        hot_seed_file: None,
        push_profile_address: None,
    }
}

/// Put a record on disk through the real writer, exactly as a flow that had minted a bad id would
/// have left it. `commit_active` does not validate on purpose -- that is what lets a corrupt record
/// be archived on the way out -- so this is the state the reader must cope with.
fn seed_record(store: &WalletStore, id: &str) -> WalletBinding {
    let binding = binding_with(id);
    store.commit_active(&binding).expect("seed the record");
    binding
}

/// Give a binding the secrets directory a healthy one has.
fn create_secrets_dir(store: &WalletStore, id: &str) {
    std::fs::create_dir_all(store.bindings_dir().join(id)).expect("create the secrets directory");
}

/// The one id shape the store mints: what `new_binding_id` produces.
const GOOD_ID: &str = "0123456789abcdef0123456789abcdef";

/// Every refusal has to be usable by the operator holding the broken file: it names the file, and
/// it names the command that gets them out.
fn assert_names_file_and_remediation(rendered: &str, store: &WalletStore) {
    let path = store.binding_path(&crate::cli::wallet::test_network_a());
    assert!(
        rendered.contains(&path.display().to_string()),
        "the refusal must name the file it refused: {rendered}"
    );
    assert!(
        rendered.contains("dexdo wallet rebind"),
        "the refusal must name the remediation: {rendered}"
    );
}

/// The positive control. Without this every assertion below could be satisfied by a reader that
/// refuses everything, which would be a worse defect than the one being fixed.
#[test]
fn a_binding_whose_id_names_its_secrets_directory_still_loads() {
    let (_temp, store) = store();
    let expected = seed_record(&store, GOOD_ID);
    create_secrets_dir(&store, GOOD_ID);

    let loaded = store
        .load_active(&crate::cli::wallet::test_network_a())
        .expect("a well-formed binding loads")
        .expect("present");
    assert_eq!(loaded, expected);

    let resolved = resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        None,
        &None,
        &None,
    )
    .expect("and it resolves as the funding wallet");
    assert_eq!(resolved.address, expected.hot_address);
}

/// An empty id names no directory at all. It is the degenerate case of the defect and the cheapest
/// to write by hand into `binding.json`.
#[test]
fn an_empty_binding_id_is_refused_on_load() {
    let (_temp, store) = store();
    seed_record(&store, "");

    let error = store
        .load_active(&crate::cli::wallet::test_network_a())
        .expect_err("a binding with no id must not load");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("empty binding id"),
        "the refusal must say which of the three rules failed: {rendered}"
    );
    assert_names_file_and_remediation(&rendered, &store);
}

/// Not the shape the store mints. Each entry is a different way to be wrong, and the last two are
/// the ones that reach a path: `..` and `/` are what turn this into a traversal, and they are
/// refused BEFORE the id is ever joined onto one.
#[test]
fn every_id_outside_the_minted_shape_is_refused_with_the_file_and_the_remediation() {
    for id in [
        "0123456789abcdef", // right alphabet, too short
        "0123456789abcdef0123456789abcdef0", // right alphabet, too long
        "0123456789ABCDEF0123456789ABCDEF", // uppercase: hex::encode never emits it
        "0123456789abcdef0123456789abcdeg", // right length, not hex
        "../../../../etc/dexdo-escaped-by-shape", // traversal
        "nested/id", // separator
    ] {
        let (_temp, store) = store();
        seed_record(&store, id);

        let error = store
            .load_active(&crate::cli::wallet::test_network_a())
            .expect_err(&format!("id {id:?} is not the shape the store mints"));
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("not an id this store mints"),
            "id {id:?}: the refusal must say which rule failed: {rendered}"
        );
        assert_names_file_and_remediation(&rendered, &store);
    }
}

/// The shape is right and the directory is not there. This is the exact record the live run left
/// behind: an id that looks entirely plausible and resolves to nothing.
#[test]
fn a_binding_id_naming_no_directory_is_refused_on_load() {
    let (_temp, store) = store();
    seed_record(&store, GOOD_ID);

    let error = store
        .load_active(&crate::cli::wallet::test_network_a())
        .expect_err("an id whose secrets directory is absent must not load");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains(GOOD_ID),
        "the refusal must name the id that resolved to nothing: {rendered}"
    );
    assert!(
        rendered.contains("does not exist"),
        "the refusal must say the directory is what is missing: {rendered}"
    );
    assert_names_file_and_remediation(&rendered, &store);
}

/// A FILE where the secrets directory should be is not a secrets directory. Without this the rule
/// could be satisfied by `exists()`, which a stray file passes.
#[test]
fn a_binding_id_naming_a_file_rather_than_a_directory_is_refused_on_load() {
    let (_temp, store) = store();
    seed_record(&store, GOOD_ID);
    let bindings = store.bindings_dir();
    std::fs::create_dir_all(&bindings).expect("bindings dir");
    std::fs::write(bindings.join(GOOD_ID), b"not a directory").expect("write the impostor");

    let error = store
        .load_active(&crate::cli::wallet::test_network_a())
        .expect_err("a file standing where the secrets directory belongs must not load");
    assert_names_file_and_remediation(&format!("{error:#}"), &store);
}

/// The defect as the operator meets it: the money path.

/// `resolve_funding_wallet` is what decides which Hot a spend signs from. Before this fix it
/// answered with a binding that named nothing.
#[test]
fn a_binding_that_names_nothing_never_resolves_as_the_funding_wallet() {
    let (_temp, store) = store();
    seed_record(&store, GOOD_ID);

    let error = resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        None,
        &None,
        &None,
    )
    .expect_err("a binding that names no secrets directory must not become the funding wallet");
    assert_names_file_and_remediation(&format!("{error:#}"), &store);
}

/// An explicit `--multisig-address` still wins, and is not made unreachable by a broken binding on
/// disk. The binding is not even read on that path, and it must stay that way: an operator whose
/// binding is corrupt can still spend by naming the wallet outright.
#[test]
fn an_explicit_address_still_wins_over_a_broken_binding() {
    let (_temp, store) = store();
    seed_record(&store, "");

    let resolved = resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        Some("4::explicit"),
        &None,
        &None,
    )
    .expect("an explicit address does not read the binding at all");
    assert_eq!(resolved.address, "4::explicit");
}

/// The remediation has to WORK. A validation that makes the corrupt record unreplaceable would be
/// worse than the defect: the operator's only way out is `rebind`, which archives this record and
/// writes one that resolves.

/// Driven through `commit_active`, which is the writer `rebind` commits through.
#[test]
fn a_corrupt_binding_can_still_be_archived_and_replaced() {
    let (_temp, store) = store();
    seed_record(&store, "");

    let replacement = binding_with(GOOD_ID);
    let archived = store
        .commit_active(&replacement)
        .expect("replacing a corrupt binding must not be blocked by its own corruption")
        .expect("the corrupt record is archived, not dropped");
    assert!(
        archived.is_file(),
        "the replaced record must be on disk at {}",
        archived.display()
    );
    let archived_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&archived).expect("read the archive"))
            .expect("archive is json");
    assert_eq!(
        archived_json["id"].as_str(),
        Some(""),
        "the archived record keeps the id it actually had, however broken"
    );

    create_secrets_dir(&store, GOOD_ID);
    assert_eq!(
        store
            .load_active(&crate::cli::wallet::test_network_a())
            .expect("load")
            .expect("present"),
        replacement,
        "and the replacement is what loads afterwards"
    );
}

/// The path traversal that follows from the same gap: the archive filename used to be built by
/// interpolating the previous record's id, which nothing had validated.

/// The assertion is about the FILESYSTEM outside the archive directory, not about the string: a
/// test that only inspected the name would pass against a writer that still escaped.
#[test]
fn an_id_holding_a_traversal_cannot_write_the_archive_outside_the_archive_directory() {
    let (temp, store) = store();
    let escape_target = temp.path().join("escaped");
    std::fs::create_dir_all(&escape_target).expect("somewhere to escape to");
    seed_record(&store, "../../escaped/pwned");

    store
        .commit_active(&binding_with(GOOD_ID))
        .expect("the replacement still commits");

    assert!(
        read_dir_names(&escape_target).is_empty(),
        "the archive escaped into {}: {:?}",
        escape_target.display(),
        read_dir_names(&escape_target)
    );
    let archive_dir = store.archive_dir();
    let archived = read_dir_names(&archive_dir);
    assert_eq!(
        archived.len(),
        1,
        "exactly one record was replaced, so exactly one archive file: {archived:?}"
    );
    assert!(
        !archived[0].contains(".."),
        "the untrusted id must not reach the filename at all: {archived:?}"
    );
}

/// Two records with unusable ids replaced in the same second must not land on the same archive
/// file. Dropping the id from the name is only safe if what replaces it is still distinct --
/// otherwise the second archive silently overwrites the first, which is the record-losing failure
/// archiving exists to prevent.
#[test]
fn two_unusable_ids_archived_in_the_same_second_do_not_overwrite_each_other() {
    let (_temp, store) = store();
    seed_record(&store, "first/bad");
    store
        .commit_active(&binding_with("second/bad"))
        .expect("replace the first");
    store
        .commit_active(&binding_with(GOOD_ID))
        .expect("replace the second");

    let archived = read_dir_names(&store.archive_dir());
    assert_eq!(
        archived.len(),
        2,
        "two records were replaced, so two archive files must exist: {archived:?}"
    );
}

fn read_dir_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| Some(entry.ok()?.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort();
    names
}
