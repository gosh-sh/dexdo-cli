//! `dexdo wallet`: the wallet PROVIDER model and the durable active binding.

//! Three things live here, and deliberately nothing else.

//! **The provider is a subcommand, never a flag and never a default.** `ackinacki-wallet`,
//! `gosh-ai` and `manual` can all hand over the *same* canonical multisig contract, so an address,
//! a code hash or an on-chain parameter cannot tell them apart afterwards. The origin is therefore
//! recorded once, at bind time, and never re-derived. A guess here picks the wrong funding flow for
//! real money, which is why `dexdo wallet onboard` with no provider is an ERROR outside a terminal
//! rather than a menu nobody can answer or a default nobody chose.

//! **The binding is one atomic commit point.** `<data-dir>/wallet/binding.json` names the single
//! active binding. Per-binding secrets live under `<data-dir>/wallet/bindings/<binding-id>/`, and
//! the `binding-id` is minted BEFORE any key exists so re-binding the same provider writes beside
//! the previous binding's secrets instead of over them. A replaced binding is ARCHIVED, never
//! deleted: funds can still sit in the old Hot.

//! **A wallet-dependent command with no binding fails fast**, before any chain write, with
//! [`dexdo_core::error_codes::E_WALLET_NOT_CONFIGURED`] and a remediation naming the providers.

use crate::cli::args::{
    WalletArgs, WalletCommand, WalletProviderCommand, WalletRemoveArchivedArgs,
};
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{BufRead, IsTerminal as _, Write};

/// Which of the two setup commands is running. Both select a provider the same way; they differ in
/// what they require of the state that already exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WalletAction {
    /// First binding for this instance. Refuses to touch an existing one.
    Onboard,
    /// Replace the active binding. Requires one to exist, and archives it.
    Rebind,
}

impl WalletAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Onboard => "onboard",
            Self::Rebind => "rebind",
        }
    }
}

impl fmt::Display for WalletAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Where the funding (Hot) wallet came from. Written into the binding at bind time and read back
/// from that field only -- never inferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum WalletProvider {
    /// The Acki Nacki Wallet app, which provides a Vault/Hot pair.
    AckinackiWallet,
    /// The Gosh.ai service, which provides a Hot only.
    GoshAi,
    /// A Hot the operator already controls, connected by address plus a local secret file.
    Manual,
}

impl WalletProvider {
    /// The closed set, in the order the interactive menu numbers them.
    pub(crate) const ALL: [Self; 3] = [Self::AckinackiWallet, Self::GoshAi, Self::Manual];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::AckinackiWallet => "ackinacki-wallet",
            Self::GoshAi => "gosh-ai",
            Self::Manual => "manual",
        }
    }
}

impl fmt::Display for WalletProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<&WalletProviderCommand> for WalletProvider {
    fn from(command: &WalletProviderCommand) -> Self {
        match command {
            // The payload is the onboarding request, not part of the provider's identity.
            WalletProviderCommand::AckinackiWallet(_) => Self::AckinackiWallet,
            WalletProviderCommand::GoshAi(_) => Self::GoshAi,
            WalletProviderCommand::Manual(_) => Self::Manual,
        }
    }
}

/// The chain a binding is valid on. A Hot bound on one network must never be spent on the other,
/// so the network is part of the binding rather than of whichever endpoint flag a later command
/// happens to carry.

/// The binding schema is compiled on the same gate as the [`store`] that writes it: every provider
/// flow proves the wallet on chain before it is bound, so a build with no chain backend has nothing
/// that could produce a binding to describe.
/// The chain a binding is valid on, as the MANIFEST spells it.

/// This was a closed enum of two networks. It is a label now, and the difference is the point of
/// a client carrying a list of chains has an opinion about which chains exist, and refuses
/// the ones it has not heard of. A wallet bound on a chain nobody added to that list could not be
/// resolved at all -- not because anything was wrong with it, but because the binary predated it.

/// A Hot bound on one network must still never be spent on another, and that guarantee does not
/// come from the type being closed: it comes from the binding being keyed by this label and from
/// the label having exactly one source, the selected manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct WalletNetwork(String);

impl WalletNetwork {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    /// The network a money command is running on, read from the `network` field of the manifest it
    /// was pointed at.

    /// The manifest decides which chain the command's addresses and endpoint belong to, so it
    /// decides which wallet may fund it. A second source -- a flag, an endpoint guess, the
    /// binding's own field -- would be a way for the two answers to differ, and the guarantee is
    /// that they cannot.

    /// Which chains exist is not this client's to know, so the label is not checked against a list.
    /// It is checked against what it is USED as, and that is a single file name.

    /// An EMPTY label is refused: it would name a binding file `active/.json` and read back as a
    /// network that is not one.

    /// A label that is a PATH is refused for a harder reason. It is interpolated into a file name
    /// (`store.rs`, `active_dir().join(format!("{label}.json"))`), and `Path::join` does not treat
    /// its argument as one component: an absolute argument discards the base entirely, and `..`
    /// walks out of it. So `/tmp/x` or `../../x` would put the binding -- which records the paths
    /// to the Hot key and the recovery phrase -- anywhere the process can write, and put it
    /// somewhere `bound_networks` can never enumerate back, so `wallet show` would answer "No
    /// wallet bound" with the record on disk.

    /// This was unreachable while the label was a closed pair, and the manifest
    /// is a file the operator DOWNLOADS rather than types, so the check belongs with the type.
    pub(crate) fn from_manifest_label(label: &str) -> Result<Self> {
        let label = label.trim();
        if label.is_empty() {
            bail!(
                "the manifest declares an empty `network`, so there is no label to key the wallet \
                 binding by. Bindings are kept per network -- `wallet/active/<network>.json` -- and \
                 a nameless one cannot be told apart from any other. Give the manifest a `network`."
            );
        }
        // Checked as a path, because `Path::components` is what `join` itself will see. But the
        // label must BE that component, not merely reduce to one: `components()` normalizes a
        // trailing separator away, so `net-a/`, `net-a/.` and `net-a//` all yield one `Normal` and
        // would have passed -- and the label is stored and interpolated RAW, so the binding becomes
        // `wallet/active/net-a/.json`, a file in a directory nothing creates.

        // That was not a cosmetic escape. The write is the LAST step of onboarding: the run
        // verifies the Hot, deploys the multisig and spends the gas, and only then fails ENOENT on
        // a path whose parent is absent. Money moved, no record of where it went.
        let mut components = std::path::Path::new(label).components();
        let single_plain_name = match (components.next(), components.next()) {
            (Some(std::path::Component::Normal(only)), None) => only == label,
            _ => false,
        };
        if !single_plain_name {
            bail!(
                "the manifest declares `network` as `{label}`, and that is a path, not a name. The \
                 label keys one file inside this instance -- `wallet/active/<network>.json` -- so a \
                 label that is anything but a single plain file name would put the binding \
                 somewhere the command that reads bindings back cannot see, or in a directory that \
                 does not exist. Give the manifest a plain `network` name."
            );
        }
        Ok(Self(label.to_string()))
    }
}

/// The network this run is on, read from the manifest and nowhere else.

/// This replaced a `From<crate::cli::wallet::WalletNetwork>`: onboarding used to take the network from a `--network`
/// flag whose default was compiled in, so a run that never typed it bound a wallet on the test
/// network by construction. The manifest already decides which chain every other command addresses;
/// binding is the one place that used to disagree.
pub(crate) fn network_from_manifest() -> Result<WalletNetwork> {
    let manifest = crate::cli::commands::manifest_path()?;
    let deployed = dexdo_core::Deployed::load(&manifest)
        .map_err(|error| anyhow!("load the manifest {}: {error}", manifest.display()))?;
    WalletNetwork::from_manifest_label(&deployed.network)
}

impl fmt::Display for WalletNetwork {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Schema version of `binding.json`. A binary refuses a version it was not written against rather
/// than reading a newer file with fields it does not know it is ignoring.
pub(crate) const BINDING_VERSION: u32 = 1;

/// The active wallet binding: non-secret parameters plus PATHS to owner-only secret files.

/// No key, seed phrase or recovery phrase is a field of this type, so no code path can write one
/// into `binding.json` -- the file is deliberately not where secrets live. `provider` has no serde
/// default: a binding that does not say where its wallet came from is refused rather than guessed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WalletBinding {
    pub(crate) version: u32,
    pub(crate) id: String,
    pub(crate) provider: WalletProvider,
    pub(crate) network: WalletNetwork,
    /// Canonical `<dapp_id>::<account_id>` address of the Hot wallet that funds spends.
    pub(crate) hot_address: String,
    /// Canonical address of the Vault, for the providers that supply one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) vault_address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) hot_key_file: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) vault_key_file: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) hot_seed_file: Option<std::path::PathBuf>,
    /// Reserved non-secret metadata already obtained from an authenticated `wallet_hello`. Nothing
    /// reads it yet; it is stored so it is not lost when onboarding completes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) push_profile_address: Option<String>,
}

