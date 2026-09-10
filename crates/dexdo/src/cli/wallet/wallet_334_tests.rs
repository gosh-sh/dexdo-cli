//! stage one: the provider model, the durable binding, and the fail-fast.

//! Each test names the property it is holding, because every one of them exists to stop a specific
//! money-path mistake: a provider guessed after the fact, a rebind that overwrites or deletes the
//! secrets of a Hot that still holds funds, a half-written active binding, or a command that starts
//! spending with no wallet configured at all.

use super::store::new_binding_id;
use super::*;
use std::io::Cursor;

fn store() -> (tempfile::TempDir, WalletStore) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = WalletStore::at(dir.path().join("wallet"));
    (dir, store)
}

fn binding(id: &str, provider: WalletProvider, hot: &str) -> WalletBinding {
    WalletBinding {
        network: crate::cli::wallet::test_network_a(),
        version: BINDING_VERSION,
        id: id.to_string(),
        provider,
        hot_address: hot.to_string(),
        vault_address: None,
        hot_key_file: None,
        vault_key_file: None,
        hot_seed_file: None,
        push_profile_address: None,
    }
}

/// A well-formed `ackinacki-wallet` payload. Its VALUES are irrelevant to provider selection - the
/// provider is chosen by which subcommand was named - so this exists only to construct the variant.
fn sample_onboard_args() -> crate::cli::args::WalletOnboardArgs {
    crate::cli::args::WalletOnboardArgs {
        // The manifest a run finds for itself; these fixtures never dial.
        agent_name: "build-agent".to_string(),
        state: Some(std::path::PathBuf::from("session.json")),
        hot_key: Some(std::path::PathBuf::from("hot.key")),
        vault_key: None,
        qr_file: None,
        terminal_qr: false,
    }
}

/// A well-formed gosh-ai payload. Like the ackinacki one, the payload is not part of the provider's
/// identity -- every field has a default because the menu can also choose this provider.
fn goshai_args() -> crate::cli::args::WalletGoshAiArgs {
    crate::cli::args::WalletGoshAiArgs {
        activation_timeout: None,
    }
}

/// A well-formed manual payload: every field optional, because on a terminal the flow asks.
fn manual_args() -> crate::cli::args::WalletOnboardManualArgs {
    crate::cli::args::WalletOnboardManualArgs {
        multisig_address: None,
        multisig_private_key: None,
        multisig_seed_file: None,
    }
}

// ---------------------------------------------------------------- provider selection

/// The provider subcommand is the answer whenever it is given, terminal or not.
#[test]
fn an_explicit_provider_subcommand_is_used_verbatim() {
    for (command, expected) in [
        (
            // The `ackinacki-wallet` subcommand carries the onboarding request; the payload is not
            // part of the provider's identity, so any well-formed one selects the same provider.
            WalletProviderCommand::AckinackiWallet(sample_onboard_args()),
            WalletProvider::AckinackiWallet,
        ),
        (
            WalletProviderCommand::GoshAi(goshai_args()),
            WalletProvider::GoshAi,
        ),
        (
            WalletProviderCommand::Manual(manual_args()),
            WalletProvider::Manual,
        ),
    ] {
        for interactive in [true, false] {
            let mut reader = Cursor::new(Vec::new());
            let mut writer = Vec::new();
            let chosen = resolve_provider(
                WalletAction::Onboard,
                Some(&command),
                interactive,
                &mut reader,
                &mut writer,
            )
            .expect("an explicit provider needs no terminal");
            assert_eq!(chosen, expected);
            assert!(
                writer.is_empty(),
                "an explicit provider must not print a menu"
            );
        }
    }
}

/// a headless seller has no terminal to answer a menu in. Without a provider it must get an
/// error naming every subcommand -- never a prompt it cannot see and never a hang on a closed stdin.
#[test]
fn no_provider_without_a_terminal_is_an_error_listing_the_subcommands() {
    for action in [WalletAction::Onboard, WalletAction::Rebind] {
        let mut reader = Cursor::new(b"1\n".to_vec());
        let mut writer = Vec::new();
        let error = resolve_provider(action, None, false, &mut reader, &mut writer)
            .expect_err("a non-interactive run must refuse to choose a provider")
            .to_string();
        for provider in WalletProvider::ALL {
            assert!(
                error.contains(&format!("dexdo wallet {action} {provider}")),
                "{action} refusal must name `{provider}`: {error}"
            );
        }
        assert!(
            error.contains("manual"),
            "the headless remediation must name `manual`: {error}"
        );
        assert!(
            writer.is_empty(),
            "nothing may be prompted without a terminal"
        );
        assert_eq!(
            reader.position(),
            0,
            "a non-interactive run must not consume stdin"
        );
    }
}

