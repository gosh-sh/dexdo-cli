//! Where the client keeps a secret: the operating system's store where there is one, and the store
//! the client raises for itself where there is not.

//! **The second branch is not a footnote.** A seller runs on a headless server. Secret Service needs
//! a session bus and a running agent, and a server has neither; a Windows service account and a
//! macOS box with no logged-in user are the same story. If the fallback were an afterthought, the
//! machine this product actually earns money on would be the machine running the untested path. So
//! there are two branches and both are ordinary: a system store when the platform has one that keeps
//! a secret until something deletes it, and an owner-only file when it does not.

//! **What decides, and why it is not the platform name.** A store is taken only when it reports
//! [`keyring::credential::CredentialPersistence::UntilDelete`]. The check is not decoration: with no
//! backend feature selected, `keyring` resolves to an in-process mock whose `set_password` SUCCEEDS
//! and whose contents vanish with the process. Choosing by platform name would have written a note
//! owner key into that mock and reported success. Asking the store how long it keeps things is the
//! only question whose answer cannot be wrong for the machine it is asked on.

//! **Nothing that already has a key on disk loses it.** A read on the system branch falls through to
//! the file, so a key written by an older client is still found. Only the file branch is total, and
//! only because it has to be: `DEXDO_SECRET_STORE=file` exists so this path can be reached
//! deliberately on a workstation whose keychain would otherwise answer every time, and a "force"
//! that quietly consults the other store proves nothing.

//! **The secret's value is never rendered.** Not in `Debug`, not in an error, not in a log. Errors
//! name the operation and the secret's file path, which is what an operator needs and is not
//! secret. `keyring`'s own error text is deliberately NOT propagated for four of its variants:
//! `Error::Ambiguous` renders `{items:?}` over the matching credentials, and the mock credential
//! this crate falls back to derives `Debug` over a struct that holds the password. Only
//! `PlatformFailure` and `NoStorageAccess` carry their text through -- those two wrap an error the
//! platform produced, and the platform is not handed the secret to describe.

use anyhow::{bail, Context as _, Result};
use dexdo_core::params::{SECRET_STORE_SERVICE, SECRET_STORE_VAR};
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

/// Which store this process writes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backend {
    /// The operating system's own: Keychain on macOS, Credential Manager on Windows.
    System,
    /// The one the client raises for itself: a file only its owner can read.
    File,
}

/// One secret, addressed the same way in both stores.

/// The file path IS the identity. Every reference to a secret in this client is already a path -- a
/// command-line flag, a record that names where a key lives, a refusal that tells an operator which
/// file to fix -- so giving the system store a second, separate naming scheme would have created two
/// names for one thing and a way for them to disagree. The system store holds the value under an
/// account named by that same path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SecretName {
    file: PathBuf,
}

impl SecretName {
    /// The secret the client's own store would keep at `file`.
    pub(crate) fn at(file: impl Into<PathBuf>) -> Self {
        Self { file: file.into() }
    }

    /// Where the client's own store keeps it.
    pub(crate) fn file(&self) -> &Path {
        &self.file
    }

    /// The account the system store holds it under.
    fn account(&self) -> String {
        self.file.display().to_string()
    }
}

/// A secret store the operating system provides.

/// A trait rather than a direct call into `keyring`, for one reason: the branch that TAKES a system
/// store cannot otherwise be exercised on the machine that runs the tests. Continuous integration
/// here is Linux in a container -- no session bus, no keychain, no agent -- so without this seam the
/// system branch would be reached for the first time on a user's laptop. What the seam covers is
/// this module's own decisions (which store is chosen, what a read falls through to, whether a
/// forced branch stays forced); `keyring`'s correctness is `keyring`'s.
pub(crate) trait SystemStore: Send + Sync {
    /// Whether this store keeps a secret until something deletes it.

    /// The only question worth asking. A store that answers `false` is refused rather than used
    /// carefully: a secret written to it is already lost, and the loss is silent.
    fn keeps_secrets_until_deleted(&self) -> bool;

