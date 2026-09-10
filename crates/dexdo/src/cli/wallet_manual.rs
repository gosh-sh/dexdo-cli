//! Manual wallet provider: bind a Hot multisig the operator already has.

//! # Origin is recorded, never inferred

//! A wallet's provider cannot be read back off the chain -- Acki Nacki Wallet, Gosh.ai and an
//! operator's own deploy all produce the same canonical multisig. So the provider is written down
//! once, at an explicit onboarding, and never guessed afterwards. The rule that keeps it honest is
//! structural rather than a convention: [`save_active_binding`] is private to this module and takes
//! a [`VerifiedManualWallet`], which only [`verify_manual_hot_wallet`] can build. No other module
//! can name either. A working command that happens to receive `--multisig-address` and a key file
//! therefore cannot turn those flags into a binding, whatever it does with them.

//! # What is verified before anything is written

//! In this order, so the first thing an operator is told is the first thing that is wrong:

//! 1. the supplied address parses (canonical `<dapp_id>::<account_id>`, or legacy `0:<64 hex>`);
//! 2. the account exists and is `Active`;
//! 3. its `code_hash` is one of the supported multisig spending code hashes;
//! 4. `getParameters().requiredTxnConfirms == 1`;
//! 5. the public key derived from the supplied secret file is one of `getCustodians()`.

//! A binding naming a wallet we cannot sign for is worse than no binding, so all five must pass
//! before the file exists at all.

//! # What the binding stores, and what it only references

//! Stored: schema version, binding id, provider, network, and the full canonical Hot address.
//! Referenced: the absolute path of the operator's owner-only secret file, under `hot_key_file` or
//! `hot_seed_file` depending on which one it is. Never stored: the secret. The file is written
//! through the existing atomic private-write helper, so it lands 0600 by rename or not at all.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli::args::WalletOnboardManualArgs;

use crate::cli::wallet::BINDING_VERSION;

/// Directory holding every durable wallet artifact under the effective data dir.
const WALLET_DIR: &str = "wallet";

/// The one active binding. Replaced atomically; never merged.
const BINDING_FILE: &str = "binding.json";

// the provider vocabulary and the binding schema are PR1287's, imported rather than
// redefined. This module shipped its own copies before that type existed and its author expected
// the integrator to replace them: two shapes over one `<data-dir>/wallet/binding.json` is a file
// two types disagree about, and `network` as a free string is a binding that can claim a chain
// nothing validated.
pub(crate) use crate::cli::wallet::{WalletBinding, WalletNetwork, WalletProvider};

/// Which of the two accepted secret files the operator pointed at.

/// The distinction survives into the binding because the two are read differently: a raw 32-byte
/// secret, or a phrase that has to go through TVM derivation first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManualSecretKind {
    /// A file holding the wallet's 32-byte secret hex.
    Key,
    /// A file holding the wallet's seed phrase.
    SeedPhrase,
}

impl ManualSecretKind {
    const fn flag(self) -> &'static str {
        match self {
            Self::Key => "--multisig-private-key",
            Self::SeedPhrase => "--multisig-seed-file",
        }
    }
}

/// The operator's secret file: which kind it is and where it lives. The contents never enter here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManualSecretRef {
    pub(crate) kind: ManualSecretKind,
    pub(crate) path: PathBuf,
}

/// The multisig facts one on-chain read pass yields, separated from how they were read.

/// Keeping them a plain value is what lets the whole decision below be exercised without a chain:
/// the chain layer only fills this in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObservedHotWallet {
    /// `acc_type` as the account reader reports it.
    pub(crate) status: String,
    /// The deployed code hash, absent on an account that has none yet.
    pub(crate) code_hash: Option<String>,
    /// `getParameters().requiredTxnConfirms`.
    pub(crate) required_txn_confirms: u8,
    /// `getCustodians().custodians[].owner_pubkey`, as rendered by the getter.
    pub(crate) custodian_pubkeys: Vec<String>,
}

/// A Hot that passed every check in the module header, and the binding that describes it.

/// The field is private and the type is only ever produced by [`verify_manual_hot_wallet`], so
/// [`save_active_binding`] cannot be reached with an unverified wallet from anywhere.
#[derive(Debug)]
pub(crate) struct VerifiedManualWallet {
    binding: WalletBinding,
}

impl VerifiedManualWallet {
    pub(crate) fn binding(&self) -> &WalletBinding {
        &self.binding
    }
}

/// `<data-dir>/wallet/binding.json`.
pub(crate) fn active_binding_path(data_dir: &Path) -> PathBuf {
    data_dir.join(WALLET_DIR).join(BINDING_FILE)
}

/// Read the active binding, if one has been onboarded.

/// A present but unreadable binding is an error, not a `None`: silently behaving as "no wallet
/// configured" would send a working command down the fail-fast path while the operator's real Hot
/// sits recorded on disk.
pub(crate) fn load_active_binding(data_dir: &Path) -> Result<Option<WalletBinding>> {
    let path = active_binding_path(data_dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => bail!("read wallet binding {}: {error}", path.display()),
    };
    let binding: WalletBinding = serde_json::from_slice(&bytes)
        .map_err(|error| anyhow::anyhow!("parse wallet binding {}: {error}", path.display()))?;
    if binding.version != BINDING_VERSION {
        bail!(
            "wallet binding {} has version {}, this build understands version {BINDING_VERSION}",
            path.display(),
            binding.version
        );
    }
    Ok(Some(binding))
}

/// What `wallet onboard manual` must do next, decided from one read of the account.

/// The operator does not choose between "deploy mine" and "bind the existing one" -- the chain
/// already knows which of the two this is, and asking them to say it a second time is how the wrong
/// one gets picked. So the decision is a function of the account state, and it is pure: every test
/// below drives it without a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManualOnboardStep {
    /// The account holds enough native gas to deploy. Deploy, then bind.
    Deploy,
    /// The account exists and is deployed. Verify and bind; write nothing to the chain.
    BindExisting,
    /// Nothing is deployed and there is not enough gas to deploy it. Ask, wait, then deploy.
    AwaitFunding {
        /// What the account holds now, in raw native units.
        available_raw: u128,
    },
}

/// The native gas an operator is asked to send before the wallet can be deployed.

/// Two vmshell rather than the deploy's own cost: the deploy takes 0.153501 -- measured on the chain
/// and on mainnet, equal to the raw unit -- and what remains is the wallet's own gas for the
/// operations that follow. Asking for exactly the deploy cost leaves a wallet that cannot send its
/// first message.
pub(crate) const MANUAL_DEPLOY_REQUEST_RAW: u128 = 2_000_000_000;

/// A native-gas figure, in the words this tree reserves for it.

/// Native gas is `vmshell`, and SHELL is ECC[2] -- a currency the wallet holds separately and can
/// spend. They are printed as two different lines everywhere else (`note.rs`), because an operator
/// who reads "SHELL" and sends ECC[2] credits a balance this wait never looks at: `account.balance`
/// does not move, the poll runs its full course, and the refusal names a balance the operator can
/// see they funded.
fn vmshell_amount(raw: u128) -> String {
    format!("{}.{:09} vmshell", raw / 1_000_000_000, raw % 1_000_000_000)
}

/// Decide the step from the account as it was read.

/// `status` is `acc_type` verbatim, `native_raw` the native balance. A deployed account is anything
/// the chain calls `Active`; everything else is either fundable or already funded.
pub(crate) fn manual_onboard_step(status: &str, native_raw: u128) -> ManualOnboardStep {
    if status.eq_ignore_ascii_case("Active") {
        return ManualOnboardStep::BindExisting;
    }
    if native_raw >= dexdo_core::params::OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE {
        return ManualOnboardStep::Deploy;
    }
    ManualOnboardStep::AwaitFunding {
        available_raw: native_raw,
    }
}

/// Wait for the transfer, or refuse because nobody can make it.

/// Nobody to ask means nothing to wait for: a prompt that cannot be answered is a hang, and this
/// one would hang holding the operator's money path open -- an uninit address, a key on disk and a
/// command that never returns.

/// A named decision rather than an `if` inside the wait, so that it can be TESTED. Measured by a
/// reviewer: mutating that `if` to `if false` dropped zero tests out of 1307, because the function
/// holding it is reachable only from the product and the refusal's own test called the renderer
/// directly. The words were proven; the refusal happening was not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ManualFundingWait {
    /// Someone can be asked to send, so the command waits and watches the balance.
    Wait,
    /// No terminal and nobody to answer: refuse now, with the address and the amount.
    NobodyToAsk,
}

pub(crate) fn manual_funding_wait(may_ask: bool) -> ManualFundingWait {
    if may_ask {
        ManualFundingWait::Wait
    } else {
        ManualFundingWait::NobodyToAsk
    }
}

/// The request the operator reads while nothing has been written anywhere.

/// The address is whole and canonical because it is copied into a wallet application, and the
/// instruction is drawn with the client's one call-to-act colour -- amber and bold, the same one
/// every "this is yours to do" line uses.

/// **The network is named first, and named again inside the instruction.** The canonical
/// address is the same string on both chains, so nothing further down this path can tell the
/// operator they are about to pay the right address on the wrong chain -- the owner caught exactly
/// that by eye, with the transfer already composed. The field states it; the amber line, which is
/// the line people actually read before acting, states it again. This half works whatever the
/// wallet on the phone understands, which is why it is not left to the link alone.
pub(crate) fn render_manual_deploy_funding_request(
    address: &str,
    available_raw: u128,
    network: &str,
) -> String {
    use crate::cli::choose::{action, field};
    format!(
        "wallet not deployed yet\n{}\n{}\n{}\n{}",
        field("network", network),
        field("address", address),
        field(
            "holds",
            &format!("{} native gas", vmshell_amount(available_raw))
        ),
        field(
            "send",
            &action(&format!(
                "{} SHELL from your wallet on {network} to the address above -- scan the code \
                 below and the wallet ticks \"auto-convert to vmshell\" itself, which is what \
                 makes the SHELL arrive as the gas the deploy spends. Copying the address by hand \
                 instead means ticking that yourself: without it the SHELL arrives as currency, \
                 this command goes on waiting, and the money has already left. Then this command \
                 deploys the wallet itself",
                MANUAL_DEPLOY_REQUEST_RAW / dexdo_core::params::SHELL_UNIT
            )),
        ),
    )
}

/// Which message flag a payment link states, and therefore what the money becomes on arrival.

/// A named decision rather than a bare `Option<u8>` at four call sites, for the reason
/// [`ManualFundingWait`] gives: the two moments want OPPOSITE things and the difference cannot be
/// undone, so the choice is worth a type that a test can drive and a reader can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaymentFlag {
    /// Nothing is stated. The protocol defines an absent flag as `0`, an ordinary transfer, which
    /// is what an Active wallet being topped up must receive: spendable ECC[2] SHELL.
    None,
    /// `flag=16` -- the wallet converts the SHELL into native vmshell (gas) on arrival. Correct
    /// only for the deploy, whose address holds no contract yet and whose gas is the balance
    /// `manual_onboard_step` waits on. Irreversible: native vmshell cannot be spent as ECC[2]
    /// currency or converted back.
    RecipientGas,
}

/// The wallet's scan-only payment form for the gas this command is waiting on.

/// `token=2` is SHELL, and `flag=16` is what turns it into the native vmshell the deploy spends.
/// That flag is not decoration and not a hint: measured in `ledger.md`, ECC[2] sent at flag 1
/// arrives as ECC[2], and only flag 16 arrives as native vmshell. `manual_onboard_step` waits on
/// the NATIVE balance, so a transfer without it leaves this command waiting until timeout with the
/// operator's money already gone. Amount is human decimal, as the protocol requires -- not raw
/// units.
pub(crate) fn manual_deploy_payment_link(address: &str, network: &str) -> String {
    payment_link(
        address,
        MANUAL_DEPLOY_REQUEST_RAW / dexdo_core::params::SHELL_UNIT,
        network,
        PaymentFlag::RecipientGas,
    )
}

/// The wallet's scan-only payment form for any amount, in whole SHELL.

/// One builder for both moments a manual wallet needs money -- the deploy, and every later top-up
/// -- because they are the same act: the operator sends SHELL from a phone to an address the client
/// prints. They differed only in that the top-up path printed no code at all and left the operator
/// copying 130 characters out of a terminal.

/// Fields are APPENDED, never inserted. The compact form carries no `to=` key: the wallet reads
/// everything before the first `&` as the recipient, so order is not cosmetic here.

/// `network` and `flag` were both added by, after `ackinacki-wallet` shipped them
/// (`origin/rc/2`, `src/shared/qr/payment_uri.ts`). Until then the protocol had no field for either
/// and this builder deliberately carried neither.

/// **The label goes in exactly as the manifest declared it, and nothing here judges it.** An
/// earlier draft filtered it against the two values the wallet's parser accepts. That was wrong
/// twice over. It put network names back in a client constant, which the project's manifest
/// directive forbids in as many words -- not in a constant, not in a path -- and which `params.rs`
/// restates as the rule exists to hold. And it was the less safe of the two behaviours: a
/// manifest the wallet does not know silently loses the binding and the operator is back to a code
/// that names no chain, which is itself. Passing it through means the wallet either honours
/// it or refuses the request outright and says so. A refusal costs a scan; a silent omission costs
/// a transfer onto the wrong chain.