/// The one entry point behind `dexdo wallet`.
pub(crate) async fn run_wallet(args: WalletArgs) -> Result<()> {
    if matches!(&args.command, WalletCommand::RemoveArchived(_)) {
        let WalletCommand::RemoveArchived(remove) = args.command else {
            unreachable!("matched remove-archived")
        };
        return run_remove_archived(remove).await;
    }
    if matches!(&args.command, WalletCommand::Show(_)) {
        let WalletCommand::Show(show) = args.command else {
            unreachable!("matched show")
        };
        return run_show(show).await;
    }
    let (action, explicit, json) = match &args.command {
        WalletCommand::Onboard(onboard) => (
            WalletAction::Onboard,
            onboard.provider.as_ref(),
            onboard.json,
        ),
        WalletCommand::Rebind(rebind) => {
            (WalletAction::Rebind, rebind.provider.as_ref(), rebind.json)
        }
        WalletCommand::RemoveArchived(_) | WalletCommand::Show(_) => {
            unreachable!("handled above")
        }
    };
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let interactive = stdin.is_terminal() && stdout.is_terminal();
    let provider = resolve_provider(
        action,
        explicit,
        interactive,
        &mut stdin.lock(),
        &mut stdout.lock(),
    )?;
    run_selected(action, provider, explicit, json).await
}

/// A manifest describing a different chain than the one being worked on, refused as that.

/// The refusal belongs to the CALLER and not to `wallet_read_endpoint`, because only the caller
/// knows which network was expected -- for `remove-archived` it comes from the archived binding and
/// from nothing else. What the caller used to do instead was pass `None` onward, which
/// meant "use the network's own default" until removed the default and then meant "no
/// manifest was named": said to an operator whose variable is set and whose file is fine.
fn refuse_a_manifest_for_another_network(declared: &str, expected: &WalletNetwork) -> Result<()> {
    // Through `from_manifest_label`, because that is the ONE way this label becomes a network
    // everywhere else in the client, and it trims. Compared as raw strings, ` net-a ` is a
    // different chain from `net-a` -- and the only refusal that comparison can ever produce
    // contradicts itself in its own sentence: "names net-a, and this archived binding is on
    // net-a". A label the type refuses outright keeps the type's refusal, which says what is
    // wrong with the label instead of blaming the network for not matching.
    if WalletNetwork::from_manifest_label(declared)? == *expected {
        return Ok(());
    }
    bail!(
        "{} names {declared}, and this archived binding is on {expected}. A manifest for another \
         chain has nothing to say about how to reach this one, and the network a removal runs on \
         comes from the binding and from nothing else. Point it at {expected}'s manifest.",
        dexdo_core::params::MANIFEST_PATH_VAR
    )
}

/// The endpoint every wallet command reads through: the manifest's own `endpoint`, normalized to a
/// URL.

/// One source, because there is no longer a second. `--endpoint` ranked above the manifest and is
/// gone, and so is the per-network default that ranked below it; this doc described both
/// long after neither existed, which made the first refusal below look unreachable.

/// Normalizing here, once, is what the single function buys. A manifest naming a bare host is the
/// form every other command on this CLI takes and the form the acceptance suite passes everywhere.
/// Connected unnormalized it posts to `net-a.example/graphql` with no scheme, and every read
/// through it fails: removal is refused with "read every balance... nothing was removed" -- the very
/// refusal an EMPTY Hot would get -- and onboarding spends its whole activation timeout on "the
/// chain read failed; retrying", measured at 600 s on the stand.

/// The gate is the caller's: `dexdo_core::normalize_endpoint` is re-exported only under the chain build,
/// so a wider gate compiles this function in a build where the symbol it calls does not exist --
/// `cargo check -p dexdo --tests` on default features stops at E0425.
pub(crate) fn wallet_read_endpoint(
    manifest: Option<&std::path::Path>,
    network: WalletNetwork,
) -> Result<String> {
    // Worded as reaching the network rather than as reading its balances: two of the four callers
    // read no balance at all -- the gosh-ai path proves a Hot, and onboarding PUBLISHES a request
    // -- and the gosh-ai one only started reaching this text at all with.

    // The manifest is the only source. `--endpoint` used to be the first of three and is gone
    // a manifest an operator points at is what replaces it, and it is the same file the
    // rest of the client already reads. No per-network default to fall back on either: a client
    // that keeps host constants keeps an opinion about which chains exist.

    // THREE SITUATIONS, THREE REFUSALS, and they used to be one. The single message said "the
    // manifest names no `endpoint`" whether the caller had named a manifest or not and whether the
    // named file could be read or not -- and the loader's own error went into `.ok()` and was lost.
    // An operator in the first situation was told to add a field to a file nobody had asked them
    // for; an operator in the second was told to add a field their file may already have. Both then
    // edit something correct and see no change.
    let Some(path) = manifest else {
        anyhow::bail!(
            "no manifest was named, so there is nothing to say how to reach {network}. Point {} at the \
             deployment manifest of the network you are working on.",
            dexdo_core::params::MANIFEST_PATH_VAR
        );
    };
    // `.context`, not a fresh error built from `{error}`: `Deployed::load` documents that the
    // `io::Error` stays in the chain because `doctor` asks whether the cause was `NotFound` and
    // answers with the actionable refusal. Flattened, "Permission denied" never reaches the
    // operator and that question silently answers false.
    let deployed = dexdo_core::Deployed::load(path).map_err(|error| {
        error.context(format!(
            "the manifest {} could not be read, so there is nothing to say how to reach {network}",
            path.display()
        ))
    })?;
    // The third situation is not decided here. `resolve_endpoint` already refuses a manifest
    // carrying no `endpoint`, already normalizes the value, and its refusal says more than a
    // second copy would -- that a network NAME is not an address, which is how a mainnet manifest
    // once ended up answering from a test chain. Two texts for one condition drift apart on the
    // next edit to either. What this adds is the file, which the core function cannot know.
    dexdo_core::resolve_endpoint(None, &deployed).map_err(|error| {
        anyhow!(
            "the manifest {} does not say how to reach {network}: {error:#}",
            path.display()
        )
    })
}

/// How long one Hot's balance read may take before it is reported as unread.

/// Five seconds, the figure `note_pick` already settled on for the same trade: a balance nobody
/// could read in five seconds is worth less to an operator than the command answering at all. The
/// recorded fields -- the addresses, the provider, the secret file -- are the part they cannot get
/// anywhere else, and they must not be held behind a chain that is not answering.
const BALANCE_READ_TIMEOUT_SECS: u64 = 5;

/// What the bound Hot holds, or the reason the figure is missing. Never a zero on failure.

/// [`HotBalanceReader`](crate::cli::wallet_funding::HotBalanceReader) states the rule this obeys:
/// "a read failure must surface as `Err`. It must never be reported as a zero balance." The whole
/// value of this line to an operator is that a zero means an empty wallet rather than a chain that
/// did not answer -- confuse the two and the command that was supposed to end the guessing starts
/// it.

/// **The chain is dialled only when the manifest agrees about the network.** With no `--network`
/// this command reports every network that has a binding, and the manifest names exactly one.
/// Reading a mainnet Hot through another network's manifest endpoint would query the wrong chain and
/// label the answer with the right one -- an operator holding both would be told their mainnet
/// wallet is empty. A binding the manifest has nothing to say about is reported with that as the
/// reason, which is also the honest answer to "why is there no number here".
async fn hot_balance_for(
    network: &WalletNetwork,
    hot_address: &str,
) -> std::result::Result<crate::cli::wallet_funding::HotBalances, String> {
    use crate::cli::wallet_funding::HotBalanceReader;

    let manifest = crate::cli::commands::manifest_path().map_err(|error| format!("{error}"))?;
    let declared = dexdo_core::Deployed::load(&manifest)
        .map_err(|error| format!("{} cannot be read: {error}", manifest.display()))?
        .network;
    if declared != network.as_str() {
        return Err(format!(
            "the manifest {} names {declared}, and this binding is on {network}",
            manifest.display()
        ));
    }
    let endpoint = wallet_read_endpoint(Some(manifest.as_path()), network.clone())
        .map_err(|error| format!("{error}"))?;
    let client = dexdo_core::ChainClient::connect(&endpoint)
        .map_err(|error| format!("connect read-only balance endpoint {endpoint}: {error}"))?;
    let hot = dexdo_core::CanonicalAddress::parse(hot_address)
        .map_err(|error| format!("the recorded Hot address is unusable: {error}"))?;
    match tokio::time::timeout(
        std::time::Duration::from_secs(BALANCE_READ_TIMEOUT_SECS),
        client.hot_balances(&hot),
    )
    .await
    {
        Ok(Ok(balances)) => Ok(balances),
        Ok(Err(error)) => Err(format!("{error}")),
        Err(_) => Err(format!(
            "{endpoint} did not answer in {BALANCE_READ_TIMEOUT_SECS}s"
        )),
    }
}