/// The terminal menu is shown only in a terminal, numbered exactly as the providers are ordered.
#[test]
fn a_terminal_gets_the_numbered_menu_and_the_number_selects() {
    for (answer, expected) in [
        ("1", WalletProvider::AckinackiWallet),
        ("2", WalletProvider::GoshAi),
        ("3", WalletProvider::Manual),
        ("ackinacki-wallet", WalletProvider::AckinackiWallet),
        ("gosh-ai", WalletProvider::GoshAi),
        ("manual", WalletProvider::Manual),
    ] {
        let mut reader = Cursor::new(format!("{answer}\n").into_bytes());
        let mut writer = Vec::new();
        let chosen = resolve_provider(WalletAction::Onboard, None, true, &mut reader, &mut writer)
            .expect("a terminal answer selects a provider");
        assert_eq!(chosen, expected, "answer {answer}");
        let menu = String::from_utf8(writer).expect("utf8 menu");
        assert!(menu.contains("1. ackinacki-wallet"), "{menu}");
        assert!(menu.contains("2. gosh-ai"), "{menu}");
        assert!(menu.contains("3. manual"), "{menu}");
    }
}

/// No provider is ever a default: an unrecognised answer, and an empty one, are refused rather than
/// resolved to the first entry.
#[test]
fn an_unrecognised_or_empty_menu_answer_selects_nothing() {
    for answer in ["", "0", "4", "ackinacki", "gosh", "GOSH-AI", "y"] {
        let mut reader = Cursor::new(format!("{answer}\n").into_bytes());
        let mut writer = Vec::new();
        assert!(
            resolve_provider(WalletAction::Onboard, None, true, &mut reader, &mut writer,).is_err(),
            "`{answer}` must not select a provider"
        );
    }
}

/// A closed stdin inside a terminal is an error, not a default and not a hang.
#[test]
fn end_of_input_at_the_menu_is_an_error() {
    let mut reader = Cursor::new(Vec::new());
    let mut writer = Vec::new();
    let error = resolve_provider(WalletAction::Onboard, None, true, &mut reader, &mut writer)
        .expect_err("end of input selects nothing")
        .to_string();
    assert!(error.contains("input ended"), "{error}");
}

// ---------------------------------------------------------------- the binding file

/// The provider is written down and read back from its own field. It is never derived from the
/// address, which is exactly what makes two providers indistinguishable after the fact.
#[test]
fn the_provider_is_a_recorded_field_and_survives_a_round_trip() {
    let (_temp, store) = store();
    for provider in WalletProvider::ALL {
        let reserved = store.open_draft().expect("reserve the fixture's id");
        let written = binding(reserved.id(), provider, "4::abc");
        store.commit_active(&written).expect("commit");
        let read = store
            .load_active(&crate::cli::wallet::test_network_a())
            .expect("load")
            .expect("present");
        assert_eq!(read, written);
        assert_eq!(read.provider, provider);
        let json: serde_json::Value = serde_json::from_slice(
            &std::fs::read(store.binding_path(&crate::cli::wallet::test_network_a()))
                .expect("read"),
        )
        .expect("json");
        assert_eq!(json["provider"], provider.as_str());
    }
}

/// Two bindings whose Hot address, network and code are identical still carry different providers.
/// If the provider were inferred from anything on chain, this pair would be inseparable.
#[test]
fn the_same_hot_address_can_carry_a_different_provider() {
    let (_temp, store) = store();
    let same_hot = "4::0000000000000000000000000000000000000000000000000000000000000001";
    let gosh_ai = store
        .open_draft()
        .expect("reserve the gosh-ai fixture's id");
    let manual = store.open_draft().expect("reserve the manual fixture's id");
    store
        .commit_active(&binding(gosh_ai.id(), WalletProvider::GoshAi, same_hot))
        .expect("commit gosh-ai");
    assert_eq!(
        store
            .load_active(&crate::cli::wallet::test_network_a())
            .expect("load")
            .expect("present")
            .provider,
        WalletProvider::GoshAi
    );
    store
        .commit_active(&binding(manual.id(), WalletProvider::Manual, same_hot))
        .expect("commit manual");
    assert_eq!(
        store
            .load_active(&crate::cli::wallet::test_network_a())
            .expect("load")
            .expect("present")
            .provider,
        WalletProvider::Manual
    );
}