/// Private, together with [`PaymentFlag`], so no OTHER module can hand recipient gas to a top-up.
/// The reach is this module and its descendants, not literally the two builders above -- the test
/// module below is a descendant and calls it directly, which is the point of putting it there. A
/// future non-test submodule would get the same access, so the guard is the visibility, and the
/// visibility is what `only_the_deploy_ever_asks_for_recipient_gas` watches.
fn payment_link(address: &str, whole_shell: u128, network: &str, flag: PaymentFlag) -> String {
    let mut link = format!(
        "{address}&amount={whole_shell}&token={}",
        dexdo_core::params::SHELL_CURRENCY_ID,
    );
    let network = network.trim();
    if !network.is_empty() {
        link.push_str("&network=");
        link.push_str(&encoded_value(network));
    }
    if let PaymentFlag::RecipientGas = flag {
        link.push_str("&flag=16");
    }
    link
}

/// One field value, encoded as the payment protocol says field values are encoded.

/// **This is a containment boundary, not tidiness.** The label comes out of a manifest the operator
/// DOWNLOADS -- `dexdo-install` says so and `doctor`'s refusal calls it "a manifest you downloaded"
/// -- and since it can be any string. Appended raw it is not one value but arbitrary link
/// text: `WalletNetwork::from_manifest_label` only asks whether the label is a single plain path
/// component, and `&` and `=` are legal filename bytes, so `shellnet&flag=16` passes it. That label
/// turned a TOP-UP link into a flagged one -- every precondition the wallet checks was satisfied,
/// because the recipient is an extended address and the token is 2 -- and the operator's SHELL
/// would have landed on an Active wallet as native vmshell, unspendable as currency and not
/// convertible back. Found by review; `a_label_cannot_smuggle_a_second_field_into_the_link` holds
/// it.

/// Encoding rather than an allow-list of labels, deliberately: an allow-list would put network
/// names back in a client constant, and this states nothing about which chains exist. A strange
/// label now reaches the wallet as one strange VALUE and is refused as `invalid_network`, by name.

/// Unreserved characters (RFC 3986) pass through, so every ordinary label is byte-identical to what
/// it was before -- an encoder that escaped those would hand the wallet a value it cannot match and
/// break every payment it touches.
fn encoded_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Print a payment QR for `address` and `whole_shell`, with a line saying what it is for.

/// Used by the top-up path, which reaches this from a different module than the deploy does.
pub(crate) fn write_payment_qr(
    output: &mut dyn std::io::Write,
    address: &str,
    whole_shell: u128,
    network: &str,
) {
    write_payment_code(
        output,
        &payment_link(address, whole_shell, network, PaymentFlag::None),
    );
}

/// Draw one already-built payment link as a code, with the line that says what it is for.

/// Separate from the builders because the deploy and the top-up no longer encode the same string:
/// they differ by the message flag, and that difference is irreversible. Sharing the DRAWING while
/// splitting the BUILDING keeps both moments looking identical to the operator without letting the
/// deploy's `flag=16` reach an Active wallet.
fn write_payment_code(output: &mut dyn std::io::Write, link: &str) {
    // Both this and the live line write to stderr. A spinner frame redrawn across one row of half
    // blocks makes the symbol undecodable, and an operator points a camera at a code that will not
    // scan -- so the line comes down first, for good.
    crate::cli::progress::clear_live_line();
    let _ = writeln!(
        output,
        "  scan     to send from Acki Nacki Wallet on your phone:"
    );
    match crate::cli::qr_compact::smallest_code(link.as_bytes()) {
        Ok(code) => {
            if let Err(error) = crate::cli::qr_display::write_qr(output, &code) {
                let _ = writeln!(output, "  (the QR code could not be drawn: {error})");
            }
        }
        Err(error) => {
            let _ = writeln!(output, "  (the QR code could not be built: {error})");
        }
    }
}

/// Print the address as a QR code, under the request that asks for it.

/// The address is 130 characters and the wallet that has to receive the transfer is on a phone.
/// Nobody retypes that, and copying it out of a terminal into a phone is its own small ordeal --
/// so the camera does it. The text address stays above it: a terminal without a camera in front of
/// it still copies and pastes.

/// What is encoded is the wallet's own scan-only payment form, documented in
/// `ackinacki-wallet/docs/flows/qr_payment_protocol.md`:

/// ```text
/// <dapp64>::<account64>&amount=<decimal>&token=<tokenRoot>[&network=<mainnet|shellnet>][&flag=<0|16>]
/// ```

/// -- the compact shape the app accepts from external producers, with no `https://.../v1/pay?to=`
/// wrapper. It carries the amount, so the operator does not retype that either, and `token=2` is
/// SHELL. Scanning it opens the send flow with destination and amount already filled; the person
/// still confirms and signs.

/// The canonical `dapp::account` address is used deliberately -- the protocol's own example of this
/// form uses the extended address, it is the same string printed above the code, and the wallet
/// requires exactly that shape before it will honour a `flag` at all.

/// This is the DEPLOY code, so it states `flag=16`: the address holds no contract, and the gas the
/// deploy spends is what has to land. The top-up code states no flag -- see [`PaymentFlag`].

/// A failure to draw is not a failure to fund: the address is already on the screen, so a QR that
/// cannot be rendered is reported and stepped over rather than ending the run.
pub(crate) fn write_manual_deploy_funding_qr(
    output: &mut dyn std::io::Write,
    address: &str,
    network: &str,
) {
    write_payment_code(output, &manual_deploy_payment_link(address, network));
}

/// What a run that cannot wait says instead of waiting.

/// `--non-interactive` and a destination that is not a terminal both mean nobody is there to send
/// anything, so the wait would be a hang. The refusal carries the same two figures the request
/// carries, because the operator will act on them from a script or another window.

/// It names the network for the same reason the request does: this text is read where no
/// QR is drawn at all, so the only thing standing between the operator and the wrong chain is the
/// sentence.
pub(crate) fn render_manual_deploy_funding_refusal(
    address: &str,
    available_raw: u128,
    network: &str,
) -> String {
    format!(
        "wallet {address} on {network} is not deployed and holds {} of native gas, which is not \
         enough to deploy it. Send {} to that address, converting to vmshell as you send -- native \
         gas is vmshell, not the ECC[2] SHELL a wallet spends, and a transfer that does not convert \
         leaves this balance where it is -- then run the same command again. Nothing was written, \
         on the chain or on disk.",
        vmshell_amount(available_raw),
        vmshell_amount(MANUAL_DEPLOY_REQUEST_RAW),
    )
}

/// Decide, from the facts of one read pass, whether this Hot may be bound.

/// Every rejection names what was expected and states that nothing was written, because the
/// operator's next action differs per cause: a wrong address is retyped, a wrong threshold means a
/// Vault was passed where a Hot was wanted, and a non-custodian key means the wrong file.
pub(crate) fn verify_manual_hot_wallet(
    hot_address: &str,
    network: WalletNetwork,
    secret: ManualSecretRef,
    signer_pubkey: &str,
    observed: &ObservedHotWallet,
    binding_id: String,
) -> Result<VerifiedManualWallet> {
    let address = dexdo_core::CanonicalAddress::parse(hot_address)
        .map_err(|error| anyhow::anyhow!("--multisig-address: {error}; nothing was written"))?;
    let hot = address.to_string();

    if !observed.status.eq_ignore_ascii_case("Active") {
        bail!(
            "Hot wallet {hot} is not Active (acc_type={}); an address with no deployed multisig \
             behind it cannot be bound. Deploy or fund it first, then run this command again; \
             nothing was written",
            observed.status
        );
    }

    let code_hash = observed
        .code_hash
        .as_deref()
        .map(str::trim)
        .map(|hash| {
            hash.strip_prefix("0x")
                .or_else(|| hash.strip_prefix("0X"))
                .unwrap_or(hash)
                .to_ascii_lowercase()
        })
        .ok_or_else(|| {
            anyhow::anyhow!("Hot wallet {hot} is Active but has no code_hash; nothing was written")
        })?;
    if !dexdo_core::canonical_multisig::is_supported_spending_code_hash(&code_hash) {
        bail!(
            "Hot wallet {hot} has code_hash {code_hash}, which is not a supported multisig; dexdo \
             spends only from code_hash {} or {}. Nothing was written",
            dexdo_core::canonical_multisig::LEGACY_SPENDING_CODE_HASH,
            dexdo_core::canonical_multisig::CODE_HASH,
        );
    }

    if observed.required_txn_confirms != 1 {
        bail!(
            "Hot wallet {hot} requires {} transaction confirmations; a bound Hot must have \
             reqConfirms=1, because dexdo signs its spends alone. A wallet with a higher threshold \
             is a Vault: bind the Hot it funds instead. Nothing was written",
            observed.required_txn_confirms
        );
    }

    let signer = dexdo_core::normalize_multisig_pubkey(signer_pubkey).ok_or_else(|| {
        anyhow::anyhow!(
            "{} does not yield a usable public key for Hot wallet {hot}; nothing was written",
            secret.kind.flag()
        )
    })?;
    let custodians: Vec<String> = observed
        .custodian_pubkeys
        .iter()
        .filter_map(|pubkey| dexdo_core::normalize_multisig_pubkey(pubkey))
        .collect();
    if custodians.is_empty() {
        bail!(
            "Hot wallet {hot} reports no pubkey custodians, so no local key can sign for it; \
             nothing was written"
        );
    }
    if !custodians.contains(&signer) {
        bail!(
            "the key in {} is not a custodian of Hot wallet {hot}; binding a wallet dexdo cannot \
             sign for would fail at the first spend, so nothing was written. Point {} at the \
             wallet's own custodian secret, or bind the address that key does own",
            secret.path.display(),
            secret.kind.flag()
        );
    }

    let (hot_key_file, hot_seed_file) = match secret.kind {
        ManualSecretKind::Key => (Some(secret.path.clone()), None),
        ManualSecretKind::SeedPhrase => (None, secret.path.clone().into()),
    };
    Ok(VerifiedManualWallet {
        binding: WalletBinding {
            version: BINDING_VERSION,
            id: binding_id,
            provider: WalletProvider::Manual,
            network,
            hot_address: hot,
            // Gosh.ai and manual have no Vault, and a field naming one would be a lie a later
            // funding flow could act on.
            vault_address: None,
            hot_key_file,
            vault_key_file: None,
            hot_seed_file,
            push_profile_address: None,
        },
    })
}

fn display_path(path: &Path) -> Result<String> {
    path.to_str().map(str::to_string).ok_or_else(|| {
        anyhow::anyhow!(
            "secret file path {} is not printable text, so it cannot be recorded in the binding",
            path.display()
        )
    })
}

// The binding id is NOT minted here. `wallet::WalletStore::open_draft` reserves one before any
// onboarding runs and creates the secrets directory named after it, and that is the id this flow
// must record. A second one minted at this point produced a `binding.json` naming a directory that
// did not exist while the reserved directory was referenced by nothing -- see `wallet::manual_flow`.

/// Currency and amount one operation needs the bound Hot to hold before it may spend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HotFundingRequirement {
    pub(crate) currency_id: u32,
    pub(crate) required_raw: u128,
}

/// How a manual funding wait ended, with the balance the last read actually saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ManualFundingOutcome {
    /// The requirement was already met, or was met while waiting.
    Funded { available_raw: u128 },
    /// The wait ran out. Nothing was submitted and nothing was written.
    TimedOut { available_raw: u128 },
}

/// The one chain fact the manual funding flow needs. Read-only on purpose: the manual provider has
/// no request to create, so it is given no way to write.
#[async_trait::async_trait(?Send)]
pub(crate) trait HotEccReader {
    async fn read_hot_ecc_raw(&self, currency_id: u32) -> Result<u128>;
}

/// An amount as the operator will have to send it. The currency is named by the sentence around
/// it; this is the figure. For ECC[2], whose decimals this client knows, that is SHELL with the raw
/// beside it -- the raw is what an explorer shows. For any other ECC currency the decimals are not
/// ours to assume, so the raw figure stands alone and says so.
fn ecc_amount(currency_id: u32, raw: u128) -> String {
    if currency_id == dexdo_core::params::SHELL_CURRENCY_ID {
        format!("{} ({raw} raw)", dexdo_core::shell_amount(raw))
    } else {
        format!("{raw} raw")
    }
}

fn ecc_label(currency_id: u32) -> String {
    if currency_id == dexdo_core::params::SHELL_CURRENCY_ID {
        "SHELL ECC[2]".to_string()
    } else {
        format!("ECC[{currency_id}]")
    }
}

/// What the operator is shown when the bound Hot is short.