/// `dexdo wallet show`: which wallet this instance is bound to, and what it holds.

/// Read-only in the strong sense: it reads the recorded binding and asks the chain for a balance,
/// and writes nothing. Running it can never change what it reports.

/// The balance is the reason most operators run this at all -- it used to print four recorded
/// fields and no figure, which answered "which wallet" and left "is there money on it" to a second
/// command the operator had to know about. It is a READ, not a requirement: every failure to obtain
/// it degrades to a named reason beside the binding, and `--no-balances` skips it entirely, so the
/// command still answers offline.

/// Secrets are named by PATH and never read. The binding records where the key or seed file lives
/// because a later command has to sign with it; this command prints that location so an operator
/// knows which file matters, and opens neither.
async fn run_show(args: crate::cli::args::WalletShowArgs) -> Result<()> {
    let store = WalletStore::open()?;
    // Bindings are per network, and "which wallet am I bound to" is usually asked without one in
    // mind. With no --network every network that HAS a binding is shown, which also answers the
    // question an operator did not think to ask: that the other network is bound too, to something
    // else.
    // With no --network, every network that HAS a binding is shown -- and "every" now means what
    // is on DISK, not two names compiled into the client. Enumerating a fixed pair would hide a
    // binding on any chain added after this binary was built, and hide it silently: the command
    // would answer "nothing bound" while the file sat right there.
    let networks: Vec<WalletNetwork> = store.bound_networks();

    // Read, never migrate: `load_active` relocates a legacy `wallet/binding.json` and deletes it,
    // which would make the command that only reports change what it reports.

    // And a network whose record cannot be read is reported as that, instead of aborting the whole
    // command with `?`. A corrupt record for one network used to hide a healthy one for another, and the
    // operator lost the address of the Hot they were trying to diagnose -- which is the same rule
    // `store.rs` already states for onboarding: refusing a broken record must not put the only way
    // out of a corrupt binding behind the corrupt binding.
    let mut found = Vec::new();
    let mut unreadable = Vec::new();
    for network in &networks {
        match store.peek_active(network) {
            Ok(Some(binding)) => found.push((network, binding)),
            Ok(None) => {}
            Err(error) => unreadable.push((network, format!("{error:#}"))),
        }
    }

    // One entry per found binding, in the same order: `Some(Ok)` a figure, `Some(Err)` the reason
    // there is none, `None` nobody asked. Read before either view is rendered so the human and the
    // machine answer from the same observation rather than each making its own.
    let mut holdings: Vec<Option<std::result::Result<_, String>>> = Vec::with_capacity(found.len());
    for (network, binding) in &found {
        holdings.push(if args.no_balances {
            None
        } else {
            Some(hot_balance_for(network, &binding.hot_address).await)
        });
    }

    if args.json {
        let objects: Vec<serde_json::Value> = found
            .iter()
            .zip(&holdings)
            .map(|((network, binding), held)| {
                // Raw units, and named as raw. Directive: a rendered figure and a raw one in the
                // same object invites multiplying them. The human view below renders; this states.
                let shell = held.as_ref().and_then(|read| read.as_ref().ok())
                    .map(|balances| balances.get(dexdo_core::params::SHELL_CURRENCY_ID).to_string());
                let native = held.as_ref().and_then(|read| read.as_ref().ok())
                    .map(|balances| balances.native.to_string());
                // Present and non-null ONLY when a read was attempted and failed. A caller
                // distinguishes "empty wallet" from "unknown" by this field and not by a zero.
                let unread = held.as_ref().and_then(|read| read.as_ref().err()).cloned();
                serde_json::json!({
                    "network": network.as_str(),
                    "hot": dexdo_core::address::display(&binding.hot_address),
                    "vault": binding.vault_address.as_deref().map(dexdo_core::address::display),
                    "provider": binding.provider.as_str(),
                    "binding_id": binding.id,
                    "hot_key_file": binding.hot_key_file.as_ref().map(|path| path.display().to_string()),
                    "hot_seed_file": binding.hot_seed_file.as_ref().map(|path| path.display().to_string()),
                    "binding_file": store.binding_path(network).display().to_string(),
                    "shell_raw": shell,
                    "native_raw": native,
                    "balance_unread": unread,
                })
            })
            .collect();
        let broken: Vec<serde_json::Value> = unreadable
            .iter()
            .map(|(network, error)| {
                serde_json::json!({ "network": network.as_str(), "error": error })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({ "bindings": objects, "unreadable": broken })
        );
        return Ok(());
    }

    // Nothing bound is an ANSWER, not an error: the operator asked a question and this is the true
    // reply. It exits zero and names the command that would change it -- a refusal here would make
    // `wallet show` unusable in the one situation where it is most natural to run it, right before
    // onboarding.
    if found.is_empty() && unreadable.is_empty() {
        use crate::cli::style::{self, Role};
        let palette = style::Palette::stdout();
        // Named through `command_here`: pasted verbatim, a hardcoded onboard line -- one naming
        // its own data directory -- bound a wallet in the platform default directory rather than
        // the `--data-dir` this run was given, which is the multi-seller-per-host layout, where
        // every participant has their own state directory.

        // No `--network` on it. The flag is gone, so a line carrying it is one the operator
        // pastes and the binary refuses -- which is worse than no suggestion at all. Which chain the
        // binding lands on is the selected manifest's answer, the same as for the command printing
        // this, so the line needs to say nothing about it.
        print!(
            "{}\n{}\n",
            style::paint(palette, Role::Bold, "No wallet bound"),
            style::field(
                palette,
                "next",
                &style::action(
                    palette,
                    &crate::cli::support::command_here("wallet onboard manual"),
                ),
                Role::Text,
            ),
        );
        return Ok(());
    }

    use crate::cli::style::{self, Role};
    let palette = style::Palette::stdout();
    let mut out = String::new();
    for (index, ((network, binding), held)) in found.iter().zip(&holdings).enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&style::paint(
            palette,
            Role::Bold,
            &format!("Wallet bound on {network}"),
        ));
        out.push('\n');
        out.push_str(&style::field(
            palette,
            "Hot",
            &dexdo_core::address::display(&binding.hot_address),
            Role::Id,
        ));
        out.push('\n');
        if let Some(vault) = binding.vault_address.as_deref() {
            out.push_str(&style::field(
                palette,
                "Vault",
                &dexdo_core::address::display(vault),
                Role::Id,
            ));
            out.push('\n');
        }
        out.push_str(&style::field(
            palette,
            "provider",
            &binding.provider.to_string(),
            Role::Text,
        ));
        out.push('\n');
        out.push_str(&style::field(palette, "binding", &binding.id, Role::Id));
        out.push('\n');
        // The one path that IS the operator's business: 681 keeps the client's own files out of a
        // result, but the secret file is theirs -- they created it, they must not lose it, and only
        // the binding knows which of several it is.
        if let Some(secret) = binding
            .hot_key_file
            .as_ref()
            .or(binding.hot_seed_file.as_ref())
        {
            out.push_str(&style::field(
                palette,
                "secret",
                &secret.display().to_string(),
                Role::Meta,
            ));
            out.push('\n');
        }
        // What the Hot holds -- the question the recorded fields above cannot answer, and the one
        // this command was being run to answer anyway. Two figures because they are two disjoint
        // balances: SHELL is the trading money, native vmshell is the gas that sends it, and a Hot
        // rich in one and empty in the other fails in a way neither figure alone explains.
        match held {
            Some(Ok(balances)) => {
                out.push_str(&style::field(
                    palette,
                    "SHELL",
                    &format!(
                        "{} SHELL (raw {})",
                        dexdo_core::shell_amount(
                            balances.get(dexdo_core::params::SHELL_CURRENCY_ID)
                        ),
                        balances.get(dexdo_core::params::SHELL_CURRENCY_ID)
                    ),
                    Role::Text,
                ));
                out.push('\n');
                out.push_str(&style::field(
                    palette,
                    "gas",
                    &format!(
                        "{} vmshell (raw {})",
                        dexdo_core::shell_amount(balances.native),
                        balances.native
                    ),
                    Role::Text,
                ));
                out.push('\n');
            }
            // Named as unread, with the reason, and never as zero: an operator who reads "0 SHELL"
            // off an unreachable chain stops looking for the money that is still there.
            Some(Err(why)) => {
                out.push_str(&style::field(
                    palette,
                    "balance",
                    &format!("not read: {why}"),
                    Role::Err,
                ));
                out.push('\n');
            }
            // --no-balances: nothing was asked, so nothing is claimed. A "not read" line here would
            // report the operator's own choice back to them as a fault.
            None => {}
        }
    }
    // A record that could not be read is reported, not swallowed: it is the state an operator most
    // needs named, and this command is where they come to find out.
    for (network, error) in &unreadable {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&style::paint(
            palette,
            Role::Bold,
            &format!("Binding on {network} cannot be read"),
        ));
        out.push('\n');
        out.push_str(&style::field(
            palette,
            "file",
            &store.binding_path(network).display().to_string(),
            Role::Meta,
        ));
        out.push('\n');
        out.push_str(&style::field(palette, "why", error, Role::Err));
        out.push('\n');
    }
    print!("{out}");
    Ok(())
}