    /// The secret held under `account`, or `None` when this store holds none.
    fn read(&self, account: &str) -> Result<Option<Zeroizing<String>>>;

    /// Put `secret` under `account`, replacing whatever was there.
    fn write(&self, account: &str, secret: &str) -> Result<()>;
}

/// The platform store, through `keyring`.
struct PlatformStore;

impl SystemStore for PlatformStore {
    fn keeps_secrets_until_deleted(&self) -> bool {
        use keyring::credential::CredentialPersistence;
        matches!(
            keyring::default::default_credential_builder().persistence(),
            CredentialPersistence::UntilDelete
        )
    }

    fn read(&self, account: &str) -> Result<Option<Zeroizing<String>>> {
        let entry = entry(account)?;
        match entry.get_password() {
            Ok(secret) => Ok(Some(Zeroizing::new(secret))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => bail!(
                "read {account} from the system secret store: {}",
                describe(&error)
            ),
        }
    }

    fn write(&self, account: &str, secret: &str) -> Result<()> {
        let entry = entry(account)?;
        entry.set_password(secret).map_err(|error| {
            anyhow::anyhow!(
                "write {account} to the system secret store: {}",
                describe(&error)
            )
        })
    }
}

fn entry(account: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SECRET_STORE_SERVICE, account).map_err(|error| {
        anyhow::anyhow!(
            "address {account} in the system secret store: {}",
            describe(&error)
        )
    })
}

/// What a `keyring` failure may be told to an operator.

/// Two variants wrap an error the platform itself produced and are passed through; the rest are
/// named by what went wrong and nothing else. `Error::Ambiguous` is the reason this function exists:
/// its `Display` renders `{items:?}` over the matching credentials, and the mock credential
/// `keyring` falls back to when no backend is selected derives `Debug` over a struct that holds the
/// password. One `{error}` in the wrong arm and a secret is in a log.
fn describe(error: &keyring::Error) -> String {
    match error {
        keyring::Error::PlatformFailure(inner) => {
            format!("platform secure storage failure: {inner}")
        }
        keyring::Error::NoStorageAccess(inner) => {
            format!("platform secure storage is not accessible: {inner}")
        }
        keyring::Error::NoEntry => "no such entry in the system secret store".to_string(),
        keyring::Error::BadEncoding(_) => {
            "the stored value is not UTF-8, so it was not written by this client".to_string()
        }
        keyring::Error::TooLong(attribute, limit) => {
            format!("attribute `{attribute}` is longer than this platform's limit of {limit} characters")
        }
        keyring::Error::Invalid(attribute, reason) => {
            format!("attribute `{attribute}` is invalid: {reason}")
        }
        keyring::Error::Ambiguous(items) => format!(
            "{} credentials in the system secret store match this entry; \
             remove the duplicates and run the same command again",
            items.len()
        ),
        // `keyring::Error` is `#[non_exhaustive]`: a variant added upstream must not become a way
        // for text this function has never seen to reach a log.
        _ => "the system secret store refused the operation".to_string(),
    }
}

/// The store this process uses.
pub(crate) struct SecretStore {
    backend: Backend,
    system: Box<dyn SystemStore>,
}

impl std::fmt::Debug for SecretStore {
    /// The backend and nothing else. Derived `Debug` would reach into the system store, and on the
    /// mock that means the password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretStore")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

impl SecretStore {
    /// Open the store this machine and this environment ask for.
    pub(crate) fn open() -> Result<Self> {
        Self::open_with(
            std::env::var(SECRET_STORE_VAR).ok().as_deref(),
            Box::new(PlatformStore),
        )
    }

    /// The same, with the request and the system store supplied.
    fn open_with(requested: Option<&str>, system: Box<dyn SystemStore>) -> Result<Self> {
        let backend = choose(requested, system.keeps_secrets_until_deleted())?;
        Ok(Self { backend, system })
    }