/// It carries the exact figures and the full canonical address, and nothing else: the manual
/// provider creates no Vault request and points at no external service, so naming one here would
/// promise an action that does not happen.
pub(crate) fn render_manual_funding_shortfall(
    hot_address: &str,
    operation: &str,
    requirement: HotFundingRequirement,
    available_raw: u128,
    timeout: Duration,
) -> String {
    let missing_raw = requirement.required_raw.saturating_sub(available_raw);
    let currency = ecc_label(requirement.currency_id);
    let missing = ecc_amount(requirement.currency_id, missing_raw);
    format!(
        "{operation} needs {} {currency} in Hot wallet {hot_address}, which holds \
         {}: missing {missing}.\nSend {missing} {currency} to \
         {hot_address} yourself. This wallet is bound as provider `manual`, so dexdo creates no \
         funding request and opens no external service on your behalf; the only thing it watches \
         is the on-chain balance of that address.\nWaiting up to {} seconds for the top-up. \
         Nothing has been submitted.",
        ecc_amount(requirement.currency_id, requirement.required_raw),
        ecc_amount(requirement.currency_id, available_raw),
        timeout.as_secs(),
    )
}

/// What the operator is shown when the wait ran out.

/// It has to say two things without either being inferable from the other: nothing was left behind,
/// and the fix is the same command again.
pub(crate) fn render_manual_funding_timeout(
    hot_address: &str,
    operation: &str,
    requirement: HotFundingRequirement,
    available_raw: u128,
    timeout: Duration,
) -> String {
    let missing_raw = requirement.required_raw.saturating_sub(available_raw);
    let currency = ecc_label(requirement.currency_id);
    format!(
        "no top-up of Hot wallet {hot_address} arrived within {} seconds; it holds {} \
         {currency} and {operation} needs {}, so {} is still missing. \
         Nothing was submitted and no local state was written, so run the same command again once \
         the transfer lands -- it reads the balance from the chain again from scratch",
        timeout.as_secs(),
        ecc_amount(requirement.currency_id, available_raw),
        ecc_amount(requirement.currency_id, requirement.required_raw),
        ecc_amount(requirement.currency_id, missing_raw),
    )
}

/// Wait for the bound Hot to hold what the operation needs.

/// Reads first and reads last: the balance decides, never a timer and never a message from a
/// wallet application. Writes nothing at any point, which is what makes the timeout safe -- a
/// second run starts from exactly the state the first one did and re-reads the chain.
pub(crate) async fn wait_for_manual_hot_funding(
    reader: &dyn HotEccReader,
    requirement: HotFundingRequirement,
    timeout: Duration,
    poll: Duration,
) -> Result<ManualFundingOutcome> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let available_raw = reader.read_hot_ecc_raw(requirement.currency_id).await?;
        if available_raw >= requirement.required_raw {
            return Ok(ManualFundingOutcome::Funded { available_raw });
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(ManualFundingOutcome::TimedOut { available_raw });
        }
        tokio::time::sleep(poll.min(deadline.duration_since(now))).await;
    }
}

/// The manual provider's half of the shared "make sure Hot can pay for this" step.

/// Success means the requirement was observed met on chain, not that a wait completed. The human
/// text goes to stderr so a machine-readable stdout stays clean.
pub(crate) async fn ensure_manual_hot_funded(
    reader: &dyn HotEccReader,
    hot_address: &str,
    operation: &str,
    requirement: HotFundingRequirement,
    timeout: Duration,
    poll: Duration,
) -> Result<u128> {
    let available_raw = reader.read_hot_ecc_raw(requirement.currency_id).await?;
    if available_raw >= requirement.required_raw {
        return Ok(available_raw);
    }
    eprintln!(
        "{}",
        render_manual_funding_shortfall(
            hot_address,
            operation,
            requirement,
            available_raw,
            timeout
        )
    );
    match wait_for_manual_hot_funding(reader, requirement, timeout, poll).await? {
        ManualFundingOutcome::Funded { available_raw } => Ok(available_raw),
        ManualFundingOutcome::TimedOut { available_raw } => bail!(
            "{}",
            render_manual_funding_timeout(
                hot_address,
                operation,
                requirement,
                available_raw,
                timeout
            )
        ),
    }
}

/// The manual provider's entry point for an operation that is about to spend the bound Hot.

/// This is the shape the shared provider-aware funding step calls: the timeout is the specification's
/// one common bound rather than a manual-only timer, and the read cadence is the one this tree
/// already uses to watch SHELL arrive at an address. An operator override (`--funding-timeout`)
/// belongs to the spending command, which passes it to [`ensure_manual_hot_funded`] directly.
pub(crate) async fn ensure_manual_hot_funded_with_defaults(
    reader: &dyn HotEccReader,
    hot_address: &str,
    operation: &str,
    requirement: HotFundingRequirement,
) -> Result<u128> {
    ensure_manual_hot_funded(
        reader,
        hot_address,
        operation,
        requirement,
        dexdo_core::params::WALLET_HOT_FUNDING_TIMEOUT,
        dexdo_core::params::NOTE_DEPLOY_SHELL_FUNDING_POLL_INTERVAL,
    )
    .await
}

/// Tell the two accepted secret files apart by what they contain.

/// Only the interactive form needs this: the automation forms say which one they mean with the
/// flag they use. It is deliberately strict rather than best-effort, because feeding a seed phrase
/// to the raw-secret reader yields a valid-looking key for a wallet nobody owns.
pub(crate) fn classify_manual_secret_file(contents: &str) -> Result<ManualSecretKind> {
    let words: Vec<&str> = contents.split_whitespace().collect();
    match words.as_slice() {
        [single] if single.len() == 64 && single.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            Ok(ManualSecretKind::Key)
        }
        _ if matches!(words.len(), 12 | 15 | 18 | 21 | 24)
            && words
                .iter()
                .all(|word| word.bytes().all(|byte| byte.is_ascii_alphabetic())) =>
        {
            Ok(ManualSecretKind::SeedPhrase)
        }
        _ => bail!(
            "cannot tell whether that file holds a 32-byte secret hex or a seed phrase; pass \
             --multisig-private-key or --multisig-seed-file to say which it is"
        ),
    }
}

/// Ask for one non-secret line on a terminal. Refuses to read when stdin is not a terminal, so an
/// automated run gets the flag list instead of blocking on a pipe.
fn prompt_line(prompt: &str, missing: &str) -> Result<String> {
    use std::io::{BufRead as _, IsTerminal as _, Write as _};
    if !std::io::stdin().is_terminal() {
        bail!(
            "`dexdo wallet onboard manual` needs {missing} when stdin is not a terminal: pass \
             --multisig-private-key <path> or --multisig-seed-file <path>. The address is not asked for -- \
             it follows from the key."
        );
    }
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        bail!("{missing} was not supplied; nothing was written");
    }
    let line = line.trim().to_string();
    if line.is_empty() {
        bail!("{missing} was not supplied; nothing was written");
    }
    Ok(line)
}

/// Resolve the address either from the flag or, on a terminal, by asking.
fn resolve_manual_address(flag: Option<&str>) -> Result<String> {
    match flag {
        Some(address) => Ok(address.trim().to_string()),
        None => prompt_line(
            "Hot wallet address (<dapp_id>::<account_id>): ",
            "an address",
        ),
    }
}

mod persist {
    use super::{active_binding_path, VerifiedManualWallet, WalletBinding, WALLET_DIR};
    use anyhow::Result;
    use slot::EmptyBindingSlot;
    use std::path::{Path, PathBuf};

    /// The already-bound refusal, and the only place the proof that it ran can come from.

    /// [`EmptyBindingSlot`]'s field is private and this module has no child modules, so nothing
    /// outside these few lines can build one -- not `persist`, not `wallet_manual`, not a test.
    /// The writer takes one by value, which turns "every write is preceded by the already-bound
    /// refusal" into an argument the compiler will not let a caller skip. It is the same shape
    /// [`VerifiedManualWallet`] already uses for the verification half, applied to the half that
    /// decides whether the slot may be written at all.

    /// This replaces counting the writer's name in the source as the guarantee. That count read
    /// `save_active_binding` followed immediately by `(`, so a second call site written with one
    /// space before the paren contributed nothing to it and the count stayed at 2 while three
    /// places wrote the binding. A guard a space walks past is worse than none, because it reads as
    /// a guarantee in review.
    mod slot {
        use super::{already_bound, WalletBinding};
        use anyhow::Result;
        use std::path::Path;

        /// A data dir whose active-binding slot was observed empty.

        /// It carries the path rather than sitting beside it, so a caller cannot prove one
        /// directory empty and then write into another.
        pub(super) struct EmptyBindingSlot<'a>(&'a Path);

        impl<'a> EmptyBindingSlot<'a> {
            /// The directory the emptiness was observed in, and the only one it licenses.
            pub(super) fn data_dir(&self) -> &'a Path {
                self.0
            }
        }

        /// Refuse an operator who is already bound, or yield the proof that the slot is free.
        pub(super) fn refuse_if_bound<'a>(
            data_dir: &'a Path,
            existing: Option<&WalletBinding>,
        ) -> Result<EmptyBindingSlot<'a>> {
            match existing {
                Some(existing) => Err(already_bound(existing)),
                None => Ok(EmptyBindingSlot(data_dir)),
            }
        }
    }

    /// Write the one active binding, atomically and owner-only.

    /// Both arguments are proofs rather than data: a [`VerifiedManualWallet`] says the wallet
    /// passed every check, an [`EmptyBindingSlot`] says the refusal ran and found nothing to
    /// protect. Neither can be constructed by a would-be second writer, so the single call site
    /// below is a fact the compiler keeps -- widening this function's visibility would not hand
    /// another module a way to call it, because it would still have no way to build the arguments.
    fn save_active_binding(
        data_dir: EmptyBindingSlot<'_>,
        verified: &VerifiedManualWallet,
    ) -> Result<PathBuf> {
        let data_dir = data_dir.data_dir();
        let dir = data_dir.join(WALLET_DIR);
        create_owner_only_dir(&dir)?;
        let path = active_binding_path(data_dir);
        let mut bytes = serde_json::to_vec_pretty(verified.binding())
            .map_err(|error| anyhow::anyhow!("render wallet binding: {error}"))?;
        bytes.push(b'\n');
        crate::cli::note::write_private_atomic(&path, &bytes)?;
        Ok(path)
    }

    fn create_owner_only_dir(dir: &Path) -> Result<()> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder
            .create(dir)
            .or_else(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|error| anyhow::anyhow!("create wallet directory {}: {error}", dir.display()))
    }

    /// The one refusal an already-bound operator gets, worded once.

    /// It is raised twice on purpose: at the top of onboarding, so no chain read or prompt is spent
    /// on a command that was never going to write, and again here, so the check that guards the
    /// write is the write's own.
    pub(crate) fn already_bound(existing: &WalletBinding) -> anyhow::Error {
        // The wallet specification points the operator at a rebind command here. It is another
        // agent's half of and does not exist yet, and this tree rejects a printed command line
        // its own parser cannot run -- rightly, since a name that does not resolve is a dead end.
        // So the replacement is described rather than named until that command ships.
        anyhow::anyhow!(
            "a wallet is already bound: provider `{}`, Hot {}. Onboarding never replaces an active \
             binding, because the old Hot may still hold funds; replacing it is a separate, \
             explicit rebind operation. Nothing was written",
            existing.provider.as_str(),
            existing.hot_address
        )
    }

    /// Refuse to replace an existing binding, then save the verified one.

    /// The refusal is not a step this function is trusted to remember: it is where the writer's
    /// argument comes from, so a version of this that forgot it would not compile.
    pub(crate) fn onboard_manual_binding(
        data_dir: &Path,
        existing: Option<&WalletBinding>,
        verified: &VerifiedManualWallet,
    ) -> Result<PathBuf> {
        let data_dir = slot::refuse_if_bound(data_dir, existing)?;
        save_active_binding(data_dir, verified)
    }
}

mod live {
    use super::{
        classify_manual_secret_file, manual_onboard_step, prompt_line,
        render_manual_deploy_funding_refusal, render_manual_deploy_funding_request,
        verify_manual_hot_wallet, ManualOnboardStep, ManualSecretKind, ManualSecretRef,
        ObservedHotWallet, WalletBinding, WalletOnboardManualArgs,
    };
    use anyhow::{bail, Result};
    use dexdo_core::chain::RetryingReads as _;
    use std::path::PathBuf;