/// Permanently forget one OLD binding after its Hot is proven empty by a read-only chain call.
async fn run_remove_archived(args: WalletRemoveArchivedArgs) -> Result<()> {
    let store = WalletStore::open()?;
    let target = store.archived_binding(&args.binding_id)?;
    // (rymkapro, 2026-08-17): this command deletes the ONLY local keys to a Hot, and it used
    // to decide from two reads it did not own. The journal check and the balance check are two
    // separate observations, and a money command running beside them fits entirely into the gap:

    // 1. `note deploy`/`note topup` loads the active binding and its keys into memory;
    // 2. a parallel rebind archives that binding;
    // 3. `remove-archived` reads a journal that is still empty and a Hot that is still zero;
    // 4. the money command writes `prepared` and sends its Vault -> Hot request;
    // 5. `remove-archived` deletes the keys;
    // 6. the confirmed request later credits a Hot nobody can spend from.

    // The cure is the turn the spenders already take, not a second mechanism: the SAME lock, under
    // the SAME key, held across BOTH observations and the deletion. `funding_wallet_lock_path`
    // canonicalises the address before hashing, so the Hot recorded in the binding and the wallet a
    // money command resolves hash to one lock file even when their spellings differ.

    // The network keying that lock is the archived binding's own, recorded when it was written and
    // never edited afterwards. It used to come from a deployed-contracts manifest the operator
    // pointed at, checked here for equality with this same field: the check could only ever fail by
    // the operator naming the wrong file, and it passed by restating what the binding already said.
    // Everything that observes state still happens under the lock.
    let _funding_lock = crate::cli::note_cmd::acquire_funding_wallet_lock(
        target.binding.network.as_str(),
        &target.binding.hot_address,
    )?;
    refuse_removal_while_funding_may_still_arrive(&target.binding)?;
    // The NETWORK comes from the archived binding and from nothing else -- removed
    // `--contracts` from this command for exactly that reason, so that no argument could send a
    // removal at a chain the binding was not bound on.

    // Where to DIAL is a different question, and it is the manifest's. The client finds it the way
    // it finds it everywhere else, and it is consulted only when it agrees with the binding about
    // which network this is: a manifest describing another chain has nothing to say about how to
    // reach this one.

    // A disagreeing manifest is refused HERE, in its own words, rather than being passed on as
    // `None`. It used to fall through to the network's own default, and when removed that
    // default the `then_some` stayed: the disagreement then arrived at `wallet_read_endpoint` as
    // "no manifest was named" -- said to an operator whose variable is set and whose file is
    // perfectly good. Only the caller knows which network was expected, so only the caller can say
    // this. Worded as `hot_balance_for` already words it, twenty lines up.
    let manifest = crate::cli::commands::manifest_path()?;
    if let Ok(deployed) = dexdo_core::Deployed::load(&manifest) {
        refuse_a_manifest_for_another_network(&deployed.network, &target.binding.network)?;
    }
    let endpoint = wallet_read_endpoint(Some(manifest.as_path()), target.binding.network.clone())?;
    let client = dexdo_core::ChainClient::connect(&endpoint).map_err(|error| {
        anyhow::anyhow!("connect read-only balance endpoint {endpoint}: {error}")
    })?;
    let removed = remove_archived_binding_after_balance_check(&store, &target, &client).await?;
    println!(
        "removed archived wallet binding {} and its secrets directory after Hot {} on {} was \
         proven to hold zero native and zero ECC balances",
        removed.id, removed.hot_address, removed.network
    );
    Ok(())
}

/// The money-safety boundary: no local removal is reachable before a successful all-zero read.
async fn remove_archived_binding_after_balance_check<R>(
    store: &WalletStore,
    target: &store::ArchivedBinding,
    reader: &R,
) -> Result<WalletBinding>
where
    R: crate::cli::wallet_funding::HotBalanceReader,
{
    let hot =
        dexdo_core::CanonicalAddress::parse(&target.binding.hot_address).map_err(|error| {
            anyhow::anyhow!(
            "archived binding {} records unusable Hot address {:?}: {error}; nothing was removed",
            target.binding.id,
            target.binding.hot_address
        )
        })?;
    let balances = reader.hot_balances(&hot).await.map_err(|error| {
        anyhow::anyhow!(
            "read every balance of archived binding {} Hot {}: {error}; nothing was removed",
            target.binding.id,
            target.binding.hot_address
        )
    })?;
    let nonzero_ecc = balances
        .balances
        .iter()
        .filter(|(_, balance)| **balance != 0)
        .map(|(currency, balance)| format!("ECC[{currency}]={balance}"))
        .collect::<Vec<_>>();
    if balances.native != 0 || !nonzero_ecc.is_empty() {
        bail!(
            "archived binding {} Hot {} still holds native={} and {}; refusing permanent removal",
            target.binding.id,
            target.binding.hot_address,
            balances.native,
            if nonzero_ecc.is_empty() {
                "no non-zero ECC balances".to_string()
            } else {
                nonzero_ecc.join(", ")
            }
        );
    }
    let removed = target.binding.clone();
    store.remove_archived_binding(target)?;
    Ok(removed)
}

/// Refuse permanent removal while the funding journal still records a generation that can move
/// money INTO this archived Hot.

/// The balance proof answers "how much is on that Hot now". This answers "can more still arrive
/// there", and the two are different questions. A Vault -> Hot request the operator has not yet
/// confirmed sits in the Vault's queue addressed at the OLD Hot; that Hot reads zero for as long as
/// the request is unconfirmed. Removing the binding inside that window deletes the only local key
/// to the account the money is on its way to, and confirming the request afterwards pays into an
/// account nobody can spend from - which is the whole loss this command exists to prevent.

/// Nothing new is recorded for this. Every generation in the durable funding journal is read under
/// the same (network, Hot) key the funding mechanism writes it under, and
/// [`crate::cli::wallet_funding::FundingJournalRecord::generation_may_still_execute`] is the
/// predicate that mechanism itself uses for "this generation may still move money". A locally
/// expired generation cannot be confirmed any longer and does not block removal; at the exact UTC
/// deadline it is still live and blocks. A generation carrying a finalized verdict - `expired` (it
/// never left the Vault) or `satisfied` (closed against an observed balance, reachable only once
/// every recorded queue id has a verdict) - is not blocking either. This refuses the window it has
/// to refuse and nothing wider: a Hot with no live journal record is removable as before.

/// An unreadable or unknown-version journal is an error rather than a pass. That record may be the
/// only local trace of a request that is already on chain, and the journal reader says so in its
/// own words.
fn refuse_removal_while_funding_may_still_arrive(binding: &WalletBinding) -> Result<()> {
    let hot = dexdo_core::CanonicalAddress::parse(&binding.hot_address).map_err(|error| {
        anyhow::anyhow!(
            "archived binding {} records unusable Hot address {:?}: {error}; nothing was removed",
            binding.id,
            binding.hot_address
        )
    })?;
    // Exactly the key `fund_hot_for_money_command` writes under: the binding's own network label
    // and the canonical round-trip of its Hot. A different rendering here would read a file that
    // does not exist and report "nothing pending" about a request that is.
    let hot_address = hot.to_string();
    let data_dir = crate::cli::data_dir::effective()?;
    refuse_removal_while_funding_may_still_arrive_at(
        binding,
        &data_dir,
        &hot_address,
        crate::cli::wallet_funding::unix_now_secs(),
    )
}

fn refuse_removal_while_funding_may_still_arrive_at(
    binding: &WalletBinding,
    data_dir: &std::path::Path,
    hot_address: &str,
    now: u64,
) -> Result<()> {
    use crate::cli::wallet_funding::{funding_journal_path, load_funding_journal_records};

    let network = binding.network.as_str();
    let Some(record) = load_funding_journal_records(data_dir, network, hot_address)?
        .unwrap_or_default()
        .into_iter()
        .find(|record| record.generation_may_still_execute() && !record.is_expired_at(now))
    else {
        return Ok(());
    };
    bail!(
        "archived binding {} cannot be removed: its funding journal {} records generation {} in \
         state {:?} for Hot {hot_address}, so that request may still move money into a Hot whose \
         only local key this removal would destroy. {} Settle it from the Vault pending list, or \
         wait until its local UTC deadline {} has passed; after that this removal is allowed. \
         Nothing was removed.",
        binding.id,
        funding_journal_path(data_dir, network, hot_address).display(),
        record.generation,
        record.state,
        record.pending_transaction_id.as_deref().map_or_else(
            || "No Vault queue transaction id was ever observed for it.".to_string(),
            |pending| format!("Its Vault queue transaction is {pending}."),
        ),
        record.expires_at_unix,
    );
}