    /// The secret, or `None` when neither store holds it.

    /// On the system branch the file is still consulted, and that is the whole compatibility story:
    /// a client that wrote a key to disk before this module existed keeps finding it. On the file
    /// branch the system store is not consulted at all -- see the module header for why a forced
    /// branch has to be total.
    pub(crate) fn read(&self, name: &SecretName) -> Result<Option<Zeroizing<String>>> {
        if self.backend == Backend::System {
            if let Some(secret) = self.system.read(&name.account())? {
                return Ok(Some(secret));
            }
        }
        read_file(name)
    }

    /// Put `secret` where this store keeps things.
    pub(crate) fn write(&self, name: &SecretName, secret: &str) -> Result<()> {
        match self.backend {
            Backend::System => self.system.write(&name.account(), secret),
            Backend::File => write_file(name, secret),
        }
    }
}

/// Which store to use, given what was asked for and what the machine has.

/// Separated from [`SecretStore::open`] so the decision can be tested without the process
/// environment: it is process-wide, the suite runs in parallel, and a variable set by one test has
/// already been read by another in this repository.
fn choose(requested: Option<&str>, system_keeps_secrets: bool) -> Result<Backend> {
    match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some("system") if system_keeps_secrets => Ok(Backend::System),
        Some("system") => bail!(
            "{SECRET_STORE_VAR}=system, and this machine has no secret store that keeps a secret \
             until it is deleted. On Linux that is the ordinary case -- the client is built without \
             a Secret Service backend, so there is nothing here to demand. Unset \
             {SECRET_STORE_VAR}, or set it to `file`, and the client keeps its keys in files only \
             their owner can read."
        ),
        Some("file") => Ok(Backend::File),
        Some(other) => bail!(
            "{SECRET_STORE_VAR} must be `system` or `file`, and this run was given `{other}`. \
             Leaving it unset lets the client use the system store where the machine has one."
        ),
        None if system_keeps_secrets => Ok(Backend::System),
        None => Ok(Backend::File),
    }
}

/// Read the secret out of the client's own store.