    /// Read every fact the decision needs from one Active multisig.
    async fn observe_hot_wallet(
        client: &dexdo_core::ChainClient,
        address: &dexdo_core::Address,
    ) -> Result<ObservedHotWallet> {
        let rendered = address.with_workchain();
        let account = client
            .get_account_retrying(address)
            .await
            .map_err(|error| anyhow::anyhow!("read Hot wallet {rendered}: {error}"))?;
        let Some(account) = account else {
            return Ok(ObservedHotWallet {
                status: "NotFound".to_string(),
                code_hash: None,
                required_txn_confirms: 0,
                custodian_pubkeys: Vec::new(),
            });
        };
        if !account.is_active() {
            return Ok(ObservedHotWallet {
                status: account.status,
                code_hash: account.code_hash,
                required_txn_confirms: 0,
                custodian_pubkeys: Vec::new(),
            });
        }
        let custodians = client
            .run_getter_retrying(
                address,
                dexdo_core::canonical_multisig::MULTISIG_ABI_JSON,
                "getCustodians",
                serde_json::json!({}),
            )
            .await
            .map_err(|error| {
                anyhow::anyhow!("read custodians of Hot wallet {rendered}: {error}")
            })?;
        let parameters = client
            .run_getter_retrying(
                address,
                dexdo_core::canonical_multisig::MULTISIG_ABI_JSON,
                "getParameters",
                serde_json::json!({}),
            )
            .await
            .map_err(|error| {
                anyhow::anyhow!("read transaction threshold of Hot wallet {rendered}: {error}")
            })?;
        let required_txn_confirms = parameters
            .as_ref()
            .and_then(|output| output.get("requiredTxnConfirms"))
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
            })
            .and_then(|value| u8::try_from(value).ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Hot wallet {rendered} is Active, but getParameters returned no usable \
                     requiredTxnConfirms (ABI/getter output mismatch); nothing was written"
                )
            })?;
        let custodian_pubkeys = custodians
            .as_ref()
            .and_then(|output| output.get("custodians"))
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Hot wallet {rendered} is Active, but getCustodians returned no `custodians` \
                     array (ABI/getter output mismatch); nothing was written"
                )
            })?
            .iter()
            .filter_map(|custodian| custodian.get("owner_pubkey"))
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect();
        Ok(ObservedHotWallet {
            status: account.status,
            code_hash: account.code_hash,
            required_txn_confirms,
            custodian_pubkeys,
        })
    }

    /// Pick the secret file from the flags, or ask for one and read what kind it is.
    fn resolve_manual_secret(args: &WalletOnboardManualArgs) -> Result<ManualSecretRef> {
        let path = match (&args.multisig_private_key, &args.multisig_seed_file) {
            (Some(_), Some(_)) => {
                bail!("use only one of --multisig-private-key or --multisig-seed-file")
            }
            (Some(path), None) => {
                return Ok(ManualSecretRef {
                    kind: ManualSecretKind::Key,
                    path: path.clone(),
                })
            }
            (None, Some(path)) => {
                return Ok(ManualSecretRef {
                    kind: ManualSecretKind::SeedPhrase,
                    path: path.clone(),
                })
            }
            (None, None) => PathBuf::from(prompt_line(
                "Path to the wallet secret file (32-byte secret hex, or a seed phrase): ",
                "a secret file path",
            )?),
        };
        // THE CHECK COMES BEFORE THE READ, and here that is the whole fix.

        // This is not a missing call, it was a wrong ORDER. The guarded read at
        // `multisig_secret_hex` further down did run -- but this classification had already pulled
        // the operator's secret into memory to decide whether it was a hex key or a seed phrase.
        // `support.rs` names that failure in its own words: "a refusal that has already loaded the
        // secret has done the thing it refuses." A guard that fires after the read is a report, not
        // a protection.
        crate::cli::support::refuse_exposed_secret_file(&path, "the wallet secret file")?;
        let contents = std::fs::read_to_string(&path)
            .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
        let kind = classify_manual_secret_file(&contents)?;
        Ok(ManualSecretRef { kind, path })
    }

    /// Render the address the way the binding will store it: the DApp id comes from the chain, not
    /// from an assumption, because a wallet does not have to live in the dexdo DApp.
    async fn canonical_hot_address(
        chain: &dexdo_core::ChainClient,
        address: &dexdo_core::Address,
        supplied: &str,
    ) -> Result<String> {
        if supplied.contains("::") {
            return dexdo_core::CanonicalAddress::parse(supplied)
                .map(|address| address.to_string())
                .map_err(|error| anyhow::anyhow!("--multisig-address: {error}"));
        }
        crate::cli::note_cmd::operator_wallet_canonical_address(chain, address)
            .await
            .map(|address| address.to_string())
    }

    /// Where the manifest is. One answer, and it does not depend on the flags of this command.

    /// This replaced a chooser that picked between two manifests by looking at `--network` and at
    /// whether `--contracts` had been answered. Both inputs are gone: there is one
    /// manifest, `DEXDO_MANIFEST` says where, and the network is the `network` field inside it.
    pub(super) fn manifest_for_network() -> Result<String> {
        Ok(crate::cli::commands::manifest_path()?.display().to_string())
    }

    /// Deploy the wallet if the chain does not have it yet, waiting for its gas if it is short.

    /// Reads first, and the read decides -- see [`manual_onboard_step`]. The three outcomes are
    /// distinct on purpose: an `Active` wallet is never redeployed, a funded one is deployed
    /// without asking, and an unfunded one produces a request and a wait rather than an error the
    /// operator has to translate into an action.

    /// Nothing here writes to disk. A wait that ends without the money leaves the chain and the
    /// data directory exactly as they were, so the same command can simply be run again.
    /// What this command does, as `(what is happening, what happened)`.

    /// The first step is the operator's, not ours: nothing moves until a transfer lands, and
    /// `progress::step_needs_you` draws it amber for exactly that. Naming it as the instruction
    /// ("send...") rather than an observation ("waiting for funds") is the same choice
    /// `ONBOARD_STEPS` makes -- said the other way round it reads as the client watching something
    /// already in motion, and an operator with the phone in their hand waits for a transfer nobody
    /// asked them to make.
    pub(crate) const MANUAL_DEPLOY_STEPS: [(&str, &str); 3] = [
        ("send the SHELL above to the address above", "wallet funded"),
        ("deploying the wallet", "wallet deployed"),
        ("confirming where the deploy landed", "deploy confirmed"),
    ];

    async fn ensure_manual_wallet_deployed(
        chain: &dexdo_core::ChainClient,
        address: &dexdo_core::Address,
        keys: &dexdo_core::KeyPair,
    ) -> Result<()> {
        let rendered = dexdo_core::address::display_self_dapp(&address.with_workchain());
        let read = || async {
            let account = chain
                .get_account_retrying(address)
                .await
                .map_err(|error| anyhow::anyhow!("read wallet {rendered}: {error}"))?;
            Ok::<_, anyhow::Error>(match account {
                Some(account) => (account.status.clone(), account.balance),
                None => ("NotFound".to_string(), 0),
            })
        };

        let (status, native_raw) = read().await?;
        let step = match manual_onboard_step(&status, native_raw) {
            // Nothing more to do here: `verify_manual_hot_wallet`, which every path reaches
            // before a binding is written, refuses a wallet whose `requiredTxnConfirms` is not 1
            // or whose custodian list does not hold this key.
            ManualOnboardStep::BindExisting => return Ok(()),
            step => step,
        };

        // Held for the rest of the command, so the deploy that follows the wait draws under the
        // same checklist rather than starting a second one. `None` until there is something worth
        // showing: a wallet that only needs deploying is seconds of work, and the funding request
        // must be on the screen before a spinner is allowed to move.
        let mut display: Option<crate::cli::progress::Status> = None;

        // The network the manifest declared, named to the operator and stated in the code.
        // One source, read once, and read BEFORE the fork: the refusal, the request and the QR all
        // say the same string, so they cannot disagree about which chain is being asked for. A
        // headless run gets it too -- that is the path with no QR at all, where the sentence is the
        // only thing between the operator and the wrong chain.
        let network = dexdo_core::params::current_network();

        if let ManualOnboardStep::AwaitFunding { available_raw } = step {
            if let super::ManualFundingWait::NobodyToAsk =
                super::manual_funding_wait(crate::cli::interaction::may_ask())
            {
                bail!(
                    "{}",
                    render_manual_deploy_funding_refusal(&rendered, available_raw, network)
                );
            }
            eprintln!(
                "{}",
                render_manual_deploy_funding_request(&rendered, available_raw, network)
            );
            super::write_manual_deploy_funding_qr(&mut std::io::stderr(), &rendered, network);
            // The display starts AFTER the request and the code are drawn, for the reason
            // `wallet_onboarding` gives at the same point: a spinner running while a QR is printed
            // rewrites a line of it, and a QR missing a line does not scan. From here on nothing
            // else prints until the wait is over.
            display = Some(crate::cli::progress::Status::with_plan(
                MANUAL_DEPLOY_STEPS[0].0,
                MANUAL_DEPLOY_STEPS.iter().copied(),
            ));
            // Amber, and named as what the operator must do: this is not the client working, it is
            // the client stopped until a transfer is confirmed in a phone.
            crate::cli::progress::step_needs_you(MANUAL_DEPLOY_STEPS[0].0);
            let deadline =
                tokio::time::Instant::now() + dexdo_core::params::WALLET_HOT_FUNDING_TIMEOUT;
            loop {
                tokio::time::sleep(dexdo_core::params::NOTE_DEPLOY_SHELL_FUNDING_POLL_INTERVAL)
                    .await;
                let (status, native_raw) = read().await?;
                match manual_onboard_step(&status, native_raw) {
                    ManualOnboardStep::BindExisting => return Ok(()),
                    ManualOnboardStep::Deploy => break,
                    ManualOnboardStep::AwaitFunding { available_raw } => {
                        if tokio::time::Instant::now() >= deadline {
                            bail!(
                                "{}",
                                render_manual_deploy_funding_refusal(
                                    &rendered,
                                    available_raw,
                                    network,
                                )
                            );
                        }
                    }
                }
            }
        }

        // The deploy must land on the chain the reads watched, and after it cannot land
        // anywhere else: the manifest `DEXDO_MANIFEST` names supplies the endpoint that was polled
        // AND the label the binding is keyed by, so there is no second answer to disagree with.

        // What this comment used to describe was real while there were two answers: `--contracts`
        // defaulted to one network's manifest whatever `--network` said, and a missing default fell
        // back to an EMBEDDED manifest, so a run could poll one chain for the operator's money and
        // submit the state-init to another. The post-deploy address guard could not catch that --
        // the canonical address is a hash of code and key and is identical on both chains. All
        // three mechanisms are gone; the explicit comparison that used to stand here went with them,
        // and the note beside the deleted test says why.
        // A wallet that was already funded reaches here without a display: the wait is what earns
        // one, but the deploy is still a chain round-trip an operator should not watch in silence.
        let display = display.get_or_insert_with(|| {
            crate::cli::progress::Status::with_plan(
                MANUAL_DEPLOY_STEPS[1].0,
                MANUAL_DEPLOY_STEPS.iter().copied(),
            )
        });
        display.step(MANUAL_DEPLOY_STEPS[1].0);

        let manifest = manifest_for_network()?;
        // The manifest names where to dial; `--endpoint` is gone.
        let backend = dexdo_core::RealChainBackend::connect_with_endpoint(&manifest, None)
            .map_err(|error| anyhow::anyhow!("connect to deploy the wallet: {error}"))?;
        let deployed = backend
            .deploy_multisig_self_funded(keys)
            .await
            .map_err(|error| anyhow::anyhow!("deploy wallet {rendered}: {error}"))?;
        // Named for what this function actually does last: confirm the deploy landed where the key
        // said it would. The wallet's CHECKS -- code hash, confirmation count, custodian list --
        // run in `run()` after this returns, so claiming them here would tick a step before the
        // work, and print "wallet checked" above the error saying it was not.
        display.step(MANUAL_DEPLOY_STEPS[2].0);
        if deployed.with_workchain() != address.with_workchain() {
            bail!(
                "the deploy landed at {} instead of {rendered}; nothing further was written",
                dexdo_core::address::display_self_dapp(&deployed.with_workchain()),
            );
        }
        // Only here: every step the plan declared is genuinely behind. Dropping the display does
        // NOT do this, deliberately -- a checklist that ticked itself on the way out of a failure
        // would claim work the error printed under it says never happened.
        // Every step this display declared is behind. The verification that follows belongs to
        // `run()` and has no display of its own; taking the line down here keeps its error, if it
        // comes, from appearing under a spinner.
        display.finish();
        crate::cli::progress::complete();
        println!("wallet deployed at {rendered}");
        Ok(())
    }

    /// Verify the operator's Hot and return the binding it proved. It does NOT write.

    /// `run_selected` already refuses an existing binding before this is reached, and commits what
    /// this returns through `WalletStore`, which archives whatever it replaces. This module's own
    /// `persist` half keeps its tests and its single-call-site guarantee; production simply no
    /// longer has a second writer of `<data-dir>/wallet/binding.json`, because two writers is how
    /// the only local record of a Hot that still holds funds gets renamed away.

    /// `binding_id` is the id `WalletStore::open_draft` reserved for this attempt, and it is
    /// recorded verbatim. It is a parameter rather than something minted here so that the id in
    /// `binding.json` is the same one the reserved secrets directory is named after.
    pub(crate) async fn run(
        args: WalletOnboardManualArgs,
        binding_id: &str,
    ) -> Result<WalletBinding> {
        let secret = resolve_manual_secret(&args)?;
        let (key_file, seed_file) = match secret.kind {
            ManualSecretKind::Key => (Some(secret.path.clone()), None),
            ManualSecretKind::SeedPhrase => (None, Some(secret.path.clone())),
        };
        let (_, secret_hex) = crate::cli::commands::multisig_secret_hex(&key_file, &seed_file)?;
        let keys = dexdo_core::KeyPair::from_secret_hex(secret_hex.trim())
            .map_err(|error| anyhow::anyhow!("{}: {error:?}", secret.kind.flag()))?;

        // The address is a CONSEQUENCE of the key, not a second input: the canonical wallet's
        // address is the hash of its code and the owner public key, so deriving it here removes the
        // one input an operator could get wrong while holding the right key. `--multisig-address`
        // survives only as a cross-check, for the operator who wants to be told they typed the
        // wallet they meant.
        let derived = dexdo_core::RealChainBackend::multisig_address(&keys)
            .await
            .map_err(|error| anyhow::anyhow!("derive the wallet address from its key: {error}"))?;
        let supplied_address = derived.with_workchain();
        if let Some(claimed) = args.multisig_address.as_deref() {
            let claimed = claimed.trim();
            // An address that will not PARSE and an address that parses but names a different
            // wallet are different accidents with different fixes, and collapsing them sends the
            // operator to the wrong one. Measured: a 130-character address broke across a line
            // when it was pasted, the flag received `<64 hex>::<26 hex>`, and the client answered
            // "is not the wallet this key controls" -- which reads as a wrong KEY. It cost an hour
            // of looking at key derivation and the `.tvc`, while the parser had known all along
            // that the string was 26 hex where 64 belong. `.unwrap_or(false)` was what threw that
            // away.
            match dexdo_core::address::parse_chain_address(claimed) {
                Err(error) => bail!(
                    "--multisig-address is not a usable address: {error}. Nothing was written, and \
                     this is about the text you passed, not about your key -- a long address \
                     pasted into a terminal is often broken across a line, which leaves a half of \
                     the wrong length. The wallet this key controls is {}, so pass exactly that or \
                     drop the flag.",
                    dexdo_core::address::display_self_dapp(&supplied_address),
                ),
                Ok(parsed) if parsed.with_workchain() != supplied_address => bail!(
                    "--multisig-address {} is not the wallet this key controls ({}); nothing was \
                     written. The address follows from the key, so pass the right key or drop the \
                     flag.",
                    dexdo_core::address::display_self_dapp(claimed),
                    dexdo_core::address::display_self_dapp(&supplied_address),
                ),
                Ok(_) => {}
            }
        }

        // Verification is read-only account queries against the selected network, and the
        // deployed-contracts manifest names none of the accounts they read. So the endpoint is the
        // whole input, the same one `wallet onboard ackinacki-wallet` connects with.
        let endpoint = crate::cli::wallet::wallet_read_endpoint(
            Some(&crate::cli::commands::manifest_path()?),
            crate::cli::wallet::network_from_manifest()?,
        )?;
        let chain = dexdo_core::ChainClient::connect(&endpoint).map_err(|error| {
            anyhow::anyhow!("connect verification endpoint {endpoint}: {error}")
        })?;
        let address = dexdo_core::address::parse_chain_address(&supplied_address)?;

        // Deploy the wallet if it is not there yet. Everything below this point is the path that
        // already existed: whatever happens here, the binding is written only for a wallet the
        // chain reports as Active with a code hash this client accepts.
        ensure_manual_wallet_deployed(&chain, &address, &keys).await?;

        let observed = observe_hot_wallet(&chain, &address).await?;
        let hot_address = canonical_hot_address(&chain, &address, &supplied_address).await?;

        let secret_path = crate::cli::note::resolve_private_file_path(&secret.path, "secret file")?;
        let verified = verify_manual_hot_wallet(
            &hot_address,
            crate::cli::wallet::network_from_manifest()?,
            ManualSecretRef {
                kind: secret.kind,
                path: secret_path,
            },
            keys.public_hex(),
            &observed,
            binding_id.to_string(),
        )?;
        println!(
            "secret file (referenced, never copied): {}",
            verified
                .binding()
                .hot_key_file
                .as_deref()
                .or(verified.binding().hot_seed_file.as_deref())
                .unwrap_or(std::path::Path::new("-"))
                .display()
        );
        Ok(verified.binding().clone())
    }
}

