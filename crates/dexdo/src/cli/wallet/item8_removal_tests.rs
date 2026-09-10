use super::*;
use crate::cli::wallet_funding::{
    store_funding_journal, FundingJournalRecord, FundingRequest, FundingState, HotBalanceReader,
    HotBalances,
};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::PathBuf;

const OLD_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NEW_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const OLD_HOT: &str = "4444444444444444444444444444444444444444444444444444444444444444::1111111111111111111111111111111111111111111111111111111111111111";
const NEW_HOT: &str = "4444444444444444444444444444444444444444444444444444444444444444::2222222222222222222222222222222222222222222222222222222222222222";

struct Fixture {
    _temp: tempfile::TempDir,
    store: WalletStore,
    old: WalletBinding,
    old_archive: PathBuf,
    old_secret: PathBuf,
    new_secret: PathBuf,
    active: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("item-8 wallet tree");
        let store = WalletStore::at(temp.path().join("wallet"));
        let old_secret = seed_secret_dir(&store, OLD_ID, b"old-secret-bytes");
        let new_secret = seed_secret_dir(&store, NEW_ID, b"new-secret-bytes");
        let old = binding(OLD_ID, OLD_HOT);
        let new = binding(NEW_ID, NEW_HOT);
        assert!(store.commit_active(&old).expect("commit old").is_none());
        let old_archive = store
            .commit_active(&new)
            .expect("commit replacement")
            .expect("old binding archived");
        let active = store.binding_path(&crate::cli::wallet::test_network_a());
        Self {
            _temp: temp,
            store,
            old,
            old_archive,
            old_secret,
            new_secret,
            active,
        }
    }

    fn refusal_snapshot(&self) -> Snapshot {
        Snapshot {
            archive: std::fs::read(&self.old_archive).expect("archive bytes"),
            old_secret: std::fs::read(&self.old_secret).expect("old secret bytes"),
            active: std::fs::read(&self.active).expect("active bytes"),
            new_secret: std::fs::read(&self.new_secret).expect("new secret bytes"),
        }
    }

    fn assert_refusal_unchanged(&self, before: &Snapshot) {
        assert_eq!(std::fs::read(&self.old_archive).unwrap(), before.archive);
        assert_eq!(std::fs::read(&self.old_secret).unwrap(), before.old_secret);
        assert_eq!(std::fs::read(&self.active).unwrap(), before.active);
        assert_eq!(std::fs::read(&self.new_secret).unwrap(), before.new_secret);
    }
}

struct Snapshot {
    archive: Vec<u8>,
    old_secret: Vec<u8>,
    active: Vec<u8>,
    new_secret: Vec<u8>,
}

fn binding(id: &str, hot_address: &str) -> WalletBinding {
    WalletBinding {
        network: crate::cli::wallet::test_network_a(),
        version: BINDING_VERSION,
        id: id.to_string(),
        provider: WalletProvider::Manual,
        hot_address: hot_address.to_string(),
        vault_address: None,
        hot_key_file: None,
        vault_key_file: None,
        hot_seed_file: None,
        push_profile_address: None,
    }
}

fn seed_secret_dir(store: &WalletStore, id: &str, bytes: &[u8]) -> PathBuf {
    let dir = store.bindings_dir().join(id);
    std::fs::create_dir_all(&dir).expect("create binding secrets dir");
    let secret = dir.join("secret.fixture");
    std::fs::write(&secret, bytes).expect("write binding secret fixture");
    secret
}

fn funding_record(
    binding: &WalletBinding,
    generation: u32,
    created_at: u64,
    amount: u128,
    state: FundingState,
) -> FundingJournalRecord {
    let hot = dexdo_core::CanonicalAddress::parse(&binding.hot_address).expect("canonical Hot");
    let shortfall = [(dexdo_core::params::SHELL_CURRENCY_ID, amount)]
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    let request = FundingRequest {
        provider: WalletProvider::AckinackiWallet,
        network: binding.network.as_str().to_string(),
        vault_address: Some(format!("{}::{}", "55".repeat(32), "55".repeat(32))),
        hot_address: hot.to_string(),
        hot_dapp_id: hot.dapp_id().to_string(),
        creator_pubkey: "66".repeat(32),
        required: shortfall.clone(),
        required_native: 0,
        shortfall,
        native_shortfall: 0,
    };
    let mut record = FundingJournalRecord::open_generation(&request, created_at, generation);
    record.state = state;
    record
}

enum ReadOutcome {
    Balances(HotBalances),
    Error(&'static str),
}

struct FixedReader {
    outcome: ReadOutcome,
    reads: Cell<usize>,
}

impl FixedReader {
    fn balances(native: u128, ecc: impl IntoIterator<Item = (u32, u128)>) -> Self {
        Self {
            outcome: ReadOutcome::Balances(HotBalances::new(native, ecc)),
            reads: Cell::new(0),
        }
    }