/// Pick the provider: the subcommand when one was given, the terminal menu when there is a terminal
/// to show it in, and otherwise an error that names every valid subcommand.

/// `interactive` is passed in rather than probed here so the three outcomes are testable without a
/// pty. It must be true only when BOTH stdin and stdout are terminals: a redirected stdout would
/// bury the menu in a file, and a closed stdin has no answer to give.
pub(crate) fn resolve_provider<R: BufRead, W: Write>(
    action: WalletAction,
    explicit: Option<&WalletProviderCommand>,
    interactive: bool,
    reader: &mut R,
    writer: &mut W,
) -> Result<WalletProvider> {
    if let Some(command) = explicit {
        return Ok(WalletProvider::from(command));
    }
    if !interactive {
        bail!(non_interactive_provider_message(action));
    }
    prompt_for_provider(action, reader, writer)
}

/// The refusal a script, a CI job or a headless server sees. It lists the whole closed set, because
/// the operator has to type one of them, and it names `manual` for the headless case: a remote
/// seller has no terminal, no browser and no camera, so a QR-based provider cannot reach it.
fn non_interactive_provider_message(action: WalletAction) -> String {
    let mut message = format!(
        "dexdo wallet {action} needs a provider subcommand in a non-interactive environment: \
         no provider is ever chosen for you. Run one of:"
    );
    for provider in WalletProvider::ALL {
        message.push_str(&format!("\n  dexdo wallet {action} {provider}"));
    }
    message.push_str(
        "\nOn a headless host with no TTY, no browser and no camera, use `manual`: its whole \
         input is a plain printed address, so it needs neither a QR nor an interactive prompt.",
    );
    message
}