/// The ECC reader the manual funding wait uses against a real Hot.
pub(crate) struct ChainHotEccReader<'a> {
    client: &'a dexdo_core::ChainClient,
    address: dexdo_core::Address,
}

impl<'a> ChainHotEccReader<'a> {
    pub(crate) fn new(client: &'a dexdo_core::ChainClient, address: dexdo_core::Address) -> Self {
        Self { client, address }
    }
}

#[async_trait::async_trait(?Send)]
impl HotEccReader for ChainHotEccReader<'_> {
    async fn read_hot_ecc_raw(&self, currency_id: u32) -> Result<u128> {
        use dexdo_core::chain::RetryingReads as _;

        let rendered = self.address.with_workchain();
        let account = self
            .client
            .get_account_retrying(&self.address)
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "read {} of Hot wallet {rendered}: {error}",
                    ecc_label(currency_id)
                )
            })?
            .ok_or_else(|| anyhow::anyhow!("Hot wallet {rendered} not found"))?;
        Ok(account.ecc_balance(currency_id))
    }
}

pub(crate) async fn run_wallet_onboard_manual(
    args: WalletOnboardManualArgs,
    binding_id: &str,
) -> Result<WalletBinding> {
    live::run(args, binding_id).await
}

#[cfg(test)]
mod deploy_1627_tests {
    use super::*;

    /// A cross-check flag that cannot read its own input says so, instead of blaming the key.

    /// Measured, on mainnet, with the operator's real wallet. The canonical address is 130
    /// characters; pasted into a terminal it broke across a line, so `--multisig-address` received
    /// `<64 hex>::<26 hex>` -- a string no parser accepts. The client answered "is not the wallet
    /// this key controls", which names the KEY as the suspect, and an hour went into the key
    /// derivation and the `.tvc` before anyone looked at the argument. The parser had rejected the
    /// string on sight, with the exact reason; `.unwrap_or(false)` turned its verdict into "did
    /// not match".

    /// So this pins the distinction, not the wording: an unparseable address and a well-formed
    /// address belonging to someone else are different accidents, and the message for one must not
    /// be reachable from the other.
    #[test]
    fn an_unreadable_address_is_reported_as_unreadable_not_as_the_wrong_wallet() {
        let whole = format!(
            "{0}::{0}",
            "ef6ecd30ab17ca3280bdc29decae1e5a1c089606740dbb915bf3a33edddccb75"
        );
        // Exactly what the terminal delivered: the second half cut where the line wrapped.
        let broken = format!(
            "{}::ef6ecd30ab17ca3280bdc29dec",
            "ef6ecd30ab17ca3280bdc29decae1e5a1c089606740dbb915bf3a33edddccb75"
        );

        let verdict = dexdo_core::address::parse_chain_address(&broken);
        assert!(
            verdict.is_err(),
            "the parser must refuse a half that is not 64 hex; if this ever passes, the whole \
             distinction below is moot"
        );
        let reason = verdict.unwrap_err().to_string();
        assert!(
            reason.contains("64"),
            "the parser's reason names the length it wanted, which is the thing the operator has \
             to see: {reason}"
        );

        // And the whole address still parses to the same wallet, so the flag keeps working.
        let parsed = dexdo_core::address::parse_chain_address(&whole)
            .expect("the operator's real mainnet address parses");
        assert_eq!(
            dexdo_core::address::display_self_dapp(&parsed.with_workchain()),
            whole,
            "a canonical address survives the round trip unchanged"
        );
    }

    /// The account decides what happens next, not a subcommand the operator picks.

    /// started as two commands -- `new` to deploy, `import` to bind. They differ only in the
    /// state of an account the client reads anyway, and an operator who picks the wrong one either
    /// redeploys over their wallet or is told to deploy something that already exists. One command,
    /// four states, no choice to get wrong.
    #[test]
    fn the_account_state_decides_the_step() {
        let enough = dexdo_core::params::OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE;

        assert_eq!(
            manual_onboard_step("Active", 0),
            ManualOnboardStep::BindExisting,
            "a deployed wallet is bound, never deployed a second time -- even at zero balance"
        );
        assert_eq!(
            manual_onboard_step("active", enough),
            ManualOnboardStep::BindExisting,
            "the chain's spelling of acc_type is not ours to depend on"
        );
        assert_eq!(
            manual_onboard_step("Uninit", enough),
            ManualOnboardStep::Deploy,
            "funded and undeployed is the one case that writes to the chain"
        );
        assert_eq!(
            manual_onboard_step("Uninit", enough - 1),
            ManualOnboardStep::AwaitFunding {
                available_raw: enough - 1
            },
            "one raw unit short is short"
        );
        assert_eq!(
            manual_onboard_step("NotFound", 0),
            ManualOnboardStep::AwaitFunding { available_raw: 0 },
            "an address nobody has ever sent anything to is the ordinary first run"
        );
    }

    // The guard this test drove is gone, and the property it protected is now structural.

    // It compared the manifest's `network` against a `declared` label -- and after both
    // operands came from the same file: `declared` is `network_from_manifest()`, which loads the
    // manifest `manifest_for_network()` names and returns its `network` field. The file against
    // itself. The one input that could ever have made them differ was trimming, and in that case
    // the refusal named `--network`, a flag this branch removed -- so the only way to reach it was
    // also the only way to be told to type something that no longer parses.

    // What it was written for was a real defect: `--network` picked one chain's manifest
    // while `--endpoint` dialled another's host, and the post-deploy address guard could not catch
    // it because the canonical address is a hash of code and key, identical on both chains. Both
    // levers are gone. There is one manifest, its `network` keys the binding, and its `endpoint`
    // is what gets dialled -- nothing is left that could disagree, so there is nothing to compare.

    // Deleted rather than left unwired: a check nobody calls reads as protection, which is worse
    // than no check, and this one sat on a path that spends.

    // The manifest is not chosen by `--network` any more, so there is nothing here to assert.

    // What stood here checked that `--network mainnet` picked the mainnet manifest and that the
    // path it picked EXISTED in the tree -- a real defect it caught once, when a caller kept the
    // old directory after moved the files. The chooser is gone: one manifest, named by
    // `DEXDO_MANIFEST`, and there is no `--network` left to check it against. The check that used
    // to carry this property was deleted with the guard it drove -- the note above says why, and
    // naming a test that no longer exists is how a reader goes looking for coverage that is gone.

    /// A run with nobody to ask REFUSES; it does not wait.

    /// Asserted on the decision itself, and that is the point of the decision existing. A reviewer
    /// mutated the guard this replaced -- `if !may_ask()` to `if false` -- and not one of 1307
    /// tests fell over: the function holding it is reachable only from the product, and the
    /// refusal's own test called the renderer directly. So the WORDS were proven and the refusal
    /// happening was not, while the comment beside it promised the command would never hang on a
    /// prompt nobody can answer.

    /// Hanging here is not an inconvenience. The address is uninit, the operator's key is on disk,
    /// and the command holds the money path open for as long as it waits.
    #[test]
    fn a_run_with_nobody_to_ask_refuses_instead_of_waiting() {
        assert_eq!(
            manual_funding_wait(false),
            ManualFundingWait::NobodyToAsk,
            "with no one to answer, the command must refuse now -- waiting is a hang that holds \
             the money path open"
        );
        assert_eq!(
            manual_funding_wait(true),
            ManualFundingWait::Wait,
            "where someone CAN send, refusing would break the ordinary path this command exists \
             for"
        );
    }

    /// What the operator is asked for, while nothing has been written anywhere.
    #[test]
    fn the_funding_request_carries_the_whole_address_and_the_amount() {
        let address = format!("{0}::{0}", "ef6ecd30".repeat(8));
        let shown = render_manual_deploy_funding_request(&address, 0, "shellnet");

        assert!(
            shown.contains(&address),
            "the address is copied into a wallet app, so it is printed whole: {shown}"
        );
        assert!(
            shown.contains(&format!(
                "{} SHELL",
                MANUAL_DEPLOY_REQUEST_RAW / 1_000_000_000
            )),
            "the amount asked for is stated: {shown}"
        );
        // Both ends of the transfer are named, because they are different things and the operator
        // acts on the first: SHELL is what leaves their wallet, native gas is what this command
        // watches arrive. Saying only "native gas" would leave them looking for a token to send
        // that their wallet does not list.
        assert!(
            shown.contains("native gas"),
            "the ask says what the SHELL becomes on arrival: {shown}"
        );
        assert!(
            shown.contains("deploys the wallet itself"),
            "the operator is told what happens after they send it: {shown}"
        );
        // The ask is the one line the operator has to act on, so it carries the call-to-act
        // drawing. Compared against the shared helper rather than an escape literal: the shade is
        // that helper's business, and a test that pinned it would break when the palette moves.
        let painted = crate::cli::choose::action("x");
        if painted != "x" {
            let opener = painted.split('x').next().unwrap_or_default().to_string();
            assert!(
                shown.contains(&opener),
                "the request must be drawn as a call to act: {shown}"
            );
        }
    }

    /// The address is offered to a phone camera, not only to a copy buffer.