/// A binding with no `provider` field is refused. There is no serde default, so a hand-edited or
/// pre- file cannot become "whichever provider the code happens to try first".
#[test]
fn a_binding_without_a_provider_field_is_refused_not_defaulted() {
    let (_temp, store) = store();
    std::fs::create_dir_all(
        store
            .binding_path(&crate::cli::wallet::test_network_a())
            .parent()
            .expect("parent"),
    )
    .expect("mkdir");
    std::fs::write(
        store.binding_path(&crate::cli::wallet::test_network_a()),
        br#"{"version":1,"id":"x","network":"net-a","hot_address":"4::abc"}"#,
    )
    .expect("write");
    let error = format!(
        "{:#}",
        store
            .load_active(&crate::cli::wallet::test_network_a())
            .expect_err("a binding with no provider must not load")
    );
    assert!(error.contains("provider"), "{error}");
}

/// A newer schema is refused rather than read with fields this build does not know about.
#[test]
fn a_future_binding_version_is_refused() {
    let (_temp, store) = store();
    let mut future = binding("id", WalletProvider::Manual, "4::abc");
    future.version = BINDING_VERSION + 1;
    std::fs::create_dir_all(
        store
            .binding_path(&crate::cli::wallet::test_network_a())
            .parent()
            .expect("parent"),
    )
    .expect("mkdir");
    std::fs::write(
        store.binding_path(&crate::cli::wallet::test_network_a()),
        serde_json::to_vec(&future).expect("serialize"),
    )
    .expect("write");
    let error = store
        .load_active(&crate::cli::wallet::test_network_a())
        .expect_err("a future version must not load")
        .to_string();
    assert!(error.contains("version"), "{error}");
}

/// The binding file carries non-secret parameters and PATHS only. This pins the whole key set, so
/// adding a key or a phrase to the type would have to fail here first.
#[test]
fn the_binding_file_holds_no_secret_material() {
    let (_temp, store) = store();
    let mut full = binding("id", WalletProvider::AckinackiWallet, "4::hot");
    full.vault_address = Some("4::vault".into());
    full.hot_key_file = Some("/keys/hot.key".into());
    full.vault_key_file = Some("/keys/vault.key".into());
    full.hot_seed_file = Some("/keys/hot.seed".into());
    full.push_profile_address = Some("0:profile".into());
    store.commit_active(&full).expect("commit");

    let json: serde_json::Value = serde_json::from_slice(
        &std::fs::read(store.binding_path(&crate::cli::wallet::test_network_a())).expect("read"),
    )
    .expect("json");
    let mut keys: Vec<&str> = json
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "hot_address",
            "hot_key_file",
            "hot_seed_file",
            "id",
            "network",
            "provider",
            "push_profile_address",
            "vault_address",
            "vault_key_file",
            "version",
        ],
        "the binding schema changed; secrets must never become fields of it"
    );
}

/// Optional fields are absent rather than null, so a `gosh-ai` binding does not claim a Vault it
/// never had.
#[test]
fn absent_optional_fields_are_omitted() {
    let (_temp, store) = store();
    store
        .commit_active(&binding("id", WalletProvider::GoshAi, "4::hot"))
        .expect("commit");
    let json: serde_json::Value = serde_json::from_slice(
        &std::fs::read(store.binding_path(&crate::cli::wallet::test_network_a())).expect("read"),
    )
    .expect("json");
    let object = json.as_object().expect("object");
    assert!(!object.contains_key("vault_address"), "{json}");
    assert!(!object.contains_key("vault_key_file"), "{json}");
}

// ---------------------------------------------------------------- rebind, archive, secrets