/// The terminal menu. Numbered exactly as the providers are ordered, and it also accepts the
/// provider name, because that is what the error above tells operators to type.
fn prompt_for_provider<R: BufRead, W: Write>(
    action: WalletAction,
    reader: &mut R,
    writer: &mut W,
) -> Result<WalletProvider> {
    writeln!(
        writer,
        "Select a wallet provider for dexdo wallet {action}:"
    )?;
    for (index, provider) in WalletProvider::ALL.iter().enumerate() {
        writeln!(writer, "{}. {provider}", index + 1)?;
    }
    write!(writer, "> ")?;
    writer.flush()?;
    let mut answer = String::new();
    if reader.read_line(&mut answer)? == 0 {
        bail!(
            "no provider was selected for dexdo wallet {action} (input ended); \
             re-run with an explicit provider subcommand"
        );
    }
    provider_from_answer(answer.trim()).ok_or_else(|| {
        anyhow::anyhow!(
            "`{}` is not a wallet provider; answer with 1, 2 or 3, or with {}",
            answer.trim(),
            WalletProvider::ALL
                .iter()
                .map(|provider| format!("`{provider}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}

/// One menu answer to one provider. Position or name; nothing else, and no fuzzy match -- a wrong
/// provider is a wrong funding flow.
fn provider_from_answer(answer: &str) -> Option<WalletProvider> {
    WalletProvider::ALL
        .iter()
        .enumerate()
        .find(|(index, provider)| answer == (index + 1).to_string() || answer == provider.as_str())
        .map(|(_, provider)| *provider)
}

/// The durable half. It is compiled on the same gate as [`crate::cli::note`], whose
/// `write_private_atomic` is the single atomic owner-only write this module commits through. Every
/// provider flow proves the wallet on chain before it is bound, so a build with no chain backend
/// has nothing that could produce a binding to store.
mod store;

pub(crate) use store::{BindingDraft, WalletStore};

/// The funding (Hot) wallet a money command will actually spend from, and where its signing secret
/// is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FundingWallet {
    pub(crate) address: String,
    pub(crate) key: Option<std::path::PathBuf>,
    pub(crate) seed_file: Option<std::path::PathBuf>,
}

/// An active binding names the right Hot but cannot supply the credential needed for a new spend.

/// Typed separately from an absent binding: onboarding over an existing binding is unsafe, while a
/// machine consumer must not receive `INTERNAL` for an operator-remediable wallet state.
#[derive(Debug)]
pub(crate) struct WalletBindingCannotSign {
    message: String,
}

impl WalletBindingCannotSign {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub(crate) fn for_binding(binding: &WalletBinding) -> Self {
        Self::new(format!(
            "wallet binding {} (provider `{}`, Hot {}) records no local Hot key or seed file, so \
             this instance cannot sign a spend from it; re-bind it with a provider flow that \
             stores one, or pass `--multisig-address` and `--multisig-private-key` for this run",
            binding.id, binding.provider, binding.hot_address,
        ))
    }
}

impl std::fmt::Display for WalletBindingCannotSign {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for WalletBindingCannotSign {}

/// Resolve only the public Hot identity and any credential path the binding happens to carry.

/// Recovery of an already submitted voucher needs the identity to match its journal, but must not
/// demand a signing credential: that recovery waits for or proves the existing submit and never
/// signs a second one. Commands that are about to make a fresh spend keep using
/// [`resolve_funding_wallet`], which enforces the credential.
pub(crate) fn resolve_funding_wallet_identity(
    store: &WalletStore,
    network: &WalletNetwork,
    explicit_address: Option<&str>,
    explicit_key: &Option<std::path::PathBuf>,
    explicit_seed_file: &Option<std::path::PathBuf>,
) -> Result<FundingWallet> {
    if let Some(address) = explicit_address {
        return Ok(FundingWallet {
            address: address.to_string(),
            key: explicit_key.clone(),
            seed_file: explicit_seed_file.clone(),
        });
    }
    let binding = store.require_active(network)?;
    Ok(FundingWallet {
        address: binding.hot_address,
        key: binding.hot_key_file,
        seed_file: binding.hot_seed_file,
    })
}

/// Decide which Hot a money command spends from, before it reaches the chain.

/// `network` is the chain the command is ACTUALLY running on, taken from the deployed contracts
/// manifest it was pointed at. It is a parameter and not something read from the binding, because
/// it is the question: the binding says which network it belongs to, the manifest says which
/// network is being spent on, and a spend is only funded when the two agree.

/// Three outcomes, in this order and no other:

/// 1. `--multisig-address` was passed -- it WINS, is used exactly as given, and the binding is not
/// even read. Every existing invocation and script therefore behaves identically, and a machine
/// that binds a wallet later cannot change where an explicit command spends from.
/// 2. No address, and a binding exists FOR THIS NETWORK -- the bound Hot and its recorded secret
/// file are used.
/// 3. No address and no binding for this network -- [`E_WALLET_NOT_CONFIGURED`], raised here rather
/// than as a clap "missing argument", because the operator's next move is a setup command and
/// not a different flag.

/// A binding for the OTHER network is none of the three and is never outcome 2. Bindings are stored
/// per network, so a mainnet command does not even name that network's record; and a record that is
/// somehow in the wrong slot is refused by [`WalletStore::load_active`] rather than followed. Both
/// halves exist because the failure they prevent is a real spend from a wallet the operator bound
/// for a different chain -- the shipped code kept one global binding and had neither (, audit
/// item 3).

/// A binding file that EXISTS but does not parse is none of the three either: it propagates as the
/// read error [`WalletStore::load_active`] raises. Treating it as "no wallet" would send the
/// operator into onboarding while a real Hot, possibly holding funds, is already bound.

/// [`E_WALLET_NOT_CONFIGURED`]: dexdo_core::error_codes::E_WALLET_NOT_CONFIGURED
pub(crate) fn resolve_funding_wallet(
    store: &WalletStore,
    network: &WalletNetwork,
    explicit_address: Option<&str>,
    explicit_key: &Option<std::path::PathBuf>,
    explicit_seed_file: &Option<std::path::PathBuf>,
) -> Result<FundingWallet> {
    let wallet = resolve_funding_wallet_identity(
        store,
        network,
        explicit_address,
        explicit_key,
        explicit_seed_file,
    )?;
    if explicit_address.is_none() && wallet.key.is_none() && wallet.seed_file.is_none() {
        let binding = store.require_active(network)?;
        return Err(WalletBindingCannotSign::for_binding(&binding).into());
    }
    Ok(wallet)
}

/// Resolve the public funding identity, onboarding only when no binding exists.

/// Unlike [`resolve_funding_wallet_or_onboard`], a valid binding without a local credential is a
/// successful identity lookup. `note deploy` decides after loading recovery whether this run will
/// sign anything; only that branch may reject the missing credential.
pub(crate) async fn resolve_funding_wallet_identity_or_onboard(
    store: &WalletStore,
    network: &WalletNetwork,
    explicit_address: Option<&str>,
    explicit_key: &Option<std::path::PathBuf>,
    explicit_seed_file: &Option<std::path::PathBuf>,
    purpose: &str,
) -> Result<FundingWallet> {
    let refusal = match resolve_funding_wallet_identity(
        store,
        network,
        explicit_address,
        explicit_key,
        explicit_seed_file,
    ) {
        Ok(wallet) => return Ok(wallet),
        Err(refusal) => refusal,
    };
    if !is_wallet_not_configured(&refusal) || !onboarding_can_be_run() {
        return Err(refusal);
    }
    eprintln!(
        "no {} wallet is bound yet, and {purpose} spends from one. Starting wallet \
         onboarding now; it continues by itself once the wallet answers.",
        network.as_str()
    );
    run_selected(
        WalletAction::Onboard,
        WalletProvider::AckinackiWallet,
        None,
        false,
    )
    .await?;
    resolve_funding_wallet_identity(
        store,
        network,
        explicit_address,
        explicit_key,
        explicit_seed_file,
    )
}

/// Resolve the funding wallet, and where there is none, onboard one instead of dead-ending.

/// The commands that call this -- deploying a note, posting an order -- are what an operator
/// actually sets out to do; binding a wallet is not a goal, it is the thing that has to exist
/// first. Refusing with "run `dexdo wallet onboard`" makes the operator run a second command whose
/// only purpose is to let the first one proceed, so the first one runs it.

/// Two conditions, both required. The refusal must be exactly "no wallet is configured": a corrupt
/// binding, a binding with no local key, a binding for the other network are all different
/// problems, and onboarding over any of them would bind a second wallet while the operator's first
/// one is still there. And the session must be interactive -- onboarding shows a code to scan with
/// a phone and then waits, which a script or a machine consumer cannot do. Anywhere else this stays
/// the refusal it has always been.
pub(crate) async fn resolve_funding_wallet_or_onboard(
    store: &WalletStore,
    network: &WalletNetwork,
    explicit_address: Option<&str>,
    explicit_key: &Option<std::path::PathBuf>,
    explicit_seed_file: &Option<std::path::PathBuf>,
    purpose: &str,
) -> Result<FundingWallet> {
    let refusal = match resolve_funding_wallet(
        store,
        network,
        explicit_address,
        explicit_key,
        explicit_seed_file,
    ) {
        Ok(wallet) => return Ok(wallet),
        Err(refusal) => refusal,
    };
    if !is_wallet_not_configured(&refusal) || !onboarding_can_be_run() {
        return Err(refusal);
    }
    eprintln!(
        "no {} wallet is bound yet, and {purpose} spends from one. Starting wallet \
         onboarding now; it continues by itself once the wallet answers.",
        network.as_str()
    );
    // Onboarding reached from inside another command: its result is for the person who is being
    // interrupted mid-purchase, never a machine contract, so the human layer is what they get.
    run_selected(
        WalletAction::Onboard,
        WalletProvider::AckinackiWallet,
        None,
        false,
    )
    .await?;
    resolve_funding_wallet(
        store,
        network,
        explicit_address,
        explicit_key,
        explicit_seed_file,
    )
}

/// Is this the "nothing is bound" refusal, and not one of the other ways a wallet can be unusable?

/// Compiled under every test build, not only the chain build: it calls nothing from the chain half, and
/// the tests that pin which refusals may start an onboarding run in both configurations.
fn is_wallet_not_configured(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<dexdo_core::DexdoError>()
        .is_some_and(|coded| {
            coded.code() == dexdo_core::error_codes::E_WALLET_NOT_CONFIGURED.code()
        })
}

/// Onboarding shows a code to scan and then waits for a phone, so it needs a real operator on the
/// other end. Stdin decides -- it is what the flow reads -- and stderr, which is where the code is
/// drawn.

/// `--non-interactive` counts here too: onboarding is a question -- scan this, then confirm on your
/// phone -- and a run told not to ask must refuse rather than start one. Both live in
/// `super::interaction`, which also latches what the operator's screen was before `note deploy`
/// points descriptor 2 at the prover's fold.

/// Compiled under every test build for the same reason as the check above.
fn onboarding_can_be_run() -> bool {
    super::interaction::may_ask()
}

/// The network the onboarding attempt is for, read from the provider subcommand's own `--network`.

/// A provider chosen from the interactive menu carries no arguments at all, and every flow that can
/// run that way already applies the chain build as its default; this repeats that default rather than
/// inventing a second one, so the network the store is asked about is the network the flow will
/// record in the binding it builds.

/// It is also the network a resumable draft has to match, for the same reason: an attempt started
/// against one chain must not be continued by a command pointed at the other.
/// The network every wallet flow binds on: the manifest's, whichever provider was chosen.

/// It used to differ by provider, because each carried its own `--network` with a compiled-in
/// default. A provider is a way of PROVING a Hot, not a chain it lives on.
fn selected_network(_explicit: Option<&WalletProviderCommand>) -> Result<WalletNetwork> {
    network_from_manifest()
}

async fn run_selected(
    action: WalletAction,
    provider: WalletProvider,
    explicit: Option<&WalletProviderCommand>,
    json: bool,
) -> Result<()> {
    let store = WalletStore::open()?;
    // Which network this attempt is binding, from the provider's own `--network`. Bindings are kept
    // per network, so "already bound" and "nothing to replace" are questions about THIS network:
    // a binding for one network must not block onboarding a mainnet wallet, and rebinding one must not
    // require the other to exist. The default matches the one each provider flow already applies to
    // an interactive selection carrying no arguments.
    let network = selected_network(explicit)?;
    match action {
        // Both arms ask whether a RECORD is there, not whether it validates. A binding whose id
        // names nothing still occupies the active file, so onboarding must still refuse to write
        // over it, and rebind -- the remediation the validation refusal names -- must still be able
        // to replace it. Validation belongs on the path that RESOLVES a binding into money
        // decisions, which is `resolve_funding_wallet`.
        WalletAction::Onboard => {
            if let Some(active) = store.read_active_record(&network)? {
                bail!(
                    "this instance is already bound to provider `{}` on {network} (binding {}, \
                     Hot {}); `dexdo wallet onboard` changes nothing on purpose. To replace it, \
                     run `dexdo wallet rebind` with the {provider} provider -- the current binding \
                     is archived, not deleted, because funds can still sit in the old Hot.",
                    active.provider,
                    active.id,
                    active.hot_address,
                );
            }
        }
        // Nothing to replace is not "start from scratch": it is the same missing-binding state
        // every wallet-dependent command reports, with the same code and the same remediation.
        WalletAction::Rebind => {
            store.require_active_record(&network)?;
        }
    }

    let draft = match resumable_binding_id(action, provider, &store, explicit) {
        Some(id) => store.adopt_draft(&id)?,
        None => store.open_draft()?,
    };
    let outcome = provider_flow(provider, &draft, explicit).await;
    if outcome.is_err() {
        draft.discard_if_empty();
    }
    let binding = outcome?;

    let archived = commit_onboarded(&store, &draft, &binding)?;
    // The attempt is finished, so it is no longer an attempt anything may resume. Retiring the
    // draft here rather than inside the flow means it happens exactly once, and only after the
    // binding it describes is actually the active one.
    retire_resumable_draft(provider, &draft);
    // The paths are printed, never their contents: the operator does not have to know the platform
    // data directory to find the binding, and nothing here reveals a secret.

    // This is the RESULT layer of, not narration, and it is printed rather than
    // logged. 681 names the wallet binding as one of the three artifacts a command must state --
    // "the addresses of Vault and Hot, the network, and where the material that signs for this
    // binding lives" -- under the rule that decides the layer: if the operator cannot find it again
    // without the client, it belongs in the result. The binding id and the Hot address are exactly
    // that. Demoting them to `info` left `wallet rebind` printing two lines, the secret file and
    // the archive, and naming neither the binding it had just created nor the Hot it now spends
    // from; 681 also rules out fixing that with a log level, because the filter is global and lets
    // other crates' lines through with ours. Only the ackinacki-wallet provider prints a result of
    // its own (`wallet_onboarding::print_handoff`), so for `manual` and `gosh-ai` these two lines
    // are the whole result.
    // Two audiences, two contracts, and the choice is the caller's rather than ours to guess.

    // `--json` is's stable object for a runtime supervising dexdo; the block below is
    // 's result for a person. They are never mixed on the same stream: a machine
    // reading one JSON document must not have a heading and four styled fields in front of it.
    if json {
        // Paths belong here and NOT in the human result: a caller that automates the client does
        // need to know where the binding and its secrets landed, and this is the contract that can
        // say so without three absolute paths reaching an operator who just bound a wallet.
        let object = serde_json::json!({
            "hot": dexdo_core::address::display(&binding.hot_address),
            "network": binding.network.as_str(),
            "provider": binding.provider.as_str(),
            "binding_id": draft.id(),
            "binding_file": store.binding_path(&binding.network).display().to_string(),
            "secrets_dir": draft.dir().display().to_string(),
            "archived": archived.as_ref().map(|path| path.display().to_string()),
        });
        println!("{object}");
        return Ok(());
    }

    // 681, the shape it spells out for this exact command: a heading that says what happened,
    // then fields the operator copies. The address goes in whole and canonical, because that is
    // the form addresses travel between commands in.
    use crate::cli::style::{self, Role};
    let palette = style::Palette::stdout();
    let mut result = style::paint(
        palette,
        Role::Bold,
        &format!("Wallet bound on {}", binding.network),
    );
    result.push('\n');
    result.push_str(&style::field(
        palette,
        "Hot",
        &dexdo_core::address::display(&binding.hot_address),
        Role::Id,
    ));
    result.push('\n');
    result.push_str(&style::field(
        palette,
        "provider",
        &binding.provider.to_string(),
        Role::Text,
    ));
    result.push('\n');
    result.push_str(&style::field(palette, "binding", draft.id(), Role::Id));
    result.push('\n');
    // The one path 681 REQUIRES here, and the one this command dropped. The artifact list names a
    // wallet binding as "the Vault and Hot addresses, the network, and where the material that
    // signs for this binding lives", under the rule the list is derived from: if the operator
    // cannot find it again without the client, it is in the result. An id is a name, not a place --
    // resolving it to a directory takes knowing which data directory this instance used, which is
    // the one thing an operator with three of them gets wrong. `wallet show` in this same file
    // already prints this field; onboarding printed it before this PR and stopped, so the two
    // commands answered the same question differently.

    // This is not the OTHER path. The binding record -- `wallet/active/<network>.json` -- stays out
    // under: the client wrote it, the client finds it, and the operator never has to.
    result.push_str(&style::field(
        palette,
        "secret",
        &draft.dir().display().to_string(),
        Role::Meta,
    ));
    result.push('\n');
    result.push_str(&style::field(
        palette,
        "next",
        &style::action(palette, "dexdo note deploy --nominal N100"),
        Role::Text,
    ));
    result.push('\n');
    print!("{result}");

    // The paths go to the log, not to the operator. 681: "paths to files the client manages itself
    // do not reach the result -- it finds them without the operator". They used to be printed
    // because tests read stdout and a test that wanted a path was reason enough to show one; the
    // owner's rule settles that the other way round -- a test that needs a path runs with RUST_LOG
    // and reads the log, rather than the operator paying for the test's convenience with three
    // lines of absolute paths above the one fact they came for.
    tracing::info!(
        binding_file = %store.binding_path(&binding.network).display(),
        secrets_dir = %draft.dir().display(),
        binding_id = %draft.id(),
        hot = %binding.hot_address,
        network = %binding.network,
        "wallet binding committed"
    );
    if let Some(path) = archived {
        tracing::info!(
            archived_at = %path.display(),
            "previous binding archived; its secrets are kept, not deleted"
        );
    }

    // Onboarding is the one moment an operator expects setup to cost something, so the 64 MB
    // reference string is fetched here rather than inside the first command that moves money --
    // where an interruption costs a re-run of a funding decision, not of a download.

    // Only the reference string. The proving key is built inside the first proof and the prover
    // exposes no way to build it on its own, so it stays where that proof is.
    {
        // Its own display: the flow's ended with the binding, and this step runs after it. One
        // step, so the line says what it is doing and how long it has been at it -- which is the
        // whole question while a 487 MB file comes down.
        let _status = crate::cli::progress::Status::with_plan(
            crate::cli::wallet_onboarding::ONBOARD_STEPS[3].0,
            [crate::cli::wallet_onboarding::ONBOARD_STEPS[3]],
        );
        crate::cli::note_cmd::prepare_prover_reference_string().await?;
    }

    Ok(())
}

/// Onboarding now carries the proving reference string, so the regression lives beside the flow
/// that carries it rather than beside the download it delegates to.
#[cfg(test)]
mod prover_preparation_tests {
    /// The whole point: the operator pays for the reference string at onboarding, not inside the
    /// first command that moves money. Pinned by the call being reachable from this module, so
    /// deleting it from the flow fails to compile rather than silently moving the cost back.
    #[test]
    fn onboarding_prepares_the_reference_string() {
        let prepare: fn() -> _ = || crate::cli::note_cmd::prepare_prover_reference_string();
        let _ = prepare;
    }
}

/// The id of an unfinished attempt this command may continue, when there is one.

/// # Why this decides here and not inside the provider flow

/// The store reserves the id BEFORE the flow runs, and `commit_onboarded` refuses any binding that
/// comes back carrying a different one. So "resume" cannot be a decision the flow makes privately:
/// by the time it has read its own draft, the id it must commit under is already fixed. Asking here
/// is what lets a resumed attempt end in a committed binding instead of a refusal.

/// Restricted to `onboard`, and to the one provider that writes a draft:

/// - `rebind` exists to bind a DIFFERENT wallet, so silently continuing a wait the operator started
/// earlier -- possibly for a Hot they have since abandoned -- would be the opposite of what they
/// asked for. It mints a fresh id, as it always has.
/// - `ackinacki-wallet` has its own durable session file and resumes through that; `manual` stores
/// no secret of its own and has nothing to resume.
fn resumable_binding_id(
    action: WalletAction,
    provider: WalletProvider,
    store: &WalletStore,
    explicit: Option<&WalletProviderCommand>,
) -> Option<String> {
    if action != WalletAction::Onboard || provider != WalletProvider::GoshAi {
        return None;
    }
    crate::cli::wallet_goshai::files::find_resumable(
        store.root(),
        &selected_network(explicit).ok()?,
    )
}

/// Retire the finished attempt's resume marker. See [`resumable_binding_id`] for who writes one.
fn retire_resumable_draft(provider: WalletProvider, draft: &BindingDraft) {
    if provider == WalletProvider::GoshAi {
        crate::cli::wallet_goshai::files::discard_draft_in(draft.dir());
    }
}

/// Commit the binding this attempt proved, under the identity the store RESERVED for it.

/// The reserved id is the name of the secrets directory `open_draft` created, so a binding that
/// carries a different id is not a cosmetic mismatch: `binding.json` would name a directory that
/// does not exist, while the directory holding the attempt's key material would be referenced by
/// nothing. The operator's Hot would then be unreachable through the active binding -- money-path
/// damage, written silently. Refusing keeps the previous binding active and leaves the reserved
/// directory and everything in it exactly where a retry can find them.

/// This is a single point rather than a rule each provider flow is trusted to follow: the flows
/// build the binding, and one of them minting its own id is precisely how this went wrong.
fn commit_onboarded(
    store: &WalletStore,
    draft: &BindingDraft,
    binding: &WalletBinding,
) -> Result<Option<std::path::PathBuf>> {
    if binding.id != draft.id() {
        bail!(
            "the `{}` onboarding flow returned binding id {} while this attempt reserved {}, whose \
             secrets directory is {}. Recording the first would leave the active binding naming a \
             directory that does not exist and the reserved one referenced by nothing, so nothing \
             was written and the previous binding, if any, is untouched",
            binding.provider,
            binding.id,
            draft.id(),
            draft.dir().display(),
        );
    }
    store.commit_active(binding)
}

/// Run one provider's onboarding and return the binding it proved.

/// is delivered in stages. `gosh-ai` is wired; `ackinacki-wallet` and `manual` are not, and
/// their refusal names what still works today so nobody is left without a funding path.

/// The flow BUILDS a binding and never writes `wallet/binding.json`: the caller commits it through
/// [`WalletStore::commit_active`], which archives whatever it replaces. Two writers would mean the
/// second one renaming over the only local record of a Hot that can still hold funds.
async fn provider_flow(
    provider: WalletProvider,
    draft: &BindingDraft,
    explicit: Option<&WalletProviderCommand>,
) -> Result<WalletBinding> {
    match provider {
        WalletProvider::GoshAi => goshai_flow(draft, explicit).await,
        WalletProvider::Manual => manual_flow(draft, explicit).await,
        WalletProvider::AckinackiWallet => ackinacki_flow(draft, explicit).await,
    }
}

/// Translate the clap surface into what the Gosh.ai flow takes, and run it.

/// The binding id is the one the STORE reserved for this attempt, not a fresh one: the store
/// created the owner-only directory under that id and discards it when the attempt writes nothing,
/// so a second id would leave that directory empty and put the recovery phrase somewhere the store
/// does not know about.

/// A provider chosen from the interactive menu carries no payload, so the defaults are used -- the
/// same ones clap would have applied.
/// The Acki Nacki Wallet provider: the bee session, the QR, and the validated Vault/Hot pair.

/// This used to be reached by its own dispatch arm in `main.rs`, which meant it never passed through
/// `run_selected` and therefore never produced a binding -- the flow ran to completion and left the
/// operator exactly as unconfigured as before. Routing it here is the specification's step 9: all
/// three providers reach one `provider_flow`, one writer and one archive path.

/// The binding id is the store's, as it is for gosh-ai: the store reserved a directory under it and
/// discards it when the attempt writes nothing.

/// Every argument now has a default, so this provider is reachable from the interactive menu like
/// the other two. It used to refuse there, because an agent name and two owner-only paths had to be
/// typed and a menu has no command line to carry them -- which made choosing `ackinacki-wallet` from
/// the menu a dead end that only told the operator to start again. The paths default into the
/// secrets directory the store reserved for this attempt, exactly where the gosh-ai provider writes
/// its own secret, and the agent name defaults to the constant the durable session can be resumed
/// under.
async fn ackinacki_flow(
    draft: &BindingDraft,
    explicit: Option<&WalletProviderCommand>,
) -> Result<WalletBinding> {
    crate::cli::wallet_onboarding::run_wallet_onboard(
        ackinacki_onboard_args(explicit, draft.dir()),
        draft.id(),
    )
    .await
}

/// The onboarding request this provider makes, as a pure function of the command line.

/// # Why this is separated from [`ackinacki_flow`]

/// So that the MENU path can be asserted by what it PRODUCES. `ackinacki_flow` itself cannot be
/// driven from a test that runs under the features CI uses: it is the chain build-only, it is `async`,
/// and it reaches a chain. That left the source text as the only thing a test could inspect, and an
/// assertion that some refusal phrase is *absent* from a file is satisfied by rewording it, by
/// respacing it, or by moving it one module over -- it proves the string is gone, never that the
/// operator gets a runnable request.

/// `explicit` is `None` exactly when the provider came from the interactive provider menu, which
/// carries no command line at all. That arm is the whole subject of follow-up item 6: it must
/// produce the same request the bare subcommand makes -- every clap default, including the two
/// canonical filenames resolved inside this attempt's binding draft.
fn ackinacki_onboard_args(
    explicit: Option<&WalletProviderCommand>,
    binding_dir: &std::path::Path,
) -> crate::cli::args::WalletOnboardArgs {
    match explicit {
        Some(WalletProviderCommand::AckinackiWallet(args)) => crate::cli::args::WalletOnboardArgs {
            agent_name: args.agent_name.clone(),
            state: Some(args.state.clone().unwrap_or_else(|| {
                binding_dir.join(dexdo_core::params::DEFAULT_WALLET_ONBOARD_STATE_PATH)
            })),
            hot_key: Some(args.hot_key.clone().unwrap_or_else(|| {
                binding_dir.join(dexdo_core::params::DEFAULT_WALLET_ONBOARD_HOT_KEY_PATH)
            })),
            vault_key: args.vault_key.clone(),
            qr_file: args.qr_file.clone(),
            terminal_qr: args.terminal_qr,
        },
        // The menu carries no payload, so this is the same request the bare subcommand makes:
        // every default, including the two canonical paths the flow resolves under the effective
        // data directory.
        _ => crate::cli::args::WalletOnboardArgs {
            agent_name: dexdo_core::params::WALLET_ONBOARD_DEFAULT_AGENT_NAME.to_string(),
            // The menu carries no command line, so the manifest is the one the client finds.
            state: Some(binding_dir.join(dexdo_core::params::DEFAULT_WALLET_ONBOARD_STATE_PATH)),
            hot_key: Some(
                binding_dir.join(dexdo_core::params::DEFAULT_WALLET_ONBOARD_HOT_KEY_PATH),
            ),
            vault_key: None,
            qr_file: None,
            terminal_qr: false,
        },
    }
}

/// follow-up item 6: the owner-only defaults, and the menu path they unblock.
#[cfg(test)]
mod ackinacki_defaults_tests;

/// The manual provider: an existing Hot, connected by address plus a local secret file.

/// Like the Gosh.ai arm it BUILDS a binding and never writes one -- `run_selected` commits it
/// through `WalletStore`, which archives what it replaces.

/// The binding id is the STORE's, exactly as it is for the other two providers. This flow used to
/// mint a second one of its own, on the reasoning that it generates no key material and so strands
/// no secret. That reasoning was wrong about the file it does not write and silent about the file
/// it does: the store had already reserved an id and created a directory under it, the command
/// printed THAT id as the binding's, and `binding.json` then recorded the other one. The active
/// binding named a directory that did not exist, so the operator's funded Hot could not be reached
/// through it -- which is the guarantee the reserve-before-any-key rule exists to give.

/// A provider chosen from the interactive menu carries no payload; the manual flow's arguments are
/// all optional by design and it asks on a terminal, so the defaults are exactly right there.
async fn manual_flow(
    draft: &BindingDraft,
    explicit: Option<&WalletProviderCommand>,
) -> Result<WalletBinding> {
    let args = match explicit {
        Some(WalletProviderCommand::Manual(args)) => crate::cli::args::WalletOnboardManualArgs {
            multisig_address: args.multisig_address.clone(),
            multisig_private_key: args.multisig_private_key.clone(),
            multisig_seed_file: args.multisig_seed_file.clone(),
        },
        _ => crate::cli::args::WalletOnboardManualArgs {
            multisig_address: None,
            multisig_private_key: None,
            multisig_seed_file: None,
        },
    };
    crate::cli::wallet_manual::run_wallet_onboard_manual(args, draft.id()).await
}

/// The Gosh.ai flow proves the Hot on chain before it binds, so it exists only where there is a
/// chain backend. In a default build the provider is still selectable -- that is argument parsing --
/// and refuses here for the same reason every other money path does.
async fn goshai_flow(
    draft: &BindingDraft,
    explicit: Option<&WalletProviderCommand>,
) -> Result<WalletBinding> {
    use crate::cli::wallet_goshai::{run_wallet_onboard_goshai, GoshAiOnboardOptions};

    let args = match explicit {
        Some(WalletProviderCommand::GoshAi(args)) => Some(args.clone()),
        _ => None,
    };
    run_wallet_onboard_goshai(GoshAiOnboardOptions {
        network: network_from_manifest()?,
        binding_id: draft.id().to_string(),
        activation_timeout: args
            .as_ref()
            .and_then(|a| a.activation_timeout)
            .unwrap_or(GoshAiOnboardOptions::DEFAULT_ACTIVATION_TIMEOUT),
        data_dir: crate::cli::data_dir::effective()?,
    })
    .await
}

#[cfg(test)]
mod wallet_334_tests;

/// the id in `binding.json` is the id the store reserved, and its secrets are reachable
/// through it.
#[cfg(test)]
mod reserved_binding_id_tests;

/// the fail-fast as `note deploy` and `note topup` actually reach it.
#[cfg(test)]
mod fail_fast_wiring_tests;

/// the reader validates the id, so a record naming nothing never resolves as the wallet.
#[cfg(test)]
mod binding_load_validation_tests;

/// audit item 3: the binding is kept per network, and a command running on one network never
/// resolves -- and never spends -- a wallet bound on another.
#[cfg(test)]
mod per_network_binding_tests;

/// The label that keys those per-network files is a FILE NAME, and turned it into the
/// manifest's own string. `Path::join` gives an absolute or `..`-bearing label the whole disk.
#[cfg(test)]
mod a_label_is_a_file_name_not_a_path_1640;

/// re-audit item 8: permanent removal is all-zero-only and preserves every byte on refusal.
#[cfg(test)]
mod item8_removal_tests;

/// The balance proof behind that removal is read from a URL, whatever form `--endpoint` arrived in.
/// Gated with the code it covers: the function under test exists only under the chain build.
#[cfg(test)]
mod endpoint_tests;

#[cfg(test)]
mod onboard_endpoint_source_1839_tests;

/// Two distinct network labels for fixtures, named after nothing.

/// **Kept at the end of the file on purpose.** `wallet_binding_result_681.rs` reads this file as
/// TEXT and takes the production half to be everything before the first `#[cfg(test)]`. Declared
/// higher up, these two helpers cut `run_selected` out of what that test can see, and it failed
/// with `run_selected present` -- which names the symptom and not the cause.

/// Tests used to build `test_network_a()` and `::Mainnet`, which put the names of real
/// chains into the sources for no reason -- what every one of those tests is about is that a
/// binding on ONE label is not a binding on ANOTHER. Neutral labels say that and cannot drift into
/// a claim about a chain. A test that genuinely needs the network it is running on reads it from
/// the manifest, like production does.
#[cfg(test)]
pub(crate) fn test_network_a() -> WalletNetwork {
    WalletNetwork::from_manifest_label("net-a").expect("a non-empty label")
}

#[cfg(test)]
pub(crate) fn test_network_b() -> WalletNetwork {
    WalletNetwork::from_manifest_label("net-b").expect("a non-empty label")
}