    fn error(message: &'static str) -> Self {
        Self {
            outcome: ReadOutcome::Error(message),
            reads: Cell::new(0),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl HotBalanceReader for FixedReader {
    async fn hot_balances(
        &self,
        _hot: &dexdo_core::CanonicalAddress,
    ) -> anyhow::Result<HotBalances> {
        self.reads.set(self.reads.get() + 1);
        match &self.outcome {
            ReadOutcome::Balances(balances) => Ok(balances.clone()),
            ReadOutcome::Error(message) => anyhow::bail!(*message),
        }
    }
}

async fn attempt(fixture: &Fixture, reader: &FixedReader) -> anyhow::Result<WalletBinding> {
    let target = fixture.store.archived_binding(OLD_ID)?;
    remove_archived_binding_after_balance_check(&fixture.store, &target, reader).await
}

#[tokio::test]
async fn all_zero_native_and_all_zero_ecc_remove_only_the_named_archive_and_secrets() {
    let fixture = Fixture::new();
    let active_before = std::fs::read(&fixture.active).unwrap();
    let new_secret_before = std::fs::read(&fixture.new_secret).unwrap();
    let reader = FixedReader::balances(0, [(2, 0), (17, 0)]);

    let removed = attempt(&fixture, &reader).await.expect("all-zero removal");

    assert_eq!(reader.reads.get(), 1);
    assert_eq!(removed.id, OLD_ID);
    assert!(!fixture.old_archive.exists());
    assert!(!fixture.old_secret.parent().unwrap().exists());
    assert_eq!(std::fs::read(&fixture.active).unwrap(), active_before);
    assert_eq!(
        std::fs::read(&fixture.new_secret).unwrap(),
        new_secret_before
    );
}

#[tokio::test]
async fn nonzero_native_refuses_and_preserves_archive_and_secrets_byte_for_byte() {
    let fixture = Fixture::new();
    let before = fixture.refusal_snapshot();
    let reader = FixedReader::balances(1, [(2, 0)]);

    let error = attempt(&fixture, &reader)
        .await
        .expect_err("native funds refuse");

    assert!(error.to_string().contains("native=1"), "{error:#}");
    assert_eq!(reader.reads.get(), 1);
    fixture.assert_refusal_unchanged(&before);
}

#[tokio::test]
async fn any_nonzero_ecc_refuses_and_preserves_archive_and_secrets_byte_for_byte() {
    let fixture = Fixture::new();
    let before = fixture.refusal_snapshot();
    let reader = FixedReader::balances(0, [(2, 0), (17, 9)]);

    let error = attempt(&fixture, &reader)
        .await
        .expect_err("ECC funds refuse");

    assert!(error.to_string().contains("ECC[17]=9"), "{error:#}");
    assert_eq!(reader.reads.get(), 1);
    fixture.assert_refusal_unchanged(&before);
}

#[tokio::test]
async fn balance_read_error_refuses_and_preserves_archive_and_secrets_byte_for_byte() {
    let fixture = Fixture::new();
    let before = fixture.refusal_snapshot();
    let reader = FixedReader::error("account absent or transport unavailable");

    let error = attempt(&fixture, &reader)
        .await
        .expect_err("unknown balances refuse");

    assert!(
        error.to_string().contains("nothing was removed"),
        "{error:#}"
    );
    assert_eq!(reader.reads.get(), 1);
    fixture.assert_refusal_unchanged(&before);
}

#[tokio::test]
async fn unusable_archived_hot_refuses_before_the_read_and_preserves_everything() {
    let fixture = Fixture::new();
    let mut archived = fixture.old.clone();
    archived.hot_address = "not-a-canonical-address".to_string();
    let mut archived_bytes = serde_json::to_vec_pretty(&archived).unwrap();
    archived_bytes.push(b'\n');
    std::fs::write(&fixture.old_archive, archived_bytes).expect("write unusable archived Hot");
    let before = fixture.refusal_snapshot();
    let reader = FixedReader::balances(0, []);

    let error = attempt(&fixture, &reader)
        .await
        .expect_err("unusable Hot refuses");

    assert!(
        error.to_string().contains("unusable Hot address"),
        "{error:#}"
    );
    assert_eq!(reader.reads.get(), 0);
    fixture.assert_refusal_unchanged(&before);
}

#[tokio::test]
async fn an_id_still_referenced_active_refuses_before_the_read_and_changes_nothing() {
    let fixture = Fixture::new();
    let mut old_bytes = serde_json::to_vec_pretty(&fixture.old).unwrap();
    old_bytes.push(b'\n');
    std::fs::write(&fixture.active, old_bytes).expect("duplicate archived id into active slot");
    let before = fixture.refusal_snapshot();
    let reader = FixedReader::balances(0, []);

    let error = attempt(&fixture, &reader)
        .await
        .expect_err("active reference refuses");

    assert!(error.to_string().contains("still referenced"), "{error:#}");
    assert_eq!(reader.reads.get(), 0);
    fixture.assert_refusal_unchanged(&before);
}

#[tokio::test]
async fn a_secret_directory_still_referenced_active_refuses_before_the_read() {
    let fixture = Fixture::new();
    let mut active = binding(NEW_ID, NEW_HOT);
    active.hot_key_file = Some(fixture.old_secret.clone());
    let mut active_bytes = serde_json::to_vec_pretty(&active).unwrap();
    active_bytes.push(b'\n');
    std::fs::write(&fixture.active, active_bytes).expect("point active record at old secret dir");
    let before = fixture.refusal_snapshot();
    let reader = FixedReader::balances(0, []);

    let error = attempt(&fixture, &reader)
        .await
        .expect_err("active secret reference refuses");

    assert!(error.to_string().contains("still referenced"), "{error:#}");
    assert_eq!(reader.reads.get(), 0);
    fixture.assert_refusal_unchanged(&before);
}

#[tokio::test]
async fn duplicate_archive_records_are_ambiguous_and_change_nothing() {
    let fixture = Fixture::new();
    let duplicate = fixture
        .store
        .archive_dir()
        .join(format!("duplicate-{OLD_ID}.json"));
    std::fs::copy(&fixture.old_archive, &duplicate).expect("duplicate archive record");
    let before = fixture.refusal_snapshot();
    let duplicate_before = std::fs::read(&duplicate).unwrap();
    let reader = FixedReader::balances(0, []);

    let error = attempt(&fixture, &reader)
        .await
        .expect_err("ambiguous archive refuses");

    assert!(error.to_string().contains("2 records"), "{error:#}");
    assert_eq!(reader.reads.get(), 0);
    fixture.assert_refusal_unchanged(&before);
    assert_eq!(std::fs::read(&duplicate).unwrap(), duplicate_before);
}

#[test]
fn an_older_live_request_blocks_removal_even_when_the_newest_generation_is_satisfied() {
    let fixture = Fixture::new();
    let now = 1_800_000_000;
    let mut live = funding_record(&fixture.old, 1, now, 100, FundingState::Submitted);
    live.pending_transaction_id = Some("7".to_string());
    store_funding_journal(fixture._temp.path(), &live).expect("store older live request");
    let satisfied = funding_record(&fixture.old, 2, now, 200, FundingState::Satisfied);
    store_funding_journal(fixture._temp.path(), &satisfied).expect("store newer satisfied request");

    let error = refuse_removal_while_funding_may_still_arrive_at(
        &fixture.old,
        fixture._temp.path(),
        &live.hot_address,
        now,
    )
    .expect_err("the older live generation still targets the archived Hot");

    assert!(error.to_string().contains("generation 1"), "{error:#}");
    assert!(error.to_string().contains("transaction is 7"), "{error:#}");
}

#[test]
fn a_submitted_request_blocks_through_its_deadline_and_not_one_second_later() {
    let fixture = Fixture::new();
    let created_at = 1_800_000_000;
    let submitted = funding_record(&fixture.old, 1, created_at, 100, FundingState::Submitted);
    store_funding_journal(fixture._temp.path(), &submitted).expect("store submitted request");

    let at_deadline = refuse_removal_while_funding_may_still_arrive_at(
        &fixture.old,
        fixture._temp.path(),
        &submitted.hot_address,
        submitted.expires_at_unix,
    );
    assert!(
        at_deadline.is_err(),
        "the request remains live at the canonical deadline"
    );

    refuse_removal_while_funding_may_still_arrive_at(
        &fixture.old,
        fixture._temp.path(),
        &submitted.hot_address,
        submitted.expires_at_unix.saturating_add(1),
    )
    .expect("a locally expired request no longer blocks removal");
}

#[test]
fn secrets_staging_failure_restores_the_archive_without_touching_keys() {
    let fixture = Fixture::new();
    let before = fixture.refusal_snapshot();
    let target = fixture.store.archived_binding(OLD_ID).unwrap();

    let error = fixture
        .store
        .remove_archived_binding_with_move(&target, |_source, _destination| {
            Err(std::io::Error::other("injected secrets move failure"))
        })
        .expect_err("second staging operation fails");

    assert!(error.to_string().contains("archive record was restored"));
    fixture.assert_refusal_unchanged(&before);
    assert!(!fixture
        .store
        .archive_dir()
        .join(format!(".remove-archived-{OLD_ID}.json"))
        .exists());
}

#[test]
fn no_removal_path_contains_a_chain_write_primitive() {
    let wallet = include_str!("../wallet.rs");
    let command = crate::cli::source_probe::code_of(wallet, "async fn run_remove_archived(args");
    let body = wallet
        .split_once("async fn remove_archived_binding_after_balance_check")
        .expect("removal boundary")
        .1
        .split_once("\n}\n")
        .expect("removal boundary end")
        .0;
    for forbidden in ["submit", ".call(", "send_message", "sendTransaction"] {
        assert!(
            !command.contains(forbidden) && !body.contains(forbidden),
            "remove-archived contains {forbidden}: command={command}; boundary={body}"
        );
    }
    assert!(command.contains("ChainClient::connect"));
    assert!(body.contains("hot_balances"));
}