/// The permission check comes first and the read second, which is the order this repository already
/// requires of every secret reader: a refusal that has already loaded the secret has done the thing
/// it refuses.
fn read_file(name: &SecretName) -> Result<Option<Zeroizing<String>>> {
    // A directory that is not there yet holds no secret, and that is the ordinary first run rather
    // than a failure. Without this the next line refuses instead: `resolve_private_file_path`
    // canonicalises the PARENT, so an absent one becomes "resolve parent directory for secret file
    // <path>: No such file or directory" -- the exact shape that already once made `note list`
    // report a missing pool as a broken environment variable.
    if !name
        .file()
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .is_none_or(Path::is_dir)
    {
        return Ok(None);
    }
    let path = crate::cli::note::resolve_private_file_path(name.file(), "secret file")?;
    crate::cli::support::refuse_exposed_secret_file_if_present(&path, "secret file")?;
    match std::fs::read_to_string(&path) {
        Ok(secret) => Ok(Some(Zeroizing::new(secret))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        // The path, never the bytes.
        Err(error) => bail!("read secret file {}: {error}", path.display()),
    }
}

/// Write the secret into the client's own store.

/// The directory is created owner-only WITH that mode rather than chmod-ed into it afterwards, and
/// the file lands 0600 by the same rule through `write_private_atomic` (an exclusive owner-only temp
/// in the destination directory, then a rename). A `chmod` on the next line leaves a window in which
/// the secret is on disk for anyone to read, and a window is exactly what this is here to prevent.
fn write_file(name: &SecretName, secret: &str) -> Result<()> {
    if let Some(parent) = name.file().parent().filter(|p| !p.as_os_str().is_empty()) {
        if !parent.is_dir() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt as _;
                builder.mode(0o700);
            }
            builder
                .create(parent)
                .with_context(|| format!("create secret directory {}", parent.display()))?;
        }
    }
    crate::cli::note::write_private_atomic(name.file(), secret.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A system store that answers, so the branch that takes one can be reached on a machine that
    /// has none. It also counts its calls, which is how "the forced branch never touched it" is
    /// asserted rather than assumed.
    struct FakeSystemStore {
        persistent: bool,
        held: Mutex<Option<String>>,
        reads: Mutex<usize>,
        writes: Mutex<usize>,
    }

    impl FakeSystemStore {
        fn persistent() -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                persistent: true,
                held: Mutex::new(None),
                reads: Mutex::new(0),
                writes: Mutex::new(0),
            })
        }

        fn vanishing() -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                persistent: false,
                held: Mutex::new(None),
                reads: Mutex::new(0),
                writes: Mutex::new(0),
            })
        }

        fn touches(&self) -> usize {
            *self.reads.lock().unwrap() + *self.writes.lock().unwrap()
        }
    }

    impl SystemStore for std::sync::Arc<FakeSystemStore> {
        fn keeps_secrets_until_deleted(&self) -> bool {
            self.persistent
        }

        fn read(&self, _account: &str) -> Result<Option<Zeroizing<String>>> {
            *self.reads.lock().unwrap() += 1;
            Ok(self.held.lock().unwrap().clone().map(Zeroizing::new))
        }

        fn write(&self, _account: &str, secret: &str) -> Result<()> {
            *self.writes.lock().unwrap() += 1;
            *self.held.lock().unwrap() = Some(secret.to_string());
            Ok(())
        }
    }

    const SECRET: &str = "d34dbeefcafef00d-secret-value-that-must-never-be-printed";

    fn store(requested: Option<&str>, system: std::sync::Arc<FakeSystemStore>) -> SecretStore {
        SecretStore::open_with(requested, Box::new(system)).expect("open the secret store")
    }

    /// The branch a headless seller takes, and the only branch this platform can take: no system
    /// store, so the client raises its own -- and the file it raises is owner-only from the moment
    /// it exists.
    #[test]
    fn with_no_system_store_the_client_raises_its_own_owner_only_file() {
        let temp = tempfile::tempdir().expect("temp dir");
        let name = SecretName::at(temp.path().join("nested").join("gateway.pem"));
        let store = store(None, FakeSystemStore::vanishing());
        assert_eq!(store.backend, Backend::File);

        assert!(
            store.read(&name).expect("read an absent secret").is_none(),
            "a secret nobody has written yet is absent, not an error"
        );
        store.write(&name, SECRET).expect("write the secret");
        assert_eq!(
            store
                .read(&name)
                .expect("read it back")
                .as_deref()
                .map(String::as_str),
            Some(SECRET)
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(name.file()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the client's own store is owner-only");
            let parent = std::fs::metadata(name.file().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(parent, 0o700, "and so is the directory it created for it");
        }
    }

    /// The other branch: a store that keeps a secret until it is deleted is used, and the value does
    /// not reach the disk at all.
    #[test]
    fn with_a_system_store_the_secret_goes_there_and_not_to_disk() {
        let temp = tempfile::tempdir().expect("temp dir");
        let name = SecretName::at(temp.path().join("gateway.pem"));
        let system = FakeSystemStore::persistent();
        let store = store(None, system.clone());
        assert_eq!(store.backend, Backend::System);

        store.write(&name, SECRET).expect("write the secret");
        assert_eq!(
            store
                .read(&name)
                .expect("read it back")
                .as_deref()
                .map(String::as_str),
            Some(SECRET)
        );
        assert!(
            !name.file().exists(),
            "a secret the system store holds has no business being on disk as well"
        );
    }

    /// A store that does not keep what it is given is refused, not used carefully.

    /// This is the mock `keyring` falls back to when no backend feature is selected. Its
    /// `set_password` returns `Ok`, so choosing by platform name would have reported a key stored
    /// and lost it at exit.
    #[test]
    fn a_store_that_forgets_is_refused_rather_than_used() {
        assert_eq!(choose(None, false).unwrap(), Backend::File);
        assert_eq!(choose(None, true).unwrap(), Backend::System);

        let error = choose(Some("system"), false).expect_err("a demand that cannot be met");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains(SECRET_STORE_VAR) && rendered.contains("until it is deleted"),
            "the refusal must say which variable and what is missing, got: {rendered}"
        );
    }

    /// The forced branch is total. A `file` that still consulted the keychain would make the file
    /// path untestable on every machine that has one -- which is the reason the variable exists.
    #[test]
    fn forcing_the_file_branch_leaves_the_system_store_untouched() {
        let temp = tempfile::tempdir().expect("temp dir");
        let name = SecretName::at(temp.path().join("gateway.pem"));
        let system = FakeSystemStore::persistent();
        let store = store(Some("file"), system.clone());
        assert_eq!(store.backend, Backend::File);

        store.write(&name, SECRET).expect("write the secret");
        assert_eq!(
            store
                .read(&name)
                .expect("read it back")
                .as_deref()
                .map(String::as_str),
            Some(SECRET)
        );
        assert_eq!(
            system.touches(),
            0,
            "a forced file branch must not read or write the system store"
        );
        assert!(
            name.file().exists(),
            "and the secret is in the file it forced"
        );
    }

    /// Nobody who already has a key on disk loses it: the system branch falls through to the file.
    #[test]
    fn a_key_already_on_disk_is_still_found_on_the_system_branch() {
        let temp = tempfile::tempdir().expect("temp dir");
        let name = SecretName::at(temp.path().join("gateway.pem"));
        let older_client = store(Some("file"), FakeSystemStore::vanishing());
        older_client
            .write(&name, SECRET)
            .expect("the key an older client wrote");

        let system = FakeSystemStore::persistent();
        let today = store(None, system.clone());
        assert_eq!(today.backend, Backend::System);
        assert_eq!(
            today
                .read(&name)
                .expect("read through the system branch")
                .as_deref()
                .map(String::as_str),
            Some(SECRET),
            "a key written before this module existed must still be found"
        );
        assert_eq!(
            *system.reads.lock().unwrap(),
            1,
            "the system store was asked first"
        );
    }

    /// A word the variable does not define is a refusal naming both words it does.
    #[test]
    fn an_unknown_request_is_refused_by_name() {
        let error = choose(Some("keychain"), true).expect_err("an undefined word");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("`system`")
                && rendered.contains("`file`")
                && rendered.contains("`keychain`"),
            "the refusal must name what was asked and what is accepted, got: {rendered}"
        );
        // An empty or whitespace-only value is the same as unset: a shell that exports the variable
        // without a value must not be a refusal.
        assert_eq!(choose(Some(""), false).unwrap(), Backend::File);
        assert_eq!(choose(Some("  "), true).unwrap(), Backend::System);
        assert_eq!(choose(Some(" file "), true).unwrap(), Backend::File);
    }

    /// The variable is actually read. `choose` is tested without the environment on purpose, so
    /// something has to prove `open` is wired to it.

    /// The environment is process-wide and this suite runs in parallel, so the variable is restored
    /// before the assertions and no other test in this module reads it.
    #[test]
    fn open_reads_the_variable() {
        static SERIALIZE: Mutex<()> = Mutex::new(());
        let _guard = SERIALIZE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let previous = std::env::var(SECRET_STORE_VAR).ok();
        std::env::set_var(SECRET_STORE_VAR, "file");
        let forced = SecretStore::open();
        std::env::set_var(SECRET_STORE_VAR, "no-such-store");
        let refused = SecretStore::open();
        match previous {
            Some(value) => std::env::set_var(SECRET_STORE_VAR, value),
            None => std::env::remove_var(SECRET_STORE_VAR),
        }

        assert_eq!(
            forced.expect("file is always available").backend,
            Backend::File
        );
        let rendered = format!("{:#}", refused.expect_err("an undefined word"));
        assert!(
            rendered.contains("no-such-store"),
            "the refusal must name the value the variable carried, got: {rendered}"
        );
    }

    /// The secret's value reaches no rendered surface -- and the same search, run over a surface
    /// where the secret IS present, finds it.

    /// Without the second half this test is worthless: "not found" and "cannot find" look
    /// identical, and a needle that never matches anything passes every negative assertion ever
    /// written. The control is not a synthetic string either: it is a real refusal from this same
    /// module, fed the secret as the word an operator typed into `DEXDO_SECRET_STORE`. That message
    /// is the one place here that echoes caller-supplied text back, it echoes a store NAME, and
    /// pointing the same search at it proves the search works on exactly the kind of text the
    /// assertion above declares clean.
    #[test]
    fn no_rendered_surface_carries_the_secret_and_the_search_can_prove_it_would() {
        let temp = tempfile::tempdir().expect("temp dir");
        let name = SecretName::at(temp.path().join("gateway.pem"));

        let mut rendered = String::new();
        // Every surface this module offers while it is holding the secret.
        let system = FakeSystemStore::persistent();
        let with_system = store(None, system.clone());
        with_system
            .write(&name, SECRET)
            .expect("write into the system store");
        rendered.push_str(&format!("{with_system:?}\n"));
        rendered.push_str(&format!("{:?}\n", with_system.backend));
        rendered.push_str(&format!("{name:?}\n"));
        rendered.push_str(&format!("{}\n", name.account()));

        let on_disk = store(Some("file"), FakeSystemStore::vanishing());
        on_disk
            .write(&name, SECRET)
            .expect("write into the client's own store");
        rendered.push_str(&format!("{on_disk:?}\n"));

        // And every failure it can produce while holding it. Both renderings of each: `{:#}` is
        // what a command prints, `{:?}` is what a panic or a `tracing` field would.
        let blocked = SecretName::at(temp.path().join("blocked"));
        std::fs::create_dir(blocked.file()).expect("occupy the path with a directory");
        for error in [
            choose(Some("system"), false).expect_err("a demand no machine here can meet"),
            on_disk
                .write(&blocked, SECRET)
                .expect_err("a directory is not a secret file"),
            on_disk
                .read(&blocked)
                .expect_err("and it is not one to read from either"),
        ] {
            rendered.push_str(&format!("{error:#}\n"));
            rendered.push_str(&format!("{error:?}\n"));
        }

        assert!(
            !rendered.contains(SECRET),
            "no rendered surface may carry the secret:\n{rendered}"
        );

        // THE CONTROL: the same needle, the same `contains`, over a refusal this module produced
        // with the secret inside it. If this fails, the assertion above proved nothing.
        let echoing = format!(
            "{:#}",
            choose(Some(SECRET), true).expect_err("an undefined store name")
        );
        assert!(
            echoing.contains(SECRET),
            "the control failed: this search cannot find the secret even in a message that \
             carries it, so the assertion above is not evidence.\nsearched: {echoing}"
        );
    }

    /// An exposed file is refused before it is read, not after.
    #[test]
    #[cfg(unix)]
    fn an_exposed_secret_file_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("temp dir");
        let name = SecretName::at(temp.path().join("gateway.pem"));
        let store = store(Some("file"), FakeSystemStore::vanishing());
        store.write(&name, SECRET).expect("write the secret");
        std::fs::set_permissions(name.file(), std::fs::Permissions::from_mode(0o644))
            .expect("expose it the way a bad backup would");

        let error = store
            .read(&name)
            .expect_err("an exposed secret file is refused");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("can be read by users other than its owner"),
            "the refusal must be the one this repository already gives, got: {rendered}"
        );
        assert!(
            !rendered.contains(SECRET),
            "and it must not carry the secret it refused to read"
        );
    }
}