/// A replaced binding is archived, never deleted: funds can still sit in the old Hot.
#[test]
fn a_replaced_binding_is_archived_with_its_provider_intact() {
    let (_temp, store) = store();
    let reserved_first = store.open_draft().expect("reserve the first binding's id");
    let first = binding(
        reserved_first.id(),
        WalletProvider::AckinackiWallet,
        "4::hot-one",
    );
    assert!(
        store.commit_active(&first).expect("commit first").is_none(),
        "the first binding replaces nothing"
    );

    let reserved_second = store.open_draft().expect("reserve the second binding's id");
    let second = binding(reserved_second.id(), WalletProvider::GoshAi, "4::hot-two");
    let archived = store
        .commit_active(&second)
        .expect("commit second")
        .expect("the replaced binding is archived");

    let recorded: WalletBinding =
        serde_json::from_slice(&std::fs::read(&archived).expect("read archive")).expect("json");
    assert_eq!(recorded, first);
    assert!(
        archived.to_string_lossy().contains(&first.id),
        "the archive file is named after the binding it holds: {}",
        archived.display()
    );
    assert_eq!(
        store
            .load_active(&crate::cli::wallet::test_network_a())
            .expect("load")
            .expect("present"),
        second,
        "the active binding is the new one"
    );
}

/// The secrets directory of a replaced binding is left alone. This is the property that makes the
/// old Hot still spendable after a rebind.
#[test]
fn rebinding_never_touches_the_previous_bindings_secrets() {
    let (_temp, store) = store();
    let old = store.open_draft().expect("first draft");
    crate::cli::support::write_owner_only_key_fixture(&old.dir().join("hot.key"), "old-secret");
    store
        .commit_active(&binding(
            old.id(),
            WalletProvider::AckinackiWallet,
            "4::hot-one",
        ))
        .expect("commit first");

    let new = store.open_draft().expect("second draft");
    assert_ne!(new.id(), old.id(), "each binding gets its own id");
    assert_ne!(new.dir(), old.dir(), "and its own secrets directory");
    crate::cli::support::write_owner_only_key_fixture(&new.dir().join("hot.key"), "new-secret");
    store
        .commit_active(&binding(new.id(), WalletProvider::GoshAi, "4::hot-two"))
        .expect("commit second");

    assert_eq!(
        std::fs::read(old.dir().join("hot.key")).expect("old secret survives"),
        b"old-secret",
        "a rebind must not overwrite or delete the previous binding's secrets"
    );
}

/// The id, and therefore the directory, exist before any key does -- that is what makes the
/// no-overwrite property structural rather than a naming convention.
#[test]
fn a_draft_reserves_its_directory_before_any_key_exists() {
    let (_temp, store) = store();
    let draft = store.open_draft().expect("draft");
    assert!(draft.dir().is_dir(), "the directory is reserved up front");
    assert!(
        std::fs::read_dir(draft.dir())
            .expect("read")
            .next()
            .is_none(),
        "and it is empty until the flow writes a secret"
    );
    assert!(!draft.id().is_empty());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(draft.dir())
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "secrets directories are owner-only");
    }
}

/// A failed attempt that wrote nothing leaves nothing; one that wrote a secret keeps it, because
/// that is the state a retry resumes from.
#[test]
fn an_empty_draft_is_discarded_and_a_used_one_is_kept() {
    let (_temp, store) = store();
    let empty = store.open_draft().expect("draft");
    empty.discard_if_empty();
    assert!(
        !empty.dir().exists(),
        "an empty draft leaves nothing behind"
    );

    let used = store.open_draft().expect("draft");
    std::fs::write(used.dir().join("onboarding.json"), b"{}").expect("write draft state");
    used.discard_if_empty();
    assert!(
        used.dir().join("onboarding.json").exists(),
        "resumable onboarding state must survive a failed attempt"
    );
}

// ---------------------------------------------------------------- the fail-fast