    /// The wallet that has to send the gas is on a phone, and the address is 130 characters. This
    /// pins that the QR is drawn from the SAME address the request prints -- a code that encodes
    /// something else is worse than none, because it is trusted without being read.
    /// The code a top-up prints carries the amount that was actually asked for.

    /// Written after a review found the first version wrong twice over, and both defects were
    /// invisible to the tests beside it because those searched the source for identifiers instead
    /// of driving the code.

    /// 1. The amount came from `native_shortfall`, which is capped by
    /// `FUNDING_WALLET_NATIVE_FLOOR_RAW` (~0.507 vmshell), so `div_ceil(SHELL_UNIT).max(1)` was
    /// 1 for every input. The line above the code said "short 100 SHELL"; the code said 1.
    /// 2. That figure is a NATIVE balance, and the link labels its amount `token=2`, ECC SHELL.
    /// They are different balances: a transfer to an Active account credits ECC[2] and leaves
    /// native gas alone, so the wait would never end however much was sent.

    /// Driving `payment_link` directly is what makes those visible, and it costs one line.
    #[test]
    fn a_top_up_code_asks_for_the_ecc_shell_that_was_short() {
        let address = format!("{0}::{0}", "ef6ecd30".repeat(8));

        // 100 SHELL short, in raw ECC[2] units.
        let hundred = 100 * dexdo_core::params::SHELL_UNIT;
        let link = super::payment_link(
            &address,
            hundred / dexdo_core::params::SHELL_UNIT,
            "shellnet",
            super::PaymentFlag::None,
        );
        assert!(
            link.contains("&amount=100"),
            "a 100 SHELL shortfall must be asked for as 100, not as whatever a native figure \
             rounds to: {link}"
        );
        assert!(
            link.contains(&format!("&token={}", dexdo_core::params::SHELL_CURRENCY_ID)),
            "the link must name the currency the shortfall is in: {link}"
        );

        // A part-SHELL shortfall rounds UP: asking for less leaves the command waiting.
        let one_and_a_bit = dexdo_core::params::SHELL_UNIT + 1;
        let rounded = one_and_a_bit.div_ceil(dexdo_core::params::SHELL_UNIT);
        assert_eq!(rounded, 2, "a shortfall above one whole SHELL asks for two");

        // And the printed code carries exactly that link.
        let mut drawn = Vec::new();
        super::write_payment_qr(&mut drawn, &address, 100, "shellnet");
        let shown = String::from_utf8_lossy(&drawn);
        assert!(
            shown.contains("scan"),
            "the code is printed without saying what it is for: {shown}"
        );
        let expected = crate::cli::qr_compact::smallest_code(link.as_bytes())
            .expect("the link fits a QR code");
        let mut again = Vec::new();
        crate::cli::qr_display::write_qr(&mut again, &expected).expect("draw the same code");
        assert!(
            drawn
                .windows(again.len())
                .any(|window| window == again.as_slice()),
            "the printed code does not encode the link that was built for this amount"
        );
    }

    #[test]
    fn the_address_is_offered_as_a_qr_code_beside_the_text() {
        let address = format!("{0}::{0}", "ef6ecd30".repeat(8));
        let mut drawn = Vec::new();
        super::write_manual_deploy_funding_qr(&mut drawn, &address, "shellnet");
        let shown = String::from_utf8_lossy(&drawn);

        assert!(
            shown.contains("scan"),
            "the operator is told what the picture is for: {shown}"
        );
        assert!(
            !shown.contains("could not be"),
            "the code has to draw for an ordinary address: {shown}"
        );
        // The payload is the wallet's own scan-only payment form, not a bare address: rebuilt here
        // and compared by rendering, since the code in the terminal is pixels by the time it is
        // written.
        let link = super::manual_deploy_payment_link(&address, "shellnet");
        assert!(
            link.starts_with(&address) && link.contains("&amount=2") && link.contains("&token=2"),
            "the scanned form is `<address>&amount=<decimal>&token=<tokenRoot>`: {link}"
        );
        let expected = crate::cli::qr_compact::smallest_code(link.as_bytes())
            .expect("the payment link fits a QR code");
        let mut again = Vec::new();
        crate::cli::qr_display::write_qr(&mut again, &expected).expect("draw the same code");
        assert!(
            drawn
                .windows(again.len())
                .any(|window| window == again.as_slice()),
            "the code drawn must encode the address the request printed"
        );
    }

    /// Addresses that exist on Mainnet, used as the payloads the compatibility check encodes.

    /// Real ones on purpose. The two differ in the way that matters to a parser splitting on `&`
    /// and `::`: the multisig repeats one 64-hex id in both halves, while the Accumulator pairs a
    /// dapp id that is almost entirely zeroes with an unrelated account id.
    const REAL_MAINNET_ADDRESSES: [&str; 2] = [
        // The v2.4 multisig deployed for this work: `dapp_id == account_id`, Active.
        "ef6ecd30ab17ca3280bdc29decae1e5a1c089606740dbb915bf3a33edddccb75::\
         ef6ecd30ab17ca3280bdc29decae1e5a1c089606740dbb915bf3a33edddccb75",
        // The Accumulator, as docs.ackinacki.com prints it for CLI targets.
        "0000000000000000000000000000000000000000000000000000000000000001::\
         3535353535353535353535353535353535353535353535353535353535353535",
    ];

    /// Read a QR back out of the half-block characters it was printed as.

    /// `qr_compact` packs two module rows into one character row -- `HALVES[top | bottom << 1]` --
    /// so each line yields two rows of modules and each column one module.
    fn modules_of_the_printed_code(printed: &str) -> Vec<Vec<bool>> {
        let mut rows = Vec::new();
        for line in printed.lines() {
            // The rendering paints per line when it writes to a terminal; drop the colours before
            // reading the glyphs, so this works on captured and on painted output alike.
            let plain: String = {
                let mut out = String::with_capacity(line.len());
                let mut chars = line.chars();
                while let Some(character) = chars.next() {
                    if character == '\u{1b}' {
                        for escaped in chars.by_ref() {
                            if escaped.is_ascii_alphabetic() {
                                break;
                            }
                        }
                    } else {
                        out.push(character);
                    }
                }
                out
            };
            // Only the symbol's own lines: the request's words are on the same stream.
            if plain.is_empty()
                || !plain.chars().all(|character| {
                    matches!(character, ' ' | '\u{2580}' | '\u{2584}' | '\u{2588}')
                })
            {
                continue;
            }
            rows.push(
                plain
                    .chars()
                    .map(|character| matches!(character, '\u{2580}' | '\u{2588}'))
                    .collect(),
            );
            rows.push(
                plain
                    .chars()
                    .map(|character| matches!(character, '\u{2584}' | '\u{2588}'))
                    .collect(),
            );
        }
        rows
    }

    /// Decode the printed symbol the way a camera would, and return what it carries.

