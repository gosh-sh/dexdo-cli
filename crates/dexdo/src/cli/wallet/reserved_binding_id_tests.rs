//! the id in `binding.json` is the id the store RESERVED, and the secrets it points at are
//! reachable through it.

//! # The defect these hold shut

//! A live `dexdo wallet onboard manual` printed one id and recorded another. The store reserved
//! `bca689...` and created `wallet/bindings/bca689.../` for it; the command printed that id and
//! that directory; and `binding.json` came out holding `01fb5703...`, an id with no directory
//! anywhere on disk. The active binding therefore named a directory that did not exist, while the
//! directory that did exist was referenced by nothing.

//! That is money-path damage, not a cosmetic mismatch. `binding-id` is minted before any key
//! precisely so that re-binding the same provider writes beside the previous binding's secrets
//! instead of over them, and so that the secrets of a Hot that still holds funds are reachable from
//! the record that names it. Two ids per onboarding voids both halves at once.

//! # Why the assertions are about the FILESYSTEM

//! Every check below resolves the id `binding.json` actually holds into a path and then asks the
//! filesystem whether that path is there. Comparing two strings, or asserting that both are
//! non-empty, is exactly the check the shipped code would have passed while stranding the key.

use super::store::new_binding_id;
use super::*;
use std::path::{Path, PathBuf};

fn store() -> (tempfile::TempDir, WalletStore) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = WalletStore::at(dir.path().join("wallet"));
    (dir, store)
}

/// The binding a provider flow returns, carrying whichever id it decided on.
fn binding_with(id: &str, key: Option<PathBuf>) -> WalletBinding {
    WalletBinding {
        network: crate::cli::wallet::test_network_a(),
        version: BINDING_VERSION,
        id: id.to_string(),
        provider: WalletProvider::Manual,
        hot_address: "4::0000000000000000000000000000000000000000000000000000000000000001"
            .to_string(),
        vault_address: None,
        hot_key_file: key,
        vault_key_file: None,
        hot_seed_file: None,
        push_profile_address: None,
    }
}

/// The directory `wallet/bindings/<id>` names, resolved from the id the FILE holds -- never from the
/// id the test happened to pass in.
fn recorded_secrets_dir(store: &WalletStore) -> (String, PathBuf) {
    let json: serde_json::Value = serde_json::from_slice(
        &std::fs::read(store.binding_path(&crate::cli::wallet::test_network_a()))
            .expect("read binding.json"),
    )
    .expect("binding.json is json");
    let id = json["id"]
        .as_str()
        .expect("binding.json has an id")
        .to_string();
    let dir = store.bindings_dir().join(&id);
    (id, dir)
}

fn recorded_key_file(store: &WalletStore) -> PathBuf {
    let json: serde_json::Value = serde_json::from_slice(
        &std::fs::read(store.binding_path(&crate::cli::wallet::test_network_a()))
            .expect("read binding.json"),
    )
    .expect("binding.json is json");
    PathBuf::from(
        json["hot_key_file"]
            .as_str()
            .expect("binding.json references a hot key file"),
    )
}

/// Write a key file into the directory the store reserved, which is what a flow that generates key
/// material does before the binding is committed.
fn key_in(dir: &Path) -> PathBuf {
    let path = dir.join("hot.key");
    std::fs::write(&path, b"cccccccccccccccccccccccccccccccc").expect("write the reserved key");
    path
}

/// The property the live run broke: the id `binding.json` holds names a directory that EXISTS, and
/// the key the binding references lives inside that directory.

/// Resolved from the file's own id and answered by the filesystem, so a binding recording a second
/// id fails here on the directory that is not there -- which is the exact shape of the defect.
#[test]
fn the_recorded_binding_id_names_the_reserved_directory_holding_its_key() {
    let (_temp, store) = store();
    let draft = store.open_draft().expect("reserve a draft");
    let key = key_in(draft.dir());

    commit_onboarded(&store, &draft, &binding_with(draft.id(), Some(key.clone())))
        .expect("a binding carrying the reserved id commits");

    let (recorded_id, secrets_dir) = recorded_secrets_dir(&store);
    assert!(
        secrets_dir.is_dir(),
        "binding.json records id {recorded_id}, whose secrets directory {} does not exist",
        secrets_dir.display()
    );
    assert_eq!(
        secrets_dir,
        draft.dir(),
        "the recorded id must resolve to the directory the store reserved for this attempt"
    );

    let recorded_key = recorded_key_file(&store);
    assert!(
        recorded_key.starts_with(&secrets_dir),
        "the referenced key {} is not inside the recorded binding's directory {}",
        recorded_key.display(),
        secrets_dir.display()
    );
    assert!(
        recorded_key.is_file(),
        "the referenced key {} is not on disk",
        recorded_key.display()
    );
    assert_eq!(recorded_key, key);
}

/// The mutation itself, driven through the real commit path: a flow that mints a SECOND id.