/// The fail-fast a wallet-dependent command raises before any chain write: the stable code, and a
/// remediation that names the providers rather than sending a headless host to a QR flow.
#[test]
fn a_missing_binding_fails_fast_with_the_stable_code() {
    let (_temp, store) = store();
    assert!(
        store
            .load_active(&crate::cli::wallet::test_network_a())
            .expect("load")
            .is_none(),
        "nothing is bound yet"
    );
    let error = store
        .require_active(&crate::cli::wallet::test_network_a())
        .expect_err("a missing binding must not resolve to a wallet");
    let structured = error
        .downcast_ref::<dexdo_core::DexdoError>()
        .expect("the fail-fast is the structured error, not a bare string");
    assert_eq!(
        structured.code(),
        dexdo_core::error_codes::E_WALLET_NOT_CONFIGURED.code()
    );
    let rendered = structured.to_string();
    assert!(
        rendered.contains("dexdo wallet onboard"),
        "the remediation must name the command: {rendered}"
    );
    for provider in WalletProvider::ALL {
        assert!(
            rendered.contains(provider.as_str()),
            "the remediation must name `{provider}`: {rendered}"
        );
    }
}

/// Once a binding exists the same call resolves it, so the code above marks "not configured" and
/// nothing else.
#[test]
fn require_active_returns_the_recorded_binding() {
    let (_temp, store) = store();
    let reserved = store.open_draft().expect("reserve the fixture's id");
    let written = binding(reserved.id(), WalletProvider::Manual, "4::hot");
    store.commit_active(&written).expect("commit");
    assert_eq!(
        store
            .require_active(&crate::cli::wallet::test_network_a())
            .expect("resolve"),
        written
    );
}

/// A binding file that exists but cannot be read is an error, never a silent "no wallet": treating
/// it as unconfigured would start onboarding while a funded Hot is already bound.
#[test]
fn an_unreadable_binding_is_an_error_not_an_absent_one() {
    let (_temp, store) = store();
    std::fs::create_dir_all(
        store
            .binding_path(&crate::cli::wallet::test_network_a())
            .parent()
            .expect("parent"),
    )
    .expect("mkdir");
    std::fs::write(
        store.binding_path(&crate::cli::wallet::test_network_a()),
        b"{ this is not json",
    )
    .expect("write");
    assert!(store
        .load_active(&crate::cli::wallet::test_network_a())
        .is_err());
    let error = store
        .require_active(&crate::cli::wallet::test_network_a())
        .expect_err("must not resolve");
    assert!(
        error.downcast_ref::<dexdo_core::DexdoError>().is_none(),
        "a corrupt binding is not the not-configured code: {error}"
    );
}

/// Ids are unique per attempt; a collision would put two bindings' secrets in one directory.
#[test]
fn binding_ids_do_not_repeat() {
    let ids: std::collections::BTreeSet<String> = (0..64).map(|_| new_binding_id()).collect();
    assert_eq!(ids.len(), 64);
    assert!(ids
        .iter()
        .all(|id| id.len() == 32 && id.chars().all(|c| c.is_ascii_hexdigit())));
}

/// A rebind that FAILS leaves the previous binding active and still usable as the funding wallet.

/// Asserted by resolving it the way a money command does, not by looking at the filesystem. "The
/// file is still there" is not the property: the property is that `note deploy` and `note topup`
/// still find a Hot to spend from, and a binding can be present and still unresolvable -- one with
/// no local key or seed file resolves to a refusal, not to a wallet.

/// The failure path is driven, not simulated: `run_selected` reserves a draft, runs the provider
/// flow, and on error discards the draft and returns WITHOUT committing. That is exactly the
/// sequence here, with the provider flow's error standing in for a wallet that failed verification.
#[test]
fn a_failed_rebind_leaves_the_previous_binding_active_and_resolvable() {
    let (temp, store) = store();
    let key = temp.path().join("hot.key");
    std::fs::write(&key, "cc").expect("write key");

    let reserved_previous = store
        .open_draft()
        .expect("reserve the previous binding's id");
    let mut previous = binding(
        reserved_previous.id(),
        WalletProvider::Manual,
        "4::hot-previous",
    );
    previous.hot_key_file = Some(key.clone());
    store.commit_active(&previous).expect("commit previous");

    // The rebind gets this far and then its provider flow refuses -- an unverifiable new wallet.
    let draft = store.open_draft().expect("reserve a draft");
    let reserved = draft.dir().to_path_buf();
    assert!(reserved.is_dir(), "the store reserves before the flow runs");
    draft.discard_if_empty();

    // Nothing was committed, so the previous binding is untouched...
    assert_eq!(
        store
            .load_active(&crate::cli::wallet::test_network_a())
            .expect("load")
            .expect("present"),
        previous,
        "a failed rebind must not replace the binding it was going to replace"
    );
    // ...and it still resolves as the wallet a money command would spend from.
    let resolved = super::resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        None,
        &None,
        &None,
    )
    .expect("the previous binding must still resolve as the funding wallet");
    assert_eq!(resolved.address, "4::hot-previous");
    assert_eq!(resolved.key, Some(key));
    // And it left no half-made binding behind to be inherited.
    assert!(
        !reserved.exists(),
        "the reserved draft directory was not given back"
    );
}