    /// The modules are blown up and given a wide light margin before decoding: the rendering ships
    /// a two-module quiet zone, which a phone held at arm's length can cope with and a detector fed
    /// a tight bitmap cannot.
    fn decode_the_printed_code(printed: &str) -> String {
        let grid = modules_of_the_printed_code(printed);
        assert!(!grid.is_empty(), "no QR was printed to decode:\n{printed}");
        let columns = grid[0].len();
        const SCALE: usize = 4;
        const MARGIN: usize = 8;
        let width = (columns + 2 * MARGIN) * SCALE;
        let height = (grid.len() + 2 * MARGIN) * SCALE;
        let mut image = rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| {
            let (column, row) = (x / SCALE, y / SCALE);
            if column < MARGIN || row < MARGIN {
                return 255;
            }
            match grid
                .get(row - MARGIN)
                .and_then(|line| line.get(column - MARGIN))
            {
                Some(true) => 0,
                _ => 255,
            }
        });
        let grids = image.detect_grids();
        assert_eq!(
            grids.len(),
            1,
            "the printed symbol must be found exactly once, found {}",
            grids.len()
        );
        grids[0].decode().expect("the printed symbol decodes").1
    }

    /// The wallet revisions this test speaks for, and why there are two of them.

    /// `v3.0.0` is the parser as it was before: it knows `to`, `amount`, `token`, `mode` and
    /// ignores everything else. `rc/2` is the one that shipped `network` and `flag`. BOTH are worth
    /// running against, and they prove different halves:

    /// * against `v3.0.0` -- that appending the two new fields did not break a reader that has
    /// never heard of them. This is the forward-compatibility promise the protocol's own
    /// "unknown query parameters are ignored on parse" rests on, and it is a promise about a
    /// wallet already installed on somebody's phone, which is the one we cannot update;
    /// * against `rc/2` -- that the fields we emit are the fields it reads, spelled the way it
    /// spells them.

    /// Neither is a tag: the wallet has one tag (`v1.2.0`) and it predates both. Pinning to branch
    /// names is what is available, so the test states which head it found rather than assuming.
    const WALLET_VERSIONS: [&str; 2] = ["v3.0.0", "rc/2"];

    /// The checkout and the revision it is on, so a caller can require what that revision proves.
    fn wallet_checkout() -> Option<(std::path::PathBuf, String)> {
        let directory = match std::env::var("DEXDO_WALLET_REPO") {
            Ok(given) => std::path::PathBuf::from(given),
            Err(_) => {
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../ackinacki-wallet")
            }
        };
        if !directory.join("src/shared/qr/payment_uri.ts").is_file() {
            eprintln!(
                "SKIPPED: no wallet checkout at {} (set DEXDO_WALLET_REPO)",
                directory.display()
            );
            return None;
        }
        // A different checkout is not evidence about this one: say which was found and stop.
        let head = std::process::Command::new("git")
            .args([
                "-C",
                &directory.to_string_lossy(),
                "rev-parse",
                "--abbrev-ref",
                "HEAD",
            ])
            .output()
            .ok()?;
        let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
        if !WALLET_VERSIONS.contains(&head.as_str()) {
            eprintln!(
                "SKIPPED: wallet checkout is on {head}, not one of {}",
                WALLET_VERSIONS.join(" / ")
            );
            return None;
        }
        eprintln!("wallet checkout is on {head}");
        if std::process::Command::new("bun")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("SKIPPED: bun is not installed, cannot run the wallet's TypeScript");
            return None;
        }
        Some((directory, head))
    }

    /// What the phone is going to do with the code this command prints -- run through the wallet's
    /// own source, not through a description of it.

    /// The format was agreed by reading `docs/flows/qr_payment_protocol.md`, and a format agreed by
    /// reading drifts silently: the wallet can tighten its parser in a release and nothing here
    /// would notice until an operator's camera came up empty. So the check starts at the picture,
    /// decodes it the way a camera would, and hands the result to `parseCompactPayment` and
    /// `buildPaymentNavTarget` as they are shipped in the wallet at one of `WALLET_VERSIONS`.

    /// Skipped, loudly, wherever the wallet checkout or `bun` is missing -- CI has neither. A skip
    /// prints which prerequisite was absent, so a silent pass cannot be mistaken for a green run.

    /// # Why this is not a closed circle

    /// Review read it as one -- decode what we printed, hand it back to ourselves -- and asked for
    /// a note saying so. Measured instead of agreed, and the reading does not hold: the decoded
    /// payload is compared against `manual_deploy_payment_link(address)`, rebuilt from the ADDRESS
    /// THE TEST SUPPLIED, not from the picture. A command that printed someone else's address
    /// would produce a link that does not match.

    /// Verified by mutation: making `write_manual_deploy_funding_qr` draw a foreign address fails
    /// this test and `the_address_is_offered_as_a_qr_code_beside_the_text` together, 2 of 2. So
    /// substitution is covered, and what this test adds on top is the half nothing else reaches --
    /// that the wallet's own parser, as shipped at the revision named on the run, accepts what we
    /// drew. Which revision that was decides what is REQUIRED of it -- see `WALLET_VERSIONS`.
    #[test]
    fn the_wallet_parses_the_code_this_command_prints() {
        let Some((wallet, revision)) = wallet_checkout() else {
            return;
        };
        // Spelled the way the link spells it, from the same constant: whole SHELL, no unit word.
        let requested_shell =
            (MANUAL_DEPLOY_REQUEST_RAW / dexdo_core::params::SHELL_UNIT).to_string();

        // Named here rather than taken from `current_network()`: that reads the environment and
        // caches the answer for the whole process, so a test using it would assert whatever the
        // machine happened to be pointed at. This is the label the code under test receives.
        let network = "mainnet";

        for address in REAL_MAINNET_ADDRESSES {
            let mut printed = Vec::new();
            super::write_manual_deploy_funding_qr(&mut printed, address, network);
            let printed = String::from_utf8(printed).expect("UTF-8");
            let scanned = decode_the_printed_code(&printed);
            assert_eq!(
                scanned,
                super::manual_deploy_payment_link(address, network),
                "the picture must carry the link the command built"
            );

            let script = format!(
                "import {{ parseCompactPayment }} from '{wallet}/src/shared/qr/payment_uri';\n\
                 import {{ buildPaymentNavTarget }} from '{wallet}/src/shared/qr/payment_routing';\n\
                 const parsed = parseCompactPayment(process.argv[1]);\n\
                 console.log(JSON.stringify({{ parsed, target: parsed ? buildPaymentNavTarget(parsed) : null }}));\n",
                wallet = wallet.display(),
            );
            let run = std::process::Command::new("bun")
                .arg("-e")
                .arg(&script)
                .arg(&scanned)
                .current_dir(&wallet)
                .output()
                .expect("run the wallet's parser");
            assert!(
                run.status.success(),
                "the wallet's parser did not run: {}",
                String::from_utf8_lossy(&run.stderr)
            );
            let read: serde_json::Value =
                serde_json::from_slice(&run.stdout).expect("the parser's answer is JSON");

            // A scanned code the wallet rejects is worse than no code: the operator points a camera
            // at it, gets nothing, and has no way to tell whose fault it is.
            assert!(
                !read["parsed"].is_null(),
                "the wallet read the code as a payment: {read}"
            );
            assert_eq!(
                read["parsed"]["to"].as_str(),
                Some(address),
                "the whole address survives the round trip: {read}"
            );
            assert_eq!(
                read["parsed"]["amount"].as_str(),
                Some(requested_shell.as_str()),
                "the amount the request asks for is the amount the wallet offers to send: {read}"
            );
            assert_eq!(
                read["parsed"]["token"].as_str(),
                Some(dexdo_core::params::SHELL_CURRENCY_ID.to_string().as_str()),
                "the token is SHELL: {read}"
            );
            assert_eq!(
                read["parsed"]["mode"].as_str(),
                Some("regular"),
                "an ordinary transfer, not the DEX flow: {read}"
            );

            // and the requirement is decided by the REVISION, not by the wallet's answer.
            // The first version of this branch asked whether the parser had reported `network` and
            // asserted only if it had -- so deleting the two fields from the link left it green on
            // every checkout, which is the one thing it exists to catch. The revision is already
            // known here, so it is what decides.
            if revision == "rc/2" {
                assert_eq!(
                    read["parsed"]["network"].as_str(),
                    Some(network),
                    "this revision reads `network`, and it must read back what the code stated: \
                     {read}"
                );
                assert_eq!(
                    read["parsed"]["flag"].as_u64(),
                    Some(16),
                    "the deploy code must arrive as recipient gas: {read}"
                );
            } else {
                // The older reader's only obligation is to keep ignoring what it does not know --
                // and that obligation is being tested, because the assertions above this block
                // just ran against a link carrying both fields.
                assert!(
                    read["parsed"]["network"].is_null(),
                    "{revision} predates  and should not be reporting a network: {read}"
                );
            }

            // Parsing is not arriving: this is the screen the person actually lands on.
            let target = read["target"].as_str().unwrap_or_default().to_string();
            assert!(
                target.starts_with(&format!("/send/{}?", dexdo_core::params::SHELL_CURRENCY_ID)),
                "the send screen opens on SHELL: {target}"
            );
            assert!(
                target.contains(&format!("amount={requested_shell}")),
                "with the amount filled in: {target}"
            );
        }
    }

    /// A run with nobody in front of it refuses instead of waiting.
    #[test]
    fn a_headless_run_is_refused_with_both_figures() {
        let address = format!("{0}::{0}", "ef6ecd30".repeat(8));
        let shown = render_manual_deploy_funding_refusal(&address, 500_000_000, "shellnet");

        assert!(shown.contains(&address), "{shown}");
        // Whatever `shell_amount` spells today: the figures must both be there, and they must be
        // the two the operator acts on -- what is there now, and what to send.
        assert!(
            shown.contains(&vmshell_amount(500_000_000)),
            "what it holds now: {shown}"
        );
        assert!(
            shown.contains(&vmshell_amount(MANUAL_DEPLOY_REQUEST_RAW)),
            "what to send: {shown}"
        );
        assert!(
            shown.contains("vmshell"),
            "a refusal names the same unit as the request: {shown}"
        );
        assert!(
            shown.to_lowercase().contains("nothing was written"),
            "a refusal on the money path says what it did not do: {shown}"
        );
    }

    /// The figure asked for is not the figure required, and both are deliberate.
    #[test]
    fn the_request_leaves_the_wallet_able_to_work_after_the_deploy() {
        let required = dexdo_core::params::OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE;
        assert!(
            MANUAL_DEPLOY_REQUEST_RAW > required,
            "asking for exactly the deploy budget leaves a wallet that cannot send its first \
             message: asked {MANUAL_DEPLOY_REQUEST_RAW}, required {required}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOT: &str = "0000000000000000000000000000000000000000000000000000000000000004::\
                       1111111111111111111111111111111111111111111111111111111111111111";
    const OWNER_PUBKEY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const STRANGER_PUBKEY: &str =
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const SECRET_HEX: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

    fn supported_wallet() -> ObservedHotWallet {
        ObservedHotWallet {
            status: "Active".to_string(),
            code_hash: Some(dexdo_core::canonical_multisig::CODE_HASH.to_string()),
            required_txn_confirms: 1,
            custodian_pubkeys: vec![format!("0x{OWNER_PUBKEY}")],
        }
    }

    fn key_file(dir: &Path) -> ManualSecretRef {
        let path = dir.join("hot.key");
        std::fs::write(&path, SECRET_HEX).expect("write test key file");
        ManualSecretRef {
            kind: ManualSecretKind::Key,
            path,
        }
    }

    fn bind(
        data_dir: &Path,
        secret: ManualSecretRef,
        signer: &str,
        observed: &ObservedHotWallet,
    ) -> Result<PathBuf> {
        let verified = verify_manual_hot_wallet(
            HOT,
            crate::cli::wallet::test_network_a(),
            secret,
            signer,
            observed,
            "0123456789abcdef0123456789abcdef".to_string(),
        )?;
        persist::onboard_manual_binding(
            data_dir,
            load_active_binding(data_dir)?.as_ref(),
            &verified,
        )
    }

    /// The positive control for every "nothing was saved" case below, and the shape the binding
    /// promises: the address plus a reference, with the secret only ever on the operator's disk.
    #[test]
    fn a_verified_wallet_is_bound_by_address_and_secret_file_reference() {
        let temp = tempfile::tempdir().expect("temp root");
        let secret = key_file(temp.path());
        let path = bind(
            temp.path(),
            secret.clone(),
            OWNER_PUBKEY,
            &supported_wallet(),
        )
        .expect("a verified manual wallet binds");

        let raw = std::fs::read_to_string(&path).expect("read binding");
        let binding: WalletBinding = serde_json::from_str(&raw).expect("parse binding");
        assert_eq!(binding.provider, WalletProvider::Manual);
        assert_eq!(binding.version, 1);
        assert_eq!(binding.hot_address, HOT);
        assert_eq!(
            binding.hot_key_file.as_deref(),
            secret.path.to_str().map(std::path::Path::new),
            "the binding must reference the operator's own secret file"
        );
        assert_eq!(binding.hot_seed_file, None);
        assert_eq!(
            load_active_binding(temp.path()).expect("reload"),
            Some(binding)
        );
    }

    /// The secret is the one thing that must never travel into the binding, so assert on the raw
    /// bytes rather than on the parsed fields: a future field would not be caught by the latter.
    #[test]
    fn the_secret_never_appears_inside_the_binding_file() {
        let temp = tempfile::tempdir().expect("temp root");
        let secret = key_file(temp.path());
        let path = bind(temp.path(), secret, OWNER_PUBKEY, &supported_wallet()).expect("bind");
        let raw = std::fs::read_to_string(&path).expect("read binding");
        assert!(
            !raw.contains(SECRET_HEX),
            "the binding copied the secret instead of referencing its file:\n{raw}"
        );
        assert!(
            raw.contains("hot.key"),
            "the binding must still reference the file:\n{raw}"
        );
    }

    /// The check the whole provider exists for: a key that does not own the wallet is refused, and
    /// the refusal leaves no binding for a later command to trust.
    #[test]
    fn a_key_that_does_not_belong_is_refused_and_nothing_is_saved() {
        let temp = tempfile::tempdir().expect("temp root");
        let secret = key_file(temp.path());
        let error = bind(temp.path(), secret, STRANGER_PUBKEY, &supported_wallet())
            .expect_err("a non-custodian key must be refused");
        let message = format!("{error}");
        assert!(
            message.contains("is not a custodian"),
            "unexpected refusal: {message}"
        );
        assert!(
            message.contains("nothing was written"),
            "the refusal must say nothing was written: {message}"
        );
        assert!(!active_binding_path(temp.path()).exists());
        assert!(!temp.path().join("wallet").exists());
        assert_eq!(load_active_binding(temp.path()).expect("reload"), None);
    }

    /// A wallet with no pubkey custodians can never be signed for, so it is refused before the
    /// signer is even compared.
    #[test]
    fn a_wallet_without_pubkey_custodians_is_refused() {
        let temp = tempfile::tempdir().expect("temp root");
        let secret = key_file(temp.path());
        let mut observed = supported_wallet();
        observed.custodian_pubkeys = vec!["not-a-key".to_string()];
        let error = bind(temp.path(), secret, OWNER_PUBKEY, &observed).expect_err("refused");
        assert!(format!("{error}").contains("no pubkey custodians"));
        assert!(!active_binding_path(temp.path()).exists());
    }

    /// A higher threshold means a Vault was handed over where a Hot was wanted; dexdo signs alone,
    /// so binding it would record a wallet no dexdo command can spend from.
    #[test]
    fn a_threshold_above_one_is_refused_and_nothing_is_saved() {
        let temp = tempfile::tempdir().expect("temp root");
        let secret = key_file(temp.path());
        let mut observed = supported_wallet();
        observed.required_txn_confirms = 2;
        let error = bind(temp.path(), secret, OWNER_PUBKEY, &observed).expect_err("refused");
        let message = format!("{error}");
        assert!(message.contains("reqConfirms=1"), "{message}");
        assert!(!active_binding_path(temp.path()).exists());
    }

    /// An unsupported family is refused by code hash, naming both accepted hashes so the operator
    /// can tell which wallet they actually have.
    #[test]
    fn an_unsupported_multisig_is_refused_and_nothing_is_saved() {
        let temp = tempfile::tempdir().expect("temp root");
        let secret = key_file(temp.path());
        let mut observed = supported_wallet();
        observed.code_hash =
            Some("3a7a53248ff39fde936a4274eab143b5fac94feac0d8e2e2748aac5e74538d5f".to_string());
        let error = bind(temp.path(), secret, OWNER_PUBKEY, &observed).expect_err("refused");
        let message = format!("{error}");
        assert!(message.contains("not a supported multisig"), "{message}");
        assert!(
            message.contains(dexdo_core::canonical_multisig::CODE_HASH),
            "{message}"
        );
        assert!(!active_binding_path(temp.path()).exists());
    }

    /// A legacy spending hash is one of the two dexdo actually spends from, so it must bind.
    #[test]
    fn the_legacy_spending_code_hash_is_accepted() {
        let temp = tempfile::tempdir().expect("temp root");
        let secret = key_file(temp.path());
        let mut observed = supported_wallet();
        observed.code_hash =
            Some(dexdo_core::canonical_multisig::LEGACY_SPENDING_CODE_HASH.to_string());
        bind(temp.path(), secret, OWNER_PUBKEY, &observed).expect("legacy spending hash binds");
    }

    /// An address that is not Active is refused before its getters are trusted.
    #[test]
    fn a_wallet_that_is_not_active_is_refused() {
        let temp = tempfile::tempdir().expect("temp root");
        let secret = key_file(temp.path());
        for status in ["NotFound", "Uninit", "NonExist", "Frozen"] {
            let observed = ObservedHotWallet {
                status: status.to_string(),
                code_hash: None,
                required_txn_confirms: 0,
                custodian_pubkeys: Vec::new(),
            };
            let error = bind(temp.path(), secret.clone(), OWNER_PUBKEY, &observed)
                .expect_err("a non-Active account must be refused");
            assert!(format!("{error}").contains("is not Active"), "{error}");
        }
        assert!(!active_binding_path(temp.path()).exists());
    }

    /// The order is a contract of its own: an operator handed several problems at once must be told
    /// about the earliest one, because that is the one whose fix changes what the rest report.
    #[test]
    fn checks_run_in_the_documented_order() {
        let temp = tempfile::tempdir().expect("temp root");
        let secret = key_file(temp.path());
        // Every check below the first one is also broken here.
        let all_broken = ObservedHotWallet {
            status: "Uninit".to_string(),
            code_hash: Some("00".repeat(32)),
            required_txn_confirms: 7,
            custodian_pubkeys: vec![STRANGER_PUBKEY.to_string()],
        };
        let bad_address = verify_manual_hot_wallet(
            "not-an-address",
            crate::cli::wallet::test_network_a(),
            secret.clone(),
            OWNER_PUBKEY,
            &all_broken,
            "id".to_string(),
        )
        .expect_err("address first");
        assert!(format!("{bad_address}").contains("--multisig-address"));

        let inactive = bind(temp.path(), secret.clone(), OWNER_PUBKEY, &all_broken)
            .expect_err("status before code hash");
        assert!(
            format!("{inactive}").contains("is not Active"),
            "{inactive}"
        );

        let mut active = all_broken.clone();
        active.status = "Active".to_string();
        let bad_hash =
            bind(temp.path(), secret.clone(), OWNER_PUBKEY, &active).expect_err("code hash next");
        assert!(
            format!("{bad_hash}").contains("not a supported multisig"),
            "{bad_hash}"
        );

        let mut supported = active.clone();
        supported.code_hash = Some(dexdo_core::canonical_multisig::CODE_HASH.to_string());
        let bad_threshold = bind(temp.path(), secret.clone(), OWNER_PUBKEY, &supported)
            .expect_err("threshold next");
        assert!(
            format!("{bad_threshold}").contains("reqConfirms=1"),
            "{bad_threshold}"
        );

        let mut threshold_ok = supported.clone();
        threshold_ok.required_txn_confirms = 1;
        let bad_custodian =
            bind(temp.path(), secret, OWNER_PUBKEY, &threshold_ok).expect_err("custodian last");
        assert!(
            format!("{bad_custodian}").contains("is not a custodian"),
            "{bad_custodian}"
        );
        assert!(!active_binding_path(temp.path()).exists());
    }

    /// An already-bound Hot may still hold funds, so onboarding never overwrites it.
    #[test]
    fn an_existing_binding_is_never_replaced_by_onboarding() {
        let temp = tempfile::tempdir().expect("temp root");
        let secret = key_file(temp.path());
        let path = bind(
            temp.path(),
            secret.clone(),
            OWNER_PUBKEY,
            &supported_wallet(),
        )
        .expect("first bind");
        let first = std::fs::read_to_string(&path).expect("read binding");

        let error = bind(temp.path(), secret, OWNER_PUBKEY, &supported_wallet())
            .expect_err("a second onboarding must be refused");
        let message = format!("{error}");
        assert!(message.contains("already bound"), "{message}");
        assert!(message.contains("rebind"), "{message}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("re-read binding"),
            first,
            "the refused onboarding rewrote the active binding"
        );
    }

    /// The secret file is owner-only, and so is the binding beside it.
    #[cfg(unix)]
    #[test]
    fn the_binding_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().expect("temp root");
        let secret = key_file(temp.path());
        let path = bind(temp.path(), secret, OWNER_PUBKEY, &supported_wallet()).expect("bind");
        let mode = std::fs::metadata(&path)
            .expect("binding metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the binding must be owner-only");
        let dir_mode = std::fs::metadata(temp.path().join("wallet"))
            .expect("wallet dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "the wallet directory must be owner-only");
    }

    /// Only the interactive form has to tell the two files apart, and it does so strictly: a file
    /// that is neither is refused rather than fed to the wrong reader.
    #[test]
    fn secret_files_are_classified_or_refused() {
        assert_eq!(
            classify_manual_secret_file(&format!("  {SECRET_HEX}\n")).expect("key"),
            ManualSecretKind::Key
        );
        let phrase = "abandon ".repeat(11) + "about";
        assert_eq!(
            classify_manual_secret_file(&phrase).expect("phrase"),
            ManualSecretKind::SeedPhrase
        );
        for ambiguous in [
            "",
            "short",
            "dead beef",
            &"abandon ".repeat(13),
            &format!("{SECRET_HEX} {SECRET_HEX}"),
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon ab0ut",
        ] {
            assert!(
                classify_manual_secret_file(ambiguous).is_err(),
                "{ambiguous:?} must be refused rather than guessed"
            );
        }
    }

    /// A reader that reports a fixed sequence of balances and counts how often it was asked.
    struct ScriptedBalances {
        readings: std::cell::RefCell<std::vec::IntoIter<u128>>,
        last: std::cell::Cell<u128>,
        reads: std::cell::Cell<usize>,
    }

    impl ScriptedBalances {
        fn new(readings: Vec<u128>) -> Self {
            Self {
                readings: std::cell::RefCell::new(readings.into_iter()),
                last: std::cell::Cell::new(0),
                reads: std::cell::Cell::new(0),
            }
        }
    }

    #[async_trait::async_trait(?Send)]
    impl HotEccReader for ScriptedBalances {
        async fn read_hot_ecc_raw(&self, _currency_id: u32) -> Result<u128> {
            self.reads.set(self.reads.get() + 1);
            let next = self.readings.borrow_mut().next();
            let value = next.unwrap_or_else(|| self.last.get());
            self.last.set(value);
            Ok(value)
        }
    }

    const REQUIREMENT: HotFundingRequirement = HotFundingRequirement {
        currency_id: dexdo_core::params::SHELL_CURRENCY_ID,
        required_raw: 1_000,
    };

    /// The shortfall the operator acts on has to be exact and has to name the address in full;
    /// "not enough SHELL" sends money to the wrong place or not at all.
    #[test]
    fn the_shortfall_message_carries_the_exact_amount_and_the_full_address() {
        let text = render_manual_funding_shortfall(
            HOT,
            "note deploy",
            REQUIREMENT,
            250,
            Duration::from_secs(600),
        );
        assert!(
            text.contains(HOT),
            "the full canonical address must appear:\n{text}"
        );
        // SHELL is what the operator sends; the raw figure beside it is what an explorer shows.
        assert!(text.contains("missing 0.00000075 (750 raw)"), "{text}");
        assert!(text.contains("0.000001 (1000 raw) SHELL ECC[2]"), "{text}");
        assert!(text.contains("holds 0.00000025 (250 raw)"), "{text}");
        assert!(text.contains("600 seconds"), "{text}");
        // The manual provider has no request to create and no service to send anyone to.
        for forbidden in ["Vault", "vault", "http", "gosh.ai", "Gosh.ai"] {
            assert!(
                !text.contains(forbidden),
                "the manual shortfall must not mention {forbidden}:\n{text}"
            );
        }
    }

    /// The timeout message is the other half: it has to state that nothing was left behind, and
    /// that the same command is the way forward.
    #[test]
    fn the_timeout_message_states_that_nothing_was_written() {
        let text = render_manual_funding_timeout(
            HOT,
            "note topup",
            REQUIREMENT,
            250,
            Duration::from_secs(600),
        );
        assert!(text.contains(HOT), "{text}");
        assert!(
            text.contains("0.00000075 (750 raw) is still missing"),
            "{text}"
        );
        assert!(text.contains("no local state was written"), "{text}");
        assert!(text.contains("run the same command again"), "{text}");
        for forbidden in ["Vault", "vault", "http", "gosh.ai"] {
            assert!(!text.contains(forbidden), "{text}");
        }
    }

    /// Enough already there means no wait at all, and exactly one read.
    #[tokio::test(start_paused = true)]
    async fn a_funded_hot_is_not_waited_for() {
        let reader = ScriptedBalances::new(vec![1_000]);
        let available = ensure_manual_hot_funded(
            &reader,
            HOT,
            "note deploy",
            REQUIREMENT,
            Duration::from_secs(600),
            Duration::from_secs(5),
        )
        .await
        .expect("already funded");
        assert_eq!(available, 1_000);
        assert_eq!(reader.reads.get(), 1);
    }

    /// A top-up that lands during the wait is picked up by the balance, not by any notification.
    #[tokio::test(start_paused = true)]
    async fn a_top_up_that_lands_during_the_wait_is_observed() {
        let reader = ScriptedBalances::new(vec![0, 0, 250, 1_000]);
        let available = ensure_manual_hot_funded(
            &reader,
            HOT,
            "note deploy",
            REQUIREMENT,
            Duration::from_secs(600),
            Duration::from_secs(5),
        )
        .await
        .expect("funded while waiting");
        assert_eq!(available, 1_000);
        assert_eq!(reader.reads.get(), 4);
    }

    /// The property the retry depends on: a timeout writes nothing, so a second run starts from the
    /// state the first one did and re-reads the chain rather than resuming a different path.
    #[tokio::test(start_paused = true)]
    async fn a_timeout_writes_nothing_and_the_rerun_rechecks_the_balance() {
        let temp = tempfile::tempdir().expect("temp root");
        let before = snapshot_dir(temp.path());

        let reader = ScriptedBalances::new(vec![0; 8]);
        let error = ensure_manual_hot_funded(
            &reader,
            HOT,
            "note deploy",
            REQUIREMENT,
            Duration::from_secs(20),
            Duration::from_secs(5),
        )
        .await
        .expect_err("the wait must run out");
        assert!(
            format!("{error}").contains("run the same command again"),
            "{error}"
        );
        let first_run_reads = reader.reads.get();
        assert!(
            first_run_reads >= 2,
            "the wait must poll: {first_run_reads}"
        );
        assert_eq!(
            snapshot_dir(temp.path()),
            before,
            "the timed-out wait left state behind"
        );

        // The rerun: the same code path, a chain that has since been funded. It must reach the same
        // first read, not skip it because a previous run already decided the balance was short.
        let rerun = ScriptedBalances::new(vec![1_000]);
        let available = ensure_manual_hot_funded(
            &rerun,
            HOT,
            "note deploy",
            REQUIREMENT,
            Duration::from_secs(20),
            Duration::from_secs(5),
        )
        .await
        .expect("the rerun sees the funded balance");
        assert_eq!(available, 1_000);
        assert_eq!(rerun.reads.get(), 1, "the rerun must re-read the balance");
        assert_eq!(snapshot_dir(temp.path()), before);
    }

    /// The defaults the shared funding step will use are the specification's, and they are pinned
    /// here: ten minutes, read every five seconds. A wait that silently became a different length
    /// is the difference between an operator's transfer landing in time and not.
    #[tokio::test(start_paused = true)]
    async fn the_default_wait_is_ten_minutes_polled_every_five_seconds() {
        assert_eq!(
            dexdo_core::params::WALLET_HOT_FUNDING_TIMEOUT,
            Duration::from_secs(600)
        );
        assert_eq!(
            dexdo_core::params::NOTE_DEPLOY_SHELL_FUNDING_POLL_INTERVAL,
            Duration::from_secs(5)
        );
        let started = tokio::time::Instant::now();
        let reader = ScriptedBalances::new(vec![0; 4096]);
        let error =
            ensure_manual_hot_funded_with_defaults(&reader, HOT, "note deploy", REQUIREMENT)
                .await
                .expect_err("an unfunded Hot must time out");
        assert!(format!("{error}").contains("within 600 seconds"), "{error}");
        assert_eq!(
            tokio::time::Instant::now().duration_since(started),
            Duration::from_secs(600)
        );
        assert_eq!(
            reader.reads.get(),
            122,
            "the read that opened the wait, then one every 5s through the 600s deadline inclusive"
        );
    }

    /// The wait is bounded by the timeout it was given, not by the poll interval dividing it.
    #[tokio::test(start_paused = true)]
    async fn the_wait_stops_at_the_timeout() {
        let started = tokio::time::Instant::now();
        let reader = ScriptedBalances::new(vec![0; 64]);
        let outcome = wait_for_manual_hot_funding(
            &reader,
            REQUIREMENT,
            Duration::from_secs(22),
            Duration::from_secs(5),
        )
        .await
        .expect("the wait completes");
        assert_eq!(outcome, ManualFundingOutcome::TimedOut { available_raw: 0 });
        let elapsed = tokio::time::Instant::now().duration_since(started);
        assert_eq!(
            elapsed,
            Duration::from_secs(22),
            "the wait must end exactly at its deadline"
        );
    }

    fn snapshot_dir(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    out.push((path, bytes));
                }
            }
        }
        out.sort();
        out
    }

    /// Property 1, at the level a test can reach inside one crate: the only way to a saved binding
    /// runs through verification, and the writer is unreachable from any other module.

    /// `save_active_binding` is private to `persist` and takes a `VerifiedManualWallet` whose field
    /// is private, so no working command can construct the argument even if it could name the
    /// function. This pins the one thing a refactor could quietly undo: that the writer has exactly
    /// one call site, inside the onboarding entry.
    #[test]
    fn the_binding_writer_has_exactly_one_call_site_and_it_is_onboarding() {
        let source = include_str!("wallet_manual.rs");
        let production = source
            .split_once("\n#[cfg(test)]\nmod tests {")
            .expect("unit-test module boundary")
            .0;
        assert_eq!(
            production.matches("save_active_binding(").count(),
            2,
            "save_active_binding must be defined once and called once, both inside this module"
        );
        assert!(
            production.contains("fn onboard_manual_binding(")
                && production
                    .split_once("fn onboard_manual_binding(")
                    .expect("onboarding entry")
                    .1
                    .contains("save_active_binding(data_dir, verified)"),
            "the single call site must be the onboarding entry"
        );
        assert!(
            !production.contains("pub(crate) fn save_active_binding"),
            "the writer must not be reachable from another module"
        );
    }
}

/// the single-writer rule, held by the compiler and by a count a space cannot defeat.
#[cfg(test)]
#[path = "wallet_manual_single_writer_1313.rs"]
mod wallet_manual_single_writer_1313;

/// the QR says which chain it asks for, in a form the wallet reads and one a human reads.
/// The canonical address is identical on both chains, so nothing else on this path can tell them
/// apart.
#[cfg(test)]
mod the_payment_qr_names_its_network_1638;