/// Nothing may be written. Recording that binding is what left the operator with a funded Hot whose
/// key the active binding could not reach, and the reserved directory -- which holds the key -- must
/// survive untouched so a retry still finds it.
#[test]
fn a_flow_that_mints_a_second_id_is_refused_and_writes_nothing() {
    let (_temp, store) = store();
    let draft = store.open_draft().expect("reserve a draft");
    let key = key_in(draft.dir());
    let second_id = new_binding_id();
    assert_ne!(second_id, draft.id(), "the mutation mints a different id");

    let error = commit_onboarded(&store, &draft, &binding_with(&second_id, Some(key.clone())))
        .expect_err("a binding whose id names no directory must not become the active one");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains(&second_id) && rendered.contains(draft.id()),
        "the refusal must name both the returned id and the reserved one: {rendered}"
    );

    assert!(
        store
            .load_active(&crate::cli::wallet::test_network_a())
            .expect("load")
            .is_none(),
        "nothing may be recorded when the id does not name the reserved directory"
    );
    assert!(
        draft.dir().is_dir() && key.is_file(),
        "the reserved directory and the key inside it must survive the refusal"
    );
    let orphan = store.bindings_dir().join(&second_id);
    assert!(
        !orphan.exists(),
        "the second id never had a directory; that is the whole defect"
    );
}

/// The same guarantee on a REBIND: a second id must not replace a good binding with one that points
/// nowhere. The previous binding stays active and stays resolvable as the funding wallet, which is
/// the property that keeps the old Hot spendable.
#[test]
fn a_second_id_on_a_rebind_leaves_the_previous_binding_active_and_resolvable() {
    let (temp, store) = store();
    let previous_key = temp.path().join("previous-hot.key");
    std::fs::write(&previous_key, b"cc").expect("write the previous key");
    let mut previous = binding_with(
        "00000000000000000000000000000000",
        Some(previous_key.clone()),
    );
    previous.hot_address = "4::hot-previous".to_string();
    // The secrets directory its id names, which every real binding has and which the reader has
    // required since.
    std::fs::create_dir_all(store.bindings_dir().join(&previous.id))
        .expect("create the previous binding's secrets directory");
    store
        .commit_active(&previous)
        .expect("commit the previous binding");

    let draft = store.open_draft().expect("reserve a draft");
    let key = key_in(draft.dir());
    commit_onboarded(&store, &draft, &binding_with(&new_binding_id(), Some(key)))
        .expect_err("a rebind to an id that names no directory must be refused");

    assert_eq!(
        store
            .load_active(&crate::cli::wallet::test_network_a())
            .expect("load")
            .expect("present"),
        previous,
        "the refusal must not replace the binding it was going to replace"
    );
    let resolved = resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        None,
        &None,
        &None,
    )
    .expect("the previous binding still resolves as the funding wallet");
    assert_eq!(resolved.address, "4::hot-previous");
    assert_eq!(resolved.key, Some(previous_key));
}

/// The manual provider's own half of the plumbing: the id it is HANDED is the id it records, minted
/// nowhere in between.

/// This is the function the live `wallet onboard manual` reaches with the draft's id, and it is the
/// point at which the flow used to substitute a fresh one.
#[test]
fn the_manual_provider_records_the_id_it_was_handed() {
    use crate::cli::wallet_manual::{
        verify_manual_hot_wallet, ManualSecretKind, ManualSecretRef, ObservedHotWallet,
    };

    const OWNER_PUBKEY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let (temp, store) = store();
    let draft = store.open_draft().expect("reserve a draft");
    let secret = temp.path().join("operator-hot.key");
    std::fs::write(&secret, b"cc").expect("write the operator secret");

    let verified = verify_manual_hot_wallet(
        "0000000000000000000000000000000000000000000000000000000000000004::\
         1111111111111111111111111111111111111111111111111111111111111111",
        crate::cli::wallet::test_network_a(),
        ManualSecretRef {
            kind: ManualSecretKind::Key,
            path: secret,
        },
        OWNER_PUBKEY,
        &ObservedHotWallet {
            status: "Active".to_string(),
            code_hash: Some(dexdo_core::canonical_multisig::CODE_HASH.to_string()),
            required_txn_confirms: 1,
            custodian_pubkeys: vec![format!("0x{OWNER_PUBKEY}")],
        },
        draft.id().to_string(),
    )
    .expect("a supported, single-confirmation, custodian-signed Hot verifies");

    assert_eq!(
        verified.binding().id,
        draft.id(),
        "the manual flow must record the reserved id verbatim, never one of its own"
    );
    commit_onboarded(&store, &draft, verified.binding())
        .expect("and the binding it produced therefore passes the commit guard");
    let (_, secrets_dir) = recorded_secrets_dir(&store);
    assert!(secrets_dir.is_dir(), "{}", secrets_dir.display());
}