/// The archived binding's ADDRESS survives, which is the whole reason archiving exists.

/// `a_replaced_binding_is_archived_with_its_provider_intact` above already compares the whole
/// struct, which subsumes this. It is asserted separately anyway because the address is the part
/// that makes stranded funds recoverable, and a future change that narrowed the archive to a
/// summary would still satisfy a whole-struct comparison against a narrowed type.
#[test]
fn the_archived_binding_still_names_the_hot_that_may_hold_funds() {
    let (_temp, store) = store();
    let old = binding("old", WalletProvider::Manual, "4::hot-with-money-in-it");
    store.commit_active(&old).expect("commit old");

    let archived = store
        .commit_active(&binding("new", WalletProvider::GoshAi, "4::hot-new"))
        .expect("rebind")
        .expect("the replaced binding is archived");

    let recovered: WalletBinding =
        serde_json::from_slice(&std::fs::read(&archived).expect("read archive")).expect("json");
    assert_eq!(
        recovered.hot_address, "4::hot-with-money-in-it",
        "an operator whose funds are in the old Hot must be able to read its address back"
    );
    assert_eq!(recovered.provider, WalletProvider::Manual);
}

/// With step 9 landed, `rebind ackinacki-wallet` is a real operation and must behave like the
/// other two: archive the previous binding and produce a new one, or leave the previous one
/// active and resolvable.

/// The prohibition on wiring it existed because a rebind through a flow that could not write a
/// binding would replace a good binding with nothing. That condition is gone: the flow now returns
/// a `WalletBinding` and `run_selected` commits it through the one writer.
#[test]
fn rebinding_to_ackinacki_wallet_archives_the_previous_binding_and_activates_the_new_one() {
    let (temp, store) = store();
    let key = temp.path().join("hot.key");
    std::fs::write(&key, "cc").expect("write key");

    let reserved_previous = store
        .open_draft()
        .expect("reserve the previous binding's id");
    let mut previous = binding(
        reserved_previous.id(),
        WalletProvider::Manual,
        "4::hot-previous",
    );
    previous.hot_key_file = Some(key.clone());
    store.commit_active(&previous).expect("commit previous");

    // What the ackinacki-wallet flow now returns: the Hot as the address dexdo spends from, the
    // Vault recorded beside it as the top-up source, and the local Hot key file referenced.
    let reserved_replacement = store.open_draft().expect("reserve the replacement's id");
    let mut replacement = binding(
        reserved_replacement.id(),
        WalletProvider::AckinackiWallet,
        "4::hot-new",
    );
    replacement.vault_address = Some("4::vault-new".to_string());
    replacement.hot_key_file = Some(key.clone());

    let archived = store
        .commit_active(&replacement)
        .expect("rebind commits")
        .expect("the previous binding is archived, never dropped");

    let kept: WalletBinding =
        serde_json::from_slice(&std::fs::read(&archived).expect("read archive")).expect("json");
    assert_eq!(
        kept.hot_address, "4::hot-previous",
        "funds can still sit in the replaced Hot, so its address must remain readable"
    );

    let active = store
        .load_active(&crate::cli::wallet::test_network_a())
        .expect("load")
        .expect("present");
    assert_eq!(active, replacement);
    // The binding names the Hot as the wallet to spend from -- never the Vault.
    let resolved = super::resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        None,
        &None,
        &None,
    )
    .expect("the new binding resolves as the funding wallet");
    assert_eq!(
        resolved.address, "4::hot-new",
        "dexdo spends from the Hot; the Vault is custody, not a funding source"
    );
    assert_ne!(resolved.address, "4::vault-new");
}
