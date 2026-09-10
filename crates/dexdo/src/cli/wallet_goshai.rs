//! The gosh-ai provider of `dexdo wallet onboard`: adopt a Hot the user already owns at
//! Gosh.ai.

//! Every mention of the command here names it without its provider argument on purpose. The lint in
//! `main.rs` (`no_command_line_in_these_sources_is_rejected_by_the_parser`) requires any backticked
//! command line to be one the shipped parser accepts, and until `wallet onboard <provider>` exists
//! the two-word form is the only honest thing to print: telling a user to run a line this binary
//! rejects is a dead end. Once the provider subcommands land, these spans can take the provider
//! argument back inside the backticks, and that same lint will keep them true.

//! Gosh.ai provides a Hot and no Vault. It establishes no session with the CLI: there is no one-time
//! code, no API exchange, no authentication of dexdo to Gosh.ai and no message back. The whole
//! handoff is one string the user copies out of the Gosh.ai UI, holding the Hot's canonical
//! self-dApp address and a 12-word recovery phrase separated by one ASCII space, in either order.

//! Everything security-relevant about that is in this file and is written to be provable without a
//! chain:

//! - [`parse_wallet_string`] and the two single-part parsers are total functions over the pasted
//! text. They return the two extracted values or a [`PasteFault`] - a closed set of `'static`
//! error classes. **No variant of `PasteFault` carries any part of what the user typed**, so no
//! caller can echo the paste even by accident, at any verbosity;
//! - [`collect_wallet_string`] drives the prompts through the [`HiddenPrompt`] seam. Production
//! supplies a non-echoing terminal; a test supplies a script. The loop asks only for the half it
//! is missing and never re-asks for a half it already holds;
//! - [`prepare_onboarding`] is the whole pre-network stage in one function, so the order "validate,
//! derive, only then touch the disk" is a property a test can hold rather than a convention;
//! - [`classify_hot_poll`] decides what an account read means while an asynchronous deploy is in
//! flight, and [`verify_active_hot`] is the refusal list applied once it is `Active`.

//! Where this module stops: it does not own the `WalletProvider` vocabulary, the durable
//! `wallet/binding.json` schema, the interactive provider chooser, `wallet rebind`, the manual
//! provider or the funding journal. [`GoshAiBinding`] is the minimum this flow must be able to
//! write and is expected to be replaced by the shared binding type when the two land together.

use anyhow::{bail, Context, Result};
use dexdo_core::CanonicalAddress;
use qrcode::QrCode;
use zeroize::Zeroizing;

// the shared provider vocabulary. `provider` is a closed type and not a string, so a binding
// cannot claim a provider nothing validated.

// `network` was a closed type here too, and made it the manifest's own label, because which
// chains exist is not this client's to know. What the closed pair used to buy is now bought
// differently: the label is not checked against a list -- there is none -- but it IS checked to be
// a plain file name (`WalletNetwork::from_manifest_label`), because that is what it is used as, and
// a binding is only ever read back out of `wallet/active/`. A Hot bound under one label still
// cannot be spent as though it were under another: the label is part of the PATH, so the two
// records are different files, and no field comparison has to be trusted for that to hold.
use crate::cli::wallet::{WalletNetwork, WalletProvider};

/// A Gosh.ai recovery phrase is 12 words today. The spec states the count explicitly rather than
/// "whatever BIP-39 allows", so the parser states it too: a 24-word phrase is not a Gosh.ai phrase.
pub(crate) const GOSHAI_PHRASE_WORDS: usize = 12;

/// Where this deployment says an operator gets a Gosh.ai wallet, or why they cannot.

/// The link used to be a constant in this file, and a temporary one at that -- the wallet-team spec
/// called it a placeholder to be replaced before release. replaced it, and moved it: it is
/// the manifest that says whether Gosh.ai issues wallets for the chain the operator is pointed at,
/// because that is a fact about the deployment and not about a name the client keeps a list of
/// .

/// A named decision rather than an `if` at the call site, so it can be TESTED: the refusal must
/// happen BEFORE anything is printed or encoded, and the only way to prove that is to drive the
/// decision on its own.

/// Nothing here parses or follows the URL -- it is shown and rendered as a QR, nothing more.
/// The manifest's own network label, made safe to print.

/// The label comes out of the same downloaded file as the URL, and `Deployed::load` is a plain
/// deserialize with no field validation -- so a document that can carry a hostile link can carry a
/// hostile label. These refusals put it on the screen that immediately precedes a recovery-phrase
/// prompt, so it goes through the same filter the link does. Anything outside printable ASCII is
/// replaced rather than dropped: a label rendered as `net?a` still tells the operator which
/// deployment answered, and silently deleting characters would not.
fn display_label(network: &str) -> String {
    network
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .take(64)
        .collect()
}

pub(crate) fn goshai_invitation_url(manifest: &dexdo_core::Deployed) -> Result<&str> {
    match manifest.goshai_onboarding_url.as_deref().map(str::trim) {
        Some(url) if !url.is_empty() => {
            // CHECKED, because the value now comes from a file the operator DOWNLOADS, and the
            // next thing this flow does is ask for a recovery phrase. It used to be a compiled
            // constant that no input could reach; moving it into the manifest is what makes these
            // three checks necessary rather than fussy.

            // The control-character one is the one worth spelling out: a `\r` or an escape
            // sequence in this string makes the terminal rewrite the line, so what the operator
            // READS stops being what the QR ENCODES -- and a byte comparison in a captured buffer
            // cannot see that, because the divergence happens in the terminal.
            if !url.starts_with("https://") {
                bail!(
                    "the manifest for {network} declares a Gosh.ai onboarding link that is not \
                     https. A wallet invitation is followed with a phone and answered with a \
                     recovery phrase, so it is not shown unless the transport is.",
                    network = display_label(&manifest.network),
                );
            }
            // `is_ascii_graphic()`, not `is_control() || is_whitespace()`. Those two are Cc and
            // White_Space, and neither covers Cf: measured, `U+202E` (right-to-left override),
            // `U+200B` (zero-width space) and `U+202D` all passed the pair. A bidi override is the
            // textbook way to make a terminal render something other than its bytes, which is
            // exactly the "what is READ stops being what the QR ENCODES" failure this check exists
            // to prevent. URLs are ASCII by specification -- an international domain arrives as
            // punycode -- so the whole class goes at once.
            if !url.chars().all(|c| c.is_ascii_graphic()) {
                bail!(
                    "the manifest for {network} declares a Gosh.ai onboarding link containing \
                     characters a URL cannot carry -- whitespace, control or formatting. Shown on \
                     a terminal such a string can rewrite or reorder its own line, so what is read \
                     and what the QR encodes stop being the same thing.",
                    network = display_label(&manifest.network),
                );
            }
            // A scheme with nothing after it is not a link: it would be printed and QR-encoded as
            // an invitation to nowhere.
            if url.len() <= "https://".len() {
                bail!(
                    "the manifest for {network} declares a Gosh.ai onboarding link with no host.",
                    network = display_label(&manifest.network),
                );
            }
            Ok(url)
        }
        _ => bail!(
            "Gosh.ai does not issue wallets on {network}, so there is no sub-wallet to onboard \
             from -- or this manifest predates the field that says where it does. Nothing was \
             asked for and nothing was written. Bind a wallet you control on {network} with \
             `dexdo wallet onboard`, or point DEXDO_MANIFEST at a deployment whose manifest \
             declares Gosh.ai onboarding.",
            network = display_label(&manifest.network),
        ),
    }
}

/// Show the invitation: one string, read by the eye and by the camera.

/// Both come from the same argument on purpose. A QR whose payload differs from the text beside it
/// is trusted without being read -- that is what a printed code is for -- so there is no second
/// place for the two to disagree.
pub(crate) fn render_goshai_invitation(out: &mut dyn std::io::Write, url: &str) -> Result<()> {
    let code = QrCode::new(url.as_bytes()).context("render the Gosh.ai link as a QR code")?;
    writeln!(
        out,
        "Open Gosh.ai and copy the one-line wallet string for your sub-wallet."
    )?;
    crate::cli::qr_display::write_qr(out, &code).context("render the Gosh.ai QR code")?;
    writeln!(out, "{url}")?;
    out.flush().context("flush the Gosh.ai invitation")
}

/// The prompts. They are `'static` text: a prompt is never built from user input.
pub(crate) const PROMPT_WHOLE_STRING: &str =
    "Paste the Gosh.ai wallet string (address and recovery phrase, input stays hidden): ";
pub(crate) const PROMPT_ADDRESS_ONLY: &str =
    "Paste the Gosh.ai Hot address only (input stays hidden): ";
pub(crate) const PROMPT_PHRASE_ONLY: &str =
    "Paste the Gosh.ai recovery phrase only, 12 words (input stays hidden): ";

// ---------------------------------------------------------------------------------------------
// What can be wrong with a paste
// ---------------------------------------------------------------------------------------------

/// The classes of fault a paste can have.

/// Every message below is a compile-time constant. That is the point: the user's clipboard held a
/// recovery phrase, so the one thing this program must never do is repeat it - not on the terminal,
/// not in a log line, not inside an error that some outer layer will render. Keeping the fault set
/// free of owned strings makes that structural instead of a rule someone has to remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteFault {
    /// Nothing but whitespace was entered.
    Empty,
    /// Something other than single ASCII spaces separates the blocks (tab, newline, double space).
    Separator,
    /// Both halves were recognised, but not as the two accepted shapes.
    Shape,
    /// No token looked like an address at all.
    AddressMissing,
    /// More than one token looked like an address.
    AddressAmbiguous,
    /// A token looked like an address and is not a valid canonical one.
    AddressMalformed,
    /// A valid canonical address that is not a self-dApp account.
    AddressNotSelfDapp,
    /// No run of 12 words formed a recovery phrase.
    PhraseMissing,
    /// More than one distinct run of 12 words formed a valid recovery phrase.
    PhraseAmbiguous,
    /// Twelve wordlist words that do not checksum: a real phrase, mistyped or truncated.
    PhraseInvalid,
}

impl PasteFault {
    /// A stable, machine-comparable class name. Carries no user input.
    pub(crate) const fn class(self) -> &'static str {
        match self {
            Self::Empty => "empty_input",
            Self::Separator => "bad_separator",
            Self::Shape => "bad_shape",
            Self::AddressMissing => "address_missing",
            Self::AddressAmbiguous => "address_ambiguous",
            Self::AddressMalformed => "address_malformed",
            Self::AddressNotSelfDapp => "address_not_self_dapp",
            Self::PhraseMissing => "phrase_missing",
            Self::PhraseAmbiguous => "phrase_ambiguous",
            Self::PhraseInvalid => "phrase_invalid",
        }
    }

    /// One line telling the user what to do. Carries no user input.
    pub(crate) const fn advice(self) -> &'static str {
        match self {
            Self::Empty => "nothing was entered",
            Self::Separator => {
                "the two blocks must be separated by exactly one ASCII space, and the phrase words \
                 by one space each: no tabs, no line breaks, no double spaces"
            }
            Self::Shape => {
                "the string must be exactly the Hot address and the 12-word recovery phrase, in \
                 either order, with nothing else in it"
            }
            Self::AddressMissing => "no `<dapp_id>::<account_id>` address was found",
            Self::AddressAmbiguous => "more than one address was found; paste exactly one",
            Self::AddressMalformed => {
                "the address is not canonical `<dapp_id>::<account_id>` with two 64-hex halves"
            }
            Self::AddressNotSelfDapp => {
                "the address is canonical but not a self-dApp account; a Hot wallet's dApp id \
                 equals its account id"
            }
            Self::PhraseMissing => "no 12-word recovery phrase was found",
            Self::PhraseAmbiguous => {
                "more than one 12-word recovery phrase was found; paste exactly one"
            }
            Self::PhraseInvalid => {
                "the 12 words are not a valid recovery phrase; check for a mistyped or reordered \
                 word"
            }
        }
    }
}

impl std::fmt::Display for PasteFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.class(), self.advice())
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing the pasted string
// ---------------------------------------------------------------------------------------------

/// Both halves of the Gosh.ai handoff, validated.
pub(crate) struct GoshAiWalletString {
    pub(crate) hot_address: CanonicalAddress,
    /// The 12 words, single-spaced. Wiped when dropped.
    pub(crate) phrase: Zeroizing<String>,
}

impl std::fmt::Debug for GoshAiWalletString {
    /// The phrase is the wallet. It never reaches a formatter, including the one a `#[derive]`
    /// would have written and that some `tracing` field or `{:?}` in an error would then print.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoshAiWalletString")
            .field("hot_address", &self.hot_address.to_string())
            .finish_non_exhaustive()
    }
}

/// What one paste of the whole string yielded.
pub(crate) enum PasteOutcome {
    /// Both halves, in the accepted shape.
    Complete(GoshAiWalletString),
    /// The phrase was recognised; ask for the address alone.
    NeedAddress {
        phrase: Zeroizing<String>,
        why: PasteFault,
    },
    /// The address was recognised; ask for the phrase alone.
    NeedPhrase {
        hot_address: CanonicalAddress,
        why: PasteFault,
    },
    /// Neither half is usable; report the class and ask for the whole string again.
    NeedWholeString { why: PasteFault },
}

/// Split the paste into tokens under the separator rule.

/// Only the outer edges of the whole pasted string are trimmed, because a terminal paste routinely
/// carries a trailing newline that the user did not choose to add. Inside, the contract is exactly
/// single ASCII spaces: a tab, a line break or a double space means the copy did not come from the
/// one-button copy this flow is the counterpart of, and guessing what such a string meant is how a
/// wrong address gets funded.
fn tokens_of(input: &str) -> Result<Vec<&str>, PasteFault> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(PasteFault::Empty);
    }
    if trimmed.chars().any(|c| c.is_whitespace() && c != ' ') || trimmed.contains("  ") {
        return Err(PasteFault::Separator);
    }
    Ok(trimmed.split(' ').collect())
}

/// A Hot wallet is a self-dApp account: its dApp id equals its account id.

/// Operational wallets are not children of the dexdo DApp, so an address whose halves differ is
/// some other account - possibly a dexdo contract - and adopting it as a Hot would point every
/// later `note deploy`/`note topup` at the wrong place.
fn is_self_dapp(address: &CanonicalAddress) -> bool {
    address.dapp_id() == address.account_id()
}

/// Locate the single address token.

/// A token is treated as an *attempted* address when it contains `::`, and an attempted address
/// that does not parse is reported as malformed rather than as absent. That distinction is what
/// tells a user who mangled the copy from a user who pasted only the phrase, without either message
/// containing a character they typed. It also means the legacy `0:<hex>` form cannot slip in: it
/// carries no dApp identity, and this flow needs the canonical form to know the Hot's dApp.
fn address_in(tokens: &[&str]) -> Result<(usize, CanonicalAddress), PasteFault> {
    let mut candidates = tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| token.contains("::"));
    let Some((index, token)) = candidates.next() else {
        return Err(PasteFault::AddressMissing);
    };
    if candidates.next().is_some() {
        return Err(PasteFault::AddressAmbiguous);
    }
    let address = CanonicalAddress::parse(token).map_err(|_| PasteFault::AddressMalformed)?;
    if !is_self_dapp(&address) {
        return Err(PasteFault::AddressNotSelfDapp);
    }
    Ok((index, address))
}

fn is_wordlist_word(word: &str) -> bool {
    bip39::Language::English
        .wordlist()
        .get_words_by_prefix(word)
        .contains(&word)
}

fn is_valid_phrase(words: &[&str]) -> bool {
    let joined = Zeroizing::new(words.join(" "));
    bip39::Mnemonic::validate(&joined, bip39::Language::English).is_ok()
}

/// Locate the single 12-word phrase.

/// Every window of 12 consecutive tokens that does not overlap the address is tried, which is what
/// makes the order of the two blocks irrelevant without the parser having to know which order it is
/// looking at. Two distinct valid windows - a phrase pasted twice, or 24 words - is ambiguity, and
/// ambiguity is re-prompted rather than resolved by picking one.
fn phrase_in(tokens: &[&str], address_index: Option<usize>) -> Result<(usize, String), PasteFault> {
    if tokens.len() < GOSHAI_PHRASE_WORDS {
        return Err(PasteFault::PhraseMissing);
    }
    let overlaps_address = |start: usize| {
        address_index.is_some_and(|index| index >= start && index < start + GOSHAI_PHRASE_WORDS)
    };
    let mut valid: Option<usize> = None;
    let mut wordlist_only: Option<usize> = None;
    for start in 0..=tokens.len() - GOSHAI_PHRASE_WORDS {
        if overlaps_address(start) {
            continue;
        }
        let window = &tokens[start..start + GOSHAI_PHRASE_WORDS];
        if !window.iter().all(|word| is_wordlist_word(word)) {
            continue;
        }
        wordlist_only.get_or_insert(start);
        if is_valid_phrase(window) {
            if valid.is_some() {
                return Err(PasteFault::PhraseAmbiguous);
            }
            valid = Some(start);
        }
    }
    match (valid, wordlist_only) {
        (Some(start), _) => Ok((start, tokens[start..start + GOSHAI_PHRASE_WORDS].join(" "))),
        // Twelve real wordlist words that do not checksum is a mistyped phrase, not a missing one.
        (None, Some(_)) => Err(PasteFault::PhraseInvalid),
        (None, None) => Err(PasteFault::PhraseMissing),
    }
}

/// The two accepted shapes, and only those: address then phrase, or phrase then address.

/// Rejecting anything else is what keeps a label, a stray word or a glued
/// `<dapp_id>::<account_id><word>` from being quietly accepted because the parts it contains
/// happened to be extractable.
fn shape_is_accepted(token_count: usize, address_index: usize, phrase_start: usize) -> bool {
    token_count == GOSHAI_PHRASE_WORDS + 1
        && ((address_index == 0 && phrase_start == 1)
            || (address_index == GOSHAI_PHRASE_WORDS && phrase_start == 0))
}

/// Extract both halves from one pasted string, in either order.
pub(crate) fn parse_wallet_string(input: &str) -> PasteOutcome {
    let tokens = match tokens_of(input) {
        Ok(tokens) => tokens,
        Err(why) => return PasteOutcome::NeedWholeString { why },
    };
    let address = address_in(&tokens);
    let phrase = phrase_in(&tokens, address.as_ref().ok().map(|(index, _)| *index));
    match (address, phrase) {
        (Ok((address_index, hot_address)), Ok((phrase_start, phrase))) => {
            let phrase = Zeroizing::new(phrase);
            if shape_is_accepted(tokens.len(), address_index, phrase_start) {
                PasteOutcome::Complete(GoshAiWalletString {
                    hot_address,
                    phrase,
                })
            } else {
                PasteOutcome::NeedWholeString {
                    why: PasteFault::Shape,
                }
            }
        }
        (Ok((_, hot_address)), Err(why)) => PasteOutcome::NeedPhrase { hot_address, why },
        (Err(why), Ok((_, phrase))) => PasteOutcome::NeedAddress {
            phrase: Zeroizing::new(phrase),
            why,
        },
        // Neither half is usable. Report the address fault, which is the one the user can act on
        // first, unless both are simply absent - then the paste was not a wallet string at all.
        (Err(address_fault), Err(phrase_fault)) => PasteOutcome::NeedWholeString {
            why: if address_fault == PasteFault::AddressMissing
                && phrase_fault == PasteFault::PhraseMissing
            {
                PasteFault::Shape
            } else {
                address_fault
            },
        },
    }
}

/// Parse the answer to the address-only re-prompt: exactly one canonical self-dApp address.
pub(crate) fn parse_address_input(input: &str) -> Result<CanonicalAddress, PasteFault> {
    let tokens = tokens_of(input)?;
    let (_, address) = address_in(&tokens)?;
    if tokens.len() != 1 {
        return Err(PasteFault::Shape);
    }
    Ok(address)
}

/// Parse the answer to the phrase-only re-prompt: exactly 12 words forming one valid phrase.
pub(crate) fn parse_phrase_input(input: &str) -> Result<Zeroizing<String>, PasteFault> {
    let tokens = tokens_of(input)?;
    let (_, phrase) = phrase_in(&tokens, None)?;
    if tokens.len() != GOSHAI_PHRASE_WORDS {
        return Err(PasteFault::Shape);
    }
    Ok(Zeroizing::new(phrase))
}

// ---------------------------------------------------------------------------------------------
// Hidden input
// ---------------------------------------------------------------------------------------------

/// One non-echoing read.

/// The seam exists so the loop below can be driven by a test without a terminal. Production's
/// implementation is [`TerminalHiddenPrompt`]; there is no implementation anywhere that echoes.
pub(crate) trait HiddenPrompt {
    /// Show `prompt` and read one line without displaying it.

    /// Implementations must not return the entered text inside an error, and must not log it.
    fn read_hidden(&mut self, prompt: &'static str) -> Result<Zeroizing<String>>;
}

/// Ask for what is missing until both halves are held, echoing nothing on the way.

/// The loop has no attempt limit and no timer: the spec asks it to wait for the user, and `Ctrl-C`
/// cancels safely because nothing has been written and no chain write has been submitted by this
/// point. It ends when both halves are held, or when the prompt itself fails - end of input in
/// production, script exhausted in a test.

/// `notices` receives one line per rejected attempt. That line is [`PasteFault`]'s `Display`, so it
/// is `'static` text by construction.
pub(crate) fn collect_wallet_string(
    prompt: &mut dyn HiddenPrompt,
    notices: &mut dyn std::io::Write,
) -> Result<GoshAiWalletString> {
    let mut held_address: Option<CanonicalAddress> = None;
    let mut held_phrase: Option<Zeroizing<String>> = None;
    loop {
        match (held_address.take(), held_phrase.take()) {
            (Some(hot_address), Some(phrase)) => {
                return Ok(GoshAiWalletString {
                    hot_address,
                    phrase,
                })
            }
            // One half is held. Ask only for the other, and never show the one already held.
            (Some(hot_address), None) => {
                held_address = Some(hot_address);
                let entered = prompt.read_hidden(PROMPT_PHRASE_ONLY)?;
                match parse_phrase_input(&entered) {
                    Ok(phrase) => held_phrase = Some(phrase),
                    Err(why) => writeln!(notices, "{why}")?,
                }
            }
            (None, Some(phrase)) => {
                held_phrase = Some(phrase);
                let entered = prompt.read_hidden(PROMPT_ADDRESS_ONLY)?;
                match parse_address_input(&entered) {
                    Ok(address) => held_address = Some(address),
                    Err(why) => writeln!(notices, "{why}")?,
                }
            }
            (None, None) => {
                let entered = prompt.read_hidden(PROMPT_WHOLE_STRING)?;
                match parse_wallet_string(&entered) {
                    PasteOutcome::Complete(wallet) => return Ok(wallet),
                    PasteOutcome::NeedAddress { phrase, why } => {
                        held_phrase = Some(phrase);
                        writeln!(notices, "{why}")?;
                    }
                    PasteOutcome::NeedPhrase { hot_address, why } => {
                        held_address = Some(hot_address);
                        writeln!(notices, "{why}")?;
                    }
                    PasteOutcome::NeedWholeString { why } => writeln!(notices, "{why}")?,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Waiting for an asynchronous deploy
// ---------------------------------------------------------------------------------------------

/// One account read while waiting for the Hot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HotPoll {
    /// The account was not found at all.
    Absent,
    /// The account exists and reported this `acc_type`.
    Status(String),
    /// The read itself failed after the shared transient-read retry gave up.
    ReadError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PollVerdict {
    /// Poll again until the timeout expires. Carries why, for the operator.
    KeepWaiting(&'static str),
    /// The Hot is `Active`; the refusal list may now be applied.
    Active,
}

/// Decide what one read means.

/// Deliberately total, with no failing verdict. Gosh.ai creates the sub-wallet after the user
/// copies the string, so for the length of that deploy the account legitimately reads as absent,
/// `NonExist` or `Uninit`, and a transient read error means the CLI learned nothing rather than
/// that something is wrong. Treating any of those as a failure would abort an onboarding that was
/// about to succeed. The refusals live in [`verify_active_hot`] instead, applied once the account
/// is `Active` - which is the only point at which the chain has stated a fact that waiting longer
/// cannot change.
pub(crate) fn classify_hot_poll(poll: &HotPoll) -> PollVerdict {
    match poll {
        HotPoll::Absent => PollVerdict::KeepWaiting("the Hot account does not exist yet"),
        HotPoll::ReadError => {
            PollVerdict::KeepWaiting("the chain read failed; retrying within the timeout")
        }
        HotPoll::Status(status) => match status.trim() {
            "Active" => PollVerdict::Active,
            "NonExist" | "NotFound" => {
                PollVerdict::KeepWaiting("the Hot account does not exist yet")
            }
            "Uninit" => PollVerdict::KeepWaiting("the Hot account exists but is not deployed yet"),
            _ => PollVerdict::KeepWaiting("the Hot account is not Active yet"),
        },
    }
}

// ---------------------------------------------------------------------------------------------
// The refusal list
// ---------------------------------------------------------------------------------------------

/// What an `Active` Hot says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActiveHotFacts {
    /// `code_hash` from the account.
    pub(crate) code_hash: String,
    /// `getVersion().value0` - the version string.
    pub(crate) version: String,
    /// `getVersion().value1` - the contract name.
    pub(crate) contract_name: String,
    /// `getParameters().requiredTxnConfirms`.
    pub(crate) required_txn_confirms: u8,
    /// `getParameters().requiredDataConfirms` - the threshold for changing the custodian set itself.

    /// Read as its own fact rather than assumed from `requiredTxnConfirms`: the compiled ABI
    /// declares the two separately (`getParameters` returns both as `uint8`), and they govern
    /// different powers. One says who may spend; this one says who may rewrite the list of who may
    /// spend.
    pub(crate) required_data_confirms: u8,
    /// The raw `getCustodians()` output, decoded here rather than upstream so the refusal is proved
    /// against the shape the compiled ABI actually returns.
    pub(crate) custodians: serde_json::Value,
}

/// Why an `Active` Hot was refused. Each of these is permanent: no amount of further waiting
/// changes a code hash, a version, a confirmation threshold or a custodian set, so onboarding stops
/// immediately and no binding is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HotRefusal {
    NotSelfDapp {
        dapp_id: String,
        account_id: String,
    },
    UnsupportedCodeHash {
        code_hash: String,
    },
    UnsupportedVersion {
        contract_name: String,
        version: String,
    },
    ThresholdNotOne {
        required_txn_confirms: u8,
    },
    /// The custodian set can be rewritten by fewer than two custodians.
    DataThresholdNotTwo {
        required_data_confirms: u8,
    },
    NoCustodians,
    CustodianMissing {
        derived_public_key: String,
    },
    /// The user's key is the ONLY pubkey custodian: no Gosh.ai service custodian is present, so this
    /// is not the managed sub-wallet the provider is defined to hand over.
    ServiceCustodianMissing {
        derived_public_key: String,
    },
    /// More than the two pubkey custodians the Gosh.ai Hot is specified to have.
    CustodianCountNotTwo {
        distinct_custodians: usize,
    },
    /// `getCustodians` did not return the array the compiled ABI declares.
    UnreadableCustodians,
    /// The locally derived public key is not a key.
    UnreadableDerivedKey,
}

impl std::fmt::Display for HotRefusal {
    /// Only public facts appear here: chain-visible identity, a threshold, and a public key. The
    /// recovery phrase and the derived secret never reach this type.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSelfDapp {
                dapp_id,
                account_id,
            } => write!(
                f,
                "the pasted address {dapp_id}::{account_id} is not a self-dApp account, so it is \
                 not a Hot wallet"
            ),
            Self::UnsupportedCodeHash { code_hash } => write!(
                f,
                "the Hot has code hash {code_hash}; dexdo supports only the canonical {} {} \
                 (code hash {})",
                dexdo_core::canonical_multisig::CONTRACT_NAME,
                dexdo_core::canonical_multisig::VERSION,
                dexdo_core::canonical_multisig::CODE_HASH
            ),
            Self::UnsupportedVersion {
                contract_name,
                version,
            } => write!(
                f,
                "the Hot reports {contract_name} {version}; dexdo supports only {} {}",
                dexdo_core::canonical_multisig::CONTRACT_NAME,
                dexdo_core::canonical_multisig::VERSION
            ),
            Self::ThresholdNotOne {
                required_txn_confirms,
            } => write!(
                f,
                "the Hot requires {required_txn_confirms} transaction confirmations; a Hot must \
                 have reqConfirms=1. A wallet needing more than one signature is a Vault, and \
                 dexdo cannot spend from it unattended"
            ),
            Self::DataThresholdNotTwo {
                required_data_confirms,
            } => write!(
                f,
                "the Hot requires {required_data_confirms} data confirmations; a Gosh.ai Hot must \
                 have requiredDataConfirms=2. Below two, one custodian alone can rewrite the \
                 custodian set -- which is the power to hand this wallet to somebody else, and it \
                 cannot be undone from here"
            ),
            Self::NoCustodians => write!(f, "the Hot has no pubkey custodians"),
            Self::CustodianMissing { derived_public_key } => write!(
                f,
                "the public key 0x{derived_public_key} derived from the recovery phrase is not a \
                 custodian of the Hot; the pasted address and phrase do not belong together"
            ),
            Self::ServiceCustodianMissing { derived_public_key } => write!(
                f,
                "the only pubkey custodian of the Hot is 0x{derived_public_key}, the key derived \
                 from the recovery phrase; a Gosh.ai Hot is a managed sub-wallet and must also \
                 carry the Gosh.ai service custodian, whose key is what its address is derived \
                 from. A wallet that has only your key is not the one this provider hands over"
            ),
            Self::CustodianCountNotTwo {
                distinct_custodians,
            } => write!(
                f,
                "the Hot has {distinct_custodians} distinct pubkey custodians; a Gosh.ai Hot must \
                 have exactly two -- the Gosh.ai service key and yours. At reqConfirms=1 every \
                 custodian can spend this wallet alone, so an unexpected third one is somebody \
                 else who can empty it"
            ),
            Self::UnreadableCustodians => write!(
                f,
                "the Hot returned no custodians array (ABI/getter output mismatch)"
            ),
            Self::UnreadableDerivedKey => {
                write!(
                    f,
                    "the key derived from the recovery phrase is not a public key"
                )
            }
        }
    }
}

/// Normalize a 256-bit public key to bare, zero-padded, lowercase hex.
fn normalize_public_key(value: &str) -> Option<String> {
    let value = value.trim();
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if value.is_empty() || value.len() > 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("{value:0>64}").to_ascii_lowercase())
}

fn normalize_code_hash(value: &str) -> Option<String> {
    let value = value.trim();
    let value = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

/// Decode `getCustodians()` into normalized pubkeys, against the shape the compiled ABI declares:
/// `custodians` is a `tuple[]` whose `owner_pubkey` is an `optional(uint256)`, so an address-only
/// custodian is absent from this list rather than being an error.
fn custodian_public_keys(custodians: &serde_json::Value) -> Option<Vec<String>> {
    let entries = custodians.get("custodians")?.as_array()?;
    Some(
        entries
            .iter()
            .filter_map(|custodian| custodian.get("owner_pubkey"))
            .filter_map(serde_json::Value::as_str)
            .filter_map(normalize_public_key)
            .collect(),
    )
}

/// The refusal list of the wallet-team spec, "Onboarding via Gosh.ai" step 9.

/// The Gosh.ai Hot must be the same contract the Acki Nacki Wallet hands over, so the identity is
/// pinned to exactly the constants `wallet_onboarding.rs` pins the Acki Nacki Hot to - the canonical
/// `UpdateCustodianMultisigWallet_v2` 2.4.0 - rather than to a second, drifting copy of them. Two
/// checks are applied to the same fact on purpose: `code_hash` is what the account *is*, while
/// `getVersion` is what the contract *says*, and a mismatch between them is itself a finding.

/// The custodian check is the one that ties the two halves of the pasted string together. Nothing
/// else can: a recovery phrase does not determine the Gosh.ai Hot's address, because the address is
/// derived from a separate service custodian key. Only the chain can confirm that the phrase the
/// user pasted controls the address the user pasted.

/// # The exact shape, and why membership alone is not enough

/// The owner froze the Gosh.ai Hot as: exactly two distinct pubkey custodians -- the Gosh.ai service
/// key and the user's -- with `requiredTxnConfirms=1` and `requiredDataConfirms=2`. All four facts
/// are REFUSED when they differ, never warned about, because each one names somebody who can take
/// the money:

/// - a third custodian is a third party who, at `reqConfirms=1`, can empty the Hot alone;
/// - a sole custodian means this is not the managed sub-wallet the provider is defined to hand over,
/// so whatever the operator is about to fund is not what they think they are funding;
/// - `requiredDataConfirms` below two lets one custodian rewrite the custodian set by itself, which
/// is the power to give the wallet away and is not reversible from this side.

/// The service key being able to spend on its own is INTENDED and is accepted: that is what a
/// managed sub-wallet is. What is refused is a set that is not exactly those two.

/// Only pubkey custodians are counted, which is what the specification fixes and what the compiled
/// ABI makes readable: `owner_pubkey` is an `optional(uint256)`, so an address-only custodian is
/// absent from this list entirely rather than appearing as some other value.
pub(crate) fn verify_active_hot(
    hot_address: &CanonicalAddress,
    facts: &ActiveHotFacts,
    derived_public_key: &str,
) -> Result<(), HotRefusal> {
    if !is_self_dapp(hot_address) {
        return Err(HotRefusal::NotSelfDapp {
            dapp_id: hot_address.dapp_id().to_string(),
            account_id: hot_address.account_id().to_string(),
        });
    }
    let code_hash =
        normalize_code_hash(&facts.code_hash).ok_or_else(|| HotRefusal::UnsupportedCodeHash {
            code_hash: facts.code_hash.trim().to_string(),
        })?;
    if code_hash != dexdo_core::canonical_multisig::CODE_HASH {
        return Err(HotRefusal::UnsupportedCodeHash { code_hash });
    }
    if facts.version.trim() != dexdo_core::canonical_multisig::VERSION
        || facts.contract_name.trim() != dexdo_core::canonical_multisig::CONTRACT_NAME
    {
        return Err(HotRefusal::UnsupportedVersion {
            contract_name: facts.contract_name.trim().to_string(),
            version: facts.version.trim().to_string(),
        });
    }
    if facts.required_txn_confirms != 1 {
        return Err(HotRefusal::ThresholdNotOne {
            required_txn_confirms: facts.required_txn_confirms,
        });
    }
    if facts.required_data_confirms != 2 {
        return Err(HotRefusal::DataThresholdNotTwo {
            required_data_confirms: facts.required_data_confirms,
        });
    }
    let custodians =
        custodian_public_keys(&facts.custodians).ok_or(HotRefusal::UnreadableCustodians)?;
    if custodians.is_empty() {
        return Err(HotRefusal::NoCustodians);
    }
    let derived =
        normalize_public_key(derived_public_key).ok_or(HotRefusal::UnreadableDerivedKey)?;
    if !custodians.contains(&derived) {
        return Err(HotRefusal::CustodianMissing {
            derived_public_key: derived,
        });
    }
    // DISTINCT, not len(): the same key listed twice is one custodian with one signature, so a
    // duplicate would otherwise satisfy "exactly two" while leaving the user's key alone on the
    // wallet -- the very shape `ServiceCustodianMissing` exists to refuse.
    let mut distinct = custodians;
    distinct.sort_unstable();
    distinct.dedup();
    match distinct.len() {
        // `derived` is known to be present, so a single distinct custodian IS the user's key.
        1 => {
            return Err(HotRefusal::ServiceCustodianMissing {
                derived_public_key: derived,
            })
        }
        2 => {}
        distinct_custodians => {
            return Err(HotRefusal::CustodianCountNotTwo {
                distinct_custodians,
            })
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Owner-only files
// ---------------------------------------------------------------------------------------------

/// The resumable onboarding draft, as a TYPE rather than a hand-spelled JSON literal.

/// This file is what the resume path reads after a timeout or a `Ctrl-C`, so it names the same
/// provider and the same network as the binding that will eventually be committed. It used to be
/// built with `serde_json::json!` and a `&str` provider constant, which is the shape that drifts: a
/// literal does not follow a type change, so the binding could become typed while the file the
/// resume path reads stayed spelled by hand, and the first person to touch either would separate
/// them silently. Sharing [`WalletProvider`] and [`WalletNetwork`] with the binding makes that
/// impossible - one change, both files - and
/// `a_draft_written_by_draft_of_round_trips_through_the_typed_reader` is the assertion that keeps it
/// true.

/// It holds no secret: `hot_seed_file` is the PATH of the file that does.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct GoshAiOnboardingDraft {
    pub(crate) version: u32,
    pub(crate) id: String,
    pub(crate) provider: WalletProvider,
    pub(crate) network: WalletNetwork,
    pub(crate) phase: String,
    pub(crate) hot_address: String,
    pub(crate) hot_seed_file: std::path::PathBuf,
}

/// The one phase this draft can be in: everything before it is in memory, and everything after it
/// has produced a binding.
pub(crate) const GOSHAI_DRAFT_PHASE_AWAITING_HOT: &str = "awaiting_hot_activation";

pub(crate) mod files {
    use super::{GoshAiOnboardingDraft, GoshAiWalletString, GOSHAI_DRAFT_PHASE_AWAITING_HOT};
    use crate::cli::wallet::{WalletBinding, WalletNetwork, WalletProvider, BINDING_VERSION};
    use anyhow::{Context as _, Result};
    use std::path::{Path, PathBuf};

    /// Every path this flow writes, derived from one effective data directory and one binding id.

    /// The binding id is generated before any key material exists, so re-onboarding the same
    /// provider lands in a fresh directory and cannot overwrite the secret files of a binding whose
    /// Hot may still hold funds.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct GoshAiPaths {
        pub(crate) wallet_root: PathBuf,
        pub(crate) binding_dir: PathBuf,
        pub(crate) seed_file: PathBuf,
        pub(crate) draft_file: PathBuf,
        pub(crate) active_binding_file: PathBuf,
    }

    /// The draft's file name, in ONE place: it is written by [`persist_secret_and_draft`], looked
    /// for by [`find_resumable`] and removed by [`discard_draft`], and a second spelling would let
    /// the resume scan quietly stop finding what the writer produces.
    pub(crate) const DRAFT_FILE_NAME: &str = "onboarding.json";

    impl GoshAiPaths {
        /// `network` names the active binding file, because bindings are kept per network (,
        /// audit item 3). It is asked of the store rather than spelled here: a second place that
        /// knows the layout is a second place that can disagree with it, and the test that pins
        /// these two together is what would otherwise stop meaning anything.
        pub(crate) fn new(data_dir: &Path, network: WalletNetwork, binding_id: &str) -> Self {
            let wallet_root = data_dir.join("wallet");
            let binding_dir = wallet_root.join("bindings").join(binding_id);
            Self {
                seed_file: binding_dir.join("hot.seed"),
                draft_file: binding_dir.join(DRAFT_FILE_NAME),
                active_binding_file: crate::cli::wallet::WalletStore::at(&wallet_root)
                    .binding_path(&network),
                binding_dir,
                wallet_root,
            }
        }
    }

    /// A directory that only its owner may traverse, so the 0600 files inside cannot be reached by
    /// another local account even by name.
    fn create_owner_only_dir(path: &Path) -> Result<()> {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder
            .create(path)
            .with_context(|| format!("create wallet directory {}", path.display()))
    }

    /// Persist the recovery phrase and a resumable draft, both owner-only, both atomically.

    /// This is the last thing that happens before the first network wait and the first thing that
    /// must survive it: a timeout or `Ctrl-C` while waiting for the Hot leaves these two files in
    /// place, so re-running the command resumes without asking for the phrase again.

    /// The writer is `note::write_private_atomic` - the one this repository already uses for note
    /// owner keys and pools. It creates an exclusive 0600 temp in the destination directory and
    /// renames over the target, which a plain write does not: a plain write inherits the umask, and
    /// a predictable temp path can be pre-created as a symlink by another local account.
    pub(crate) fn persist_secret_and_draft(
        paths: &GoshAiPaths,
        network: WalletNetwork,
        wallet: &GoshAiWalletString,
    ) -> Result<()> {
        create_owner_only_dir(&paths.binding_dir)?;
        crate::cli::note::write_private_atomic(&paths.seed_file, wallet.phrase.as_bytes())
            .with_context(|| {
                format!(
                    "persist the Gosh.ai recovery phrase to {}",
                    paths.seed_file.display()
                )
            })?;
        let draft = draft_of(paths, network, wallet);
        let bytes =
            serde_json::to_vec_pretty(&draft).context("serialize the Gosh.ai onboarding draft")?;
        crate::cli::note::write_private_atomic(&paths.draft_file, &bytes).with_context(|| {
            format!(
                "persist the Gosh.ai onboarding draft to {}",
                paths.draft_file.display()
            )
        })
    }

    /// The resumable draft. It holds no secret of its own - only the path of the file that does.

    /// Built from the same typed `provider`/`network` values [`active_binding`] puts in the binding,
    /// so the two cannot describe different things: there is no second spelling to keep in step.
    pub(crate) fn draft_of(
        paths: &GoshAiPaths,
        network: WalletNetwork,
        wallet: &GoshAiWalletString,
    ) -> GoshAiOnboardingDraft {
        GoshAiOnboardingDraft {
            version: BINDING_VERSION,
            id: binding_id_of(paths),
            provider: WalletProvider::GoshAi,
            network,
            phase: GOSHAI_DRAFT_PHASE_AWAITING_HOT.to_string(),
            hot_address: wallet.hot_address.to_string(),
            hot_seed_file: paths.seed_file.clone(),
        }
    }

    /// The draft this binding id already has, if it has one and it is readable.

    /// A draft that does not parse is `None` and not an error: it can only make this attempt fall
    /// back to asking for the wallet string, which is exactly what happens today, whereas failing
    /// would put a corrupt file between the operator and their own onboarding.
    pub(crate) fn read_draft(paths: &GoshAiPaths) -> Option<GoshAiOnboardingDraft> {
        let bytes = std::fs::read(&paths.draft_file).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Is this draft one THIS command may continue?

    /// Every field is checked rather than trusted, because the answer decides which Hot the
    /// operator ends up bound to. The network in particular: a draft written for one chain must
    /// never be resumed by a command configured for the other, or the phrase would be proved
    /// against an address on a chain nobody asked about.
    pub(crate) fn is_resumable_draft(
        draft: &GoshAiOnboardingDraft,
        network: &WalletNetwork,
        dir_name: &str,
    ) -> bool {
        draft.version == BINDING_VERSION
            && draft.provider == WalletProvider::GoshAi
            && &draft.network == network
            && draft.phase == GOSHAI_DRAFT_PHASE_AWAITING_HOT
            // The draft names the directory it lives in. A mismatch means the two disagree about
            // whose secrets these are, and the resume would carry an id the commit then refuses.
            && draft.id == dir_name
            && draft.hot_seed_file.is_file()
    }

    /// The binding id of an onboarding attempt that can be continued, if one is on disk.

    /// Scans the per-binding directories rather than keeping a pointer file: the directories ARE
    /// the record, so there is no second thing to keep in step and nothing to leave stale when an
    /// attempt is committed or removed.

    /// The most recently written draft wins when there is more than one, because that is the
    /// attempt the operator was last making; the caller prints the Hot address it resumed, so the
    /// choice is never silent.
    pub(crate) fn find_resumable(wallet_root: &Path, network: &WalletNetwork) -> Option<String> {
        let mut best: Option<(std::time::SystemTime, String)> = None;
        for entry in std::fs::read_dir(wallet_root.join("bindings"))
            .ok()?
            .flatten()
        {
            let Some(dir_name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let draft_file = entry.path().join(DRAFT_FILE_NAME);
            let Ok(bytes) = std::fs::read(&draft_file) else {
                continue;
            };
            let Ok(draft) = serde_json::from_slice::<GoshAiOnboardingDraft>(&bytes) else {
                continue;
            };
            if !is_resumable_draft(&draft, network, &dir_name) {
                continue;
            }
            let written = std::fs::metadata(&draft_file)
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            if best.as_ref().is_none_or(|(best, _)| written >= *best) {
                best = Some((written, dir_name));
            }
        }
        best.map(|(_, id)| id)
    }

    /// Retire the draft once its binding is committed, so nothing can resume a finished attempt.

    /// Only the draft goes: `hot.seed` is the committed binding's own secret and stays exactly
    /// where `binding.json` points at it. Removing the draft is what makes "resumable" mean
    /// "unfinished" instead of "was ever started" -- without it a later attempt would adopt the id
    /// of the ACTIVE binding and write over the wallet the operator is already using.

    /// Takes the directory rather than [`GoshAiPaths`] because the caller that has to do this is
    /// the one holding the store's reserved `BindingDraft`, which knows its directory and not this
    /// provider's file layout.

    /// Best effort: the binding is already committed, and failing to tidy up must not turn a
    /// successful onboarding into an error.
    pub(crate) fn discard_draft_in(binding_dir: &Path) {
        let _ = std::fs::remove_file(binding_dir.join(DRAFT_FILE_NAME));
    }

    fn binding_id_of(paths: &GoshAiPaths) -> String {
        paths
            .binding_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string()
    }

    /// The verified binding, BUILT and not written. Called only after [`super::verify_active_hot`].

    /// This flow deliberately no longer writes `wallet/binding.json` itself. `WalletStore` is the
    /// single writer, and it archives whatever it replaces before the atomic swap; a second writer
    /// that renamed over the file would destroy the only local record of a Hot that can still hold
    /// funds, leaving the operator with a balance on chain and no address to reach it. So the flow
    /// returns the binding and `run_selected` commits it through the store, once.

    /// No `vault_address`: Gosh.ai provides no Vault, and a field naming one would be a lie a later
    /// funding flow could act on. `hot_seed_file` is a path; the secret is referenced, never copied.
    pub(crate) fn active_binding(
        paths: &GoshAiPaths,
        network: WalletNetwork,
        hot_address: &dexdo_core::CanonicalAddress,
    ) -> WalletBinding {
        WalletBinding {
            version: BINDING_VERSION,
            id: binding_id_of(paths),
            provider: WalletProvider::GoshAi,
            network,
            hot_address: hot_address.to_string(),
            vault_address: None,
            hot_key_file: None,
            vault_key_file: None,
            hot_seed_file: Some(paths.seed_file.clone()),
            push_profile_address: None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The pre-network stage, in one place
// ---------------------------------------------------------------------------------------------

/// What the pre-network stage produced.
pub(crate) struct PreparedOnboarding {
    pub(crate) paths: files::GoshAiPaths,
    pub(crate) hot_address: CanonicalAddress,
    /// The public half of the custodian key derived from the phrase. The secret half is not carried
    /// out of this stage: the only thing the rest of the flow needs is a key to look for among the
    /// Hot's custodians, and that is public.
    pub(crate) derived_public_key: String,
}

/// Collect, validate, derive, and only then write - in that order, in one function.

/// The order is the security property, so it is one function rather than a sequence a caller is
/// trusted to keep: no path is touched until the paste has parsed, the phrase has proved to be a
/// valid phrase, and the custodian key has actually derived from it. A failure at any of those
/// three points returns with the data directory exactly as it was found.

/// `derive_public_key` is a parameter because the real derivation is TVM-SDK work that only exists
/// in a chain build, while the ordering above must be provable in the default build that CI
/// runs. Production passes [`derive_custodian_public_key`].
pub(crate) fn prepare_onboarding(
    data_dir: &std::path::Path,
    network: WalletNetwork,
    binding_id: &str,
    prompt: &mut dyn HiddenPrompt,
    notices: &mut dyn std::io::Write,
    derive_public_key: &dyn Fn(&str) -> Result<String>,
) -> Result<PreparedOnboarding> {
    let wallet = collect_wallet_string(prompt, notices)?;
    let derived_public_key = derive_public_key(&wallet.phrase)?;
    let paths = files::GoshAiPaths::new(data_dir, network.clone(), binding_id);
    files::persist_secret_and_draft(&paths, network, &wallet)?;
    Ok(PreparedOnboarding {
        paths,
        hot_address: wallet.hot_address,
        derived_public_key,
    })
}

/// Continue an onboarding attempt that already stored its phrase, WITHOUT asking for it again.

/// This is the promise the activation-timeout error makes, kept. The phrase and the draft were
/// written before the first network wait precisely so that a timeout or a `Ctrl-C` costs the
/// operator nothing; re-prompting for a 12-word recovery phrase they have already proved is not a
/// small annoyance, it is the point at which somebody reaches for the clipboard, a text file or a
/// screenshot to avoid typing it a third time.

/// It returns `None` rather than an error when there is nothing to resume, so the caller falls back
/// to a normal first attempt. The only failure it raises is a phrase that is on disk but no longer
/// derives, which is a corrupt secret and must not be passed off as "start again".

/// The address is re-parsed from the draft and the key re-derived from the stored phrase, so a
/// resumed attempt proves exactly what a fresh one proves: nothing is carried across as trusted.
pub(crate) fn resume_onboarding(
    data_dir: &std::path::Path,
    network: WalletNetwork,
    binding_id: &str,
    derive_public_key: &dyn Fn(&str) -> Result<String>,
) -> Result<Option<PreparedOnboarding>> {
    // The SAME network this resume is for: bindings are kept per network, so the paths a resumed
    // attempt carries must name the active-binding file of the chain it is being resumed on, not
    // another one. `is_resumable_draft` then refuses a draft that disagrees.
    let paths = files::GoshAiPaths::new(data_dir, network.clone(), binding_id);
    let Some(draft) = files::read_draft(&paths) else {
        return Ok(None);
    };
    if !files::is_resumable_draft(&draft, &network, binding_id) {
        return Ok(None);
    }
    // this reads the stored RECOVERY PHRASE back on resume. `--multisig-seed-file` gets the
    // permission check on its way in (`support.rs`, pinned there); the same secret, written by this
    // client and read by this client, did not. Checked before the read, so a refusal has not already
    // loaded what it refuses.
    crate::cli::support::refuse_exposed_secret_file(
        &draft.hot_seed_file,
        "the stored Gosh.ai recovery phrase",
    )?;
    let phrase = Zeroizing::new(
        std::fs::read_to_string(&draft.hot_seed_file)
            // The path is named, the contents never are.
            .map_err(|error| {
                anyhow::anyhow!(
                    "read the stored Gosh.ai recovery phrase from {}: {}",
                    draft.hot_seed_file.display(),
                    error.kind()
                )
            })?,
    );
    let hot_address = CanonicalAddress::parse(&draft.hot_address).map_err(|error| {
        anyhow::anyhow!(
            "the resumable onboarding draft at {} records the Hot address {}, which is not a \
             canonical address: {error}",
            paths.draft_file.display(),
            draft.hot_address
        )
    })?;
    let derived_public_key = derive_public_key(phrase.trim())?;
    Ok(Some(PreparedOnboarding {
        paths,
        hot_address,
        derived_public_key,
    }))
}

// ---------------------------------------------------------------------------------------------
// Production wiring
// ---------------------------------------------------------------------------------------------

/// The integration surface. `allow(unused_imports)` is the same temporary note as the
/// `allow(dead_code)` on the module declaration: these four names are what the gosh-ai provider of
/// `dexdo wallet onboard` will call, and until that dispatch exists nothing in the crate names
/// them. `dead_code` does not cover a re-export, so it is spelled out here as well.
#[allow(unused_imports)]
pub(crate) use live::{
    derive_custodian_public_key, run_wallet_onboard_goshai, GoshAiOnboardOptions,
    TerminalHiddenPrompt,
};

mod live {
    use std::io::{IsTerminal as _, Write as _};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context as _, Result};
    use dexdo_core::chain::RetryingReads as _;
    use dexdo_core::params::{GOSHAI_HOT_ACTIVATION_POLL_INTERVAL, GOSHAI_HOT_ACTIVATION_TIMEOUT};
    use dexdo_core::{Address, CanonicalAddress};
    use zeroize::Zeroizing;

    use super::{
        classify_hot_poll, files, prepare_onboarding, resume_onboarding, verify_active_hot,
        ActiveHotFacts, HiddenPrompt, HotPoll, PollVerdict,
    };
    use crate::cli::wallet::{WalletBinding, WalletNetwork};

    /// What the provider subcommand supplies. The clap surface of the gosh-ai provider is owned by
    /// the wallet-binding work; this is the shape it hands over, so that command stays a thin
    /// translation.
    pub(crate) struct GoshAiOnboardOptions {
        pub(crate) network: WalletNetwork,
        /// The binding id the STORE reserved for this attempt. It is not generated here: the store
        /// creates the owner-only directory under it and discards it if the attempt writes nothing,
        /// so a second id would leave that directory empty and put the secrets somewhere the store
        /// does not know about.
        pub(crate) binding_id: String,
        /// `--activation-timeout`, defaulting to
        /// [`GoshAiOnboardOptions::DEFAULT_ACTIVATION_TIMEOUT`].
        pub(crate) activation_timeout: Duration,
        pub(crate) data_dir: PathBuf,
    }

    impl GoshAiOnboardOptions {
        /// What `--activation-timeout` overrides, so the clap surface takes the figure from
        /// `params.rs` rather than restating ten minutes as a literal of its own.
        pub(crate) const DEFAULT_ACTIVATION_TIMEOUT: Duration = GOSHAI_HOT_ACTIVATION_TIMEOUT;
    }

    /// The production hidden input.

    /// The prompt goes to stderr, so machine-readable stdout is never polluted by it; the answer is
    /// read with terminal echo disabled and lands in a `Zeroizing<String>` that is wiped on drop.
    /// The pasted string therefore never reaches the terminal, and - because this flow takes no
    /// `--address`, `--seed` or `--phrase` flag at all - it can never reach the process arguments or
    /// the shell history either.
    pub(crate) struct TerminalHiddenPrompt;

    impl HiddenPrompt for TerminalHiddenPrompt {
        fn read_hidden(&mut self, prompt: &'static str) -> Result<Zeroizing<String>> {
            eprint!("{prompt}");
            std::io::stderr().flush().context("flush hidden prompt")?;
            let entered = rpassword::read_password()
                // The io error is mapped by hand: `read_password` is given a secret, and letting an
                // arbitrary error type through would be the one place that could carry it onwards.
                .map_err(|error| {
                    anyhow::anyhow!("read the hidden input: {}", error.kind_description())
                })?;
            // The newline the terminal did not echo.
            eprintln!();
            Ok(Zeroizing::new(entered))
        }
    }

    /// `std::io::ErrorKind` has no stable string form, and the error's own `Display` may quote the
    /// operation. Naming the kind keeps the message useful and provably input-free.
    trait ErrorKindDescription {
        fn kind_description(&self) -> &'static str;
    }

    impl ErrorKindDescription for std::io::Error {
        fn kind_description(&self) -> &'static str {
            match self.kind() {
                std::io::ErrorKind::UnexpectedEof => "input ended",
                std::io::ErrorKind::Interrupted => "input was interrupted",
                std::io::ErrorKind::InvalidData => "input was not valid text",
                _ => "the terminal could not be read",
            }
        }
    }

    /// Derive the user's custodian public key from the phrase, locally.

    /// The secret half is dropped here: `DerivedMultisigKey` zeroizes it, and nothing past this
    /// point needs it. Onboarding only has to prove that the phrase controls the pasted Hot, and the
    /// public key is enough to ask the chain that question.
    pub(crate) fn derive_custodian_public_key(phrase: &str) -> Result<String> {
        let derived = dexdo::wallet_seed::derive_multisig_private_key_from_seed_phrase(phrase)
            // The SDK error is replaced, not wrapped: its payload is derived from the phrase.
            .map_err(|error| anyhow::anyhow!("derive the custodian key: {error}"))?;
        Ok(derived.public_hex().to_string())
    }

    async fn poll_hot(client: &dexdo_core::ChainClient, address: &Address) -> HotPoll {
        match client.get_account_retrying(address).await {
            Ok(Some(account)) => HotPoll::Status(account.status),
            Ok(None) => HotPoll::Absent,
            Err(_) => HotPoll::ReadError,
        }
    }

    /// Wait for the Hot to become `Active`, or run out of time having written no binding.
    async fn await_active_hot(
        client: &dexdo_core::ChainClient,
        address: &Address,
        timeout: Duration,
    ) -> Result<()> {
        let started = Instant::now();
        let mut last_reason = "the Hot account does not exist yet";
        loop {
            match classify_hot_poll(&poll_hot(client, address).await) {
                PollVerdict::Active => return Ok(()),
                PollVerdict::KeepWaiting(reason) => {
                    if reason != last_reason {
                        println!("waiting for the Gosh.ai Hot: {reason}");
                        last_reason = reason;
                    }
                }
            }
            if started.elapsed() >= timeout {
                bail!(
                    "the Gosh.ai Hot did not become Active within {}s ({last_reason}). Nothing was \
                     spent and no binding was saved; the recovery phrase and the resumable draft \
                     are already stored, so running `dexdo wallet onboard` again for the gosh-ai \
                     provider continues waiting without asking for the phrase again",
                    timeout.as_secs()
                );
            }
            tokio::time::sleep(GOSHAI_HOT_ACTIVATION_POLL_INTERVAL).await;
        }
    }

    async fn read_active_hot_facts(
        client: &dexdo_core::ChainClient,
        address: &Address,
    ) -> Result<ActiveHotFacts> {
        let display = address.with_workchain();
        let account = client
            .get_account_retrying(address)
            .await
            .with_context(|| format!("read the Gosh.ai Hot {display}"))?
            .ok_or_else(|| anyhow::anyhow!("the Gosh.ai Hot {display} disappeared after Active"))?;
        let code_hash = account
            .code_hash
            .ok_or_else(|| anyhow::anyhow!("the Gosh.ai Hot {display} has no code hash"))?;
        let abi = dexdo_core::canonical_multisig::MULTISIG_ABI_JSON;
        let version = client
            .run_getter_retrying(address, abi, "getVersion", serde_json::json!({}))
            .await
            .with_context(|| format!("read getVersion of the Gosh.ai Hot {display}"))?
            .ok_or_else(|| anyhow::anyhow!("the Gosh.ai Hot {display} returned no getVersion"))?;
        let parameters = client
            .run_getter_retrying(address, abi, "getParameters", serde_json::json!({}))
            .await
            .with_context(|| format!("read getParameters of the Gosh.ai Hot {display}"))?
            .ok_or_else(|| {
                anyhow::anyhow!("the Gosh.ai Hot {display} returned no getParameters")
            })?;
        let custodians = client
            .run_getter_retrying(address, abi, "getCustodians", serde_json::json!({}))
            .await
            .with_context(|| format!("read getCustodians of the Gosh.ai Hot {display}"))?
            .ok_or_else(|| {
                anyhow::anyhow!("the Gosh.ai Hot {display} returned no getCustodians")
            })?;
        Ok(ActiveHotFacts {
            code_hash,
            version: string_field(&version, "value0"),
            contract_name: string_field(&version, "value1"),
            required_txn_confirms: u8_field(&parameters, "requiredTxnConfirms", &display)?,
            required_data_confirms: u8_field(&parameters, "requiredDataConfirms", &display)?,
            custodians,
        })
    }

    /// One `getParameters()` `uint8`, by the name the compiled ABI declares.

    /// A missing field is an ERROR and never a zero: both thresholds this reads are refused unless
    /// they hold an exact value, so silently defaulting would turn "the getter did not answer" into
    /// a policy verdict about a wallet that holds money.
    fn u8_field(parameters: &serde_json::Value, field: &str, display: &str) -> Result<u8> {
        parameters
            .get(field)
            .and_then(|value| {
                value
                    .as_u64()
                    .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
            })
            .and_then(|value| u8::try_from(value).ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "the Gosh.ai Hot {display} returned no usable {field} (ABI/getter output \
                     mismatch)"
                )
            })
    }

    fn string_field(value: &serde_json::Value, field: &str) -> String {
        value
            .get(field)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// The handler behind `dexdo wallet onboard` when the chosen provider is gosh-ai.
    pub(crate) async fn run_wallet_onboard_goshai(
        options: GoshAiOnboardOptions,
    ) -> Result<WalletBinding> {
        // Before anything is read, and therefore before there is anything to leak: this flow takes
        // its secret only through hidden inputs, so a pipe or a script has no way to supply it that
        // would not also expose it.
        if !std::io::stdin().is_terminal() {
            bail!(
                "the gosh-ai provider of `dexdo wallet onboard` needs an interactive terminal: the \
                 Gosh.ai wallet string contains a recovery phrase and is accepted only through a \
                 hidden prompt. There are deliberately no --address, --seed or --phrase flags, \
                 because process arguments are visible to every local process and land in the \
                 shell history"
            );
        }
        let binding_id = options.binding_id.clone();
        // WHERE TO DIAL IS RESOLVED BEFORE ANYTHING IS ASKED OR WRITTEN.

        // Proving the Hot is three read-only account queries on the selected network, and the
        // manifest names none of the accounts they read: the address arrives in the Gosh.ai string.
        // What the manifest does name is where to dial, which is the whole input here.

        // Read through `manifest_path()`, the same seam `wallet onboard manual` uses. It used to
        // come from `options.contracts`, and the one site building these options had nothing to put
        // there since `--contracts` was removed: it passed `None` unconditionally, so this
        // call took the no-endpoint branch on EVERY run. The path did not work for some manifests --
        // it did not work at all, and said the manifest was at fault.

        // It stood BELOW the block that follows, which is why that mattered so much: the refusal
        // arrived after the recovery phrase and the resumable draft were already owner-only on
        // disk, so an operator whose setup was fine was told their manifest was broken at the one
        // moment they had most reason to believe otherwise. Resolved here, a manifest that cannot
        // answer stops the flow before it asks for a secret it has nowhere to send.
        let endpoint = crate::cli::wallet::wallet_read_endpoint(
            Some(&crate::cli::commands::manifest_path()?),
            options.network.clone(),
        )?;
        // The resume comes FIRST, before the invitation is printed and before anything is asked.
        // `run_selected` hands this flow the id of a resumable attempt when one exists, so finding
        // a draft here means the operator is continuing the wait they already started -- they have
        // the wallet open in front of them and need neither the QR nor the prompt again.
        let prepared = match resume_onboarding(
            &options.data_dir,
            options.network.clone(),
            &binding_id,
            &derive_custodian_public_key,
        )? {
            Some(resumed) => {
                println!(
                    "resuming the Gosh.ai onboarding already stored for Hot {} -- the recovery \
                     phrase is read from {} and is not asked for again",
                    resumed.hot_address,
                    resumed.paths.seed_file.display()
                );
                resumed
            }
            None => {
                // Whether this deployment has Gosh.ai onboarding at all, resolved HERE and not
                // above the resume fork. Above, it refused a RESUMING operator with "nothing was
                // asked for and nothing was written" while their recovery phrase was already
                // owner-only on disk from the first attempt -- a false sentence at the worst
                // moment. The resume path needs no invitation: it is continuing a wait, not
                // starting one.

                // For a fresh attempt this is still before anything is printed, encoded, prompted
                // or written, which is the ordering asks for: the line below is the first
                // thing that reaches the screen.
                let invitation_url = {
                    let manifest =
                        dexdo_core::Deployed::load(&crate::cli::commands::manifest_path()?)?;
                    super::goshai_invitation_url(&manifest)?.to_string()
                };
                super::render_goshai_invitation(&mut std::io::stdout(), &invitation_url)?;
                let mut prompt = TerminalHiddenPrompt;
                let mut notices = std::io::stderr();
                let prepared = prepare_onboarding(
                    &options.data_dir,
                    options.network.clone(),
                    &binding_id,
                    &mut prompt,
                    &mut notices,
                    &derive_custodian_public_key,
                )?;
                println!(
                    "recovery phrase stored owner-only at {}",
                    prepared.paths.seed_file.display()
                );
                println!(
                    "resumable onboarding draft at {}",
                    prepared.paths.draft_file.display()
                );
                prepared
            }
        };

        let chain = dexdo_core::ChainClient::connect(&endpoint).map_err(|error| {
            anyhow::anyhow!("connect verification endpoint {endpoint}: {error}")
        })?;
        let address = chain_address(&prepared.hot_address)?;
        await_active_hot(&chain, &address, options.activation_timeout).await?;

        let facts = read_active_hot_facts(&chain, &address).await?;
        if let Err(refusal) =
            verify_active_hot(&prepared.hot_address, &facts, &prepared.derived_public_key)
        {
            bail!(
                "the account at {} is Active but is not a supported Hot: {refusal}. No wallet \
                 binding was saved and nothing was spent. Waiting longer cannot change any of \
                 these facts",
                prepared.hot_address
            );
        }

        // Built, not written. `run_selected` commits it through `WalletStore`, which archives any
        // binding it replaces -- see `files::active_binding`.
        Ok(files::active_binding(
            &prepared.paths,
            options.network.clone(),
            &prepared.hot_address,
        ))
    }

    fn chain_address(address: &CanonicalAddress) -> Result<Address> {
        Address::parse(&address.legacy())
            .with_context(|| format!("the Gosh.ai Hot address {address} is not a chain address"))
    }
}

/// follow-up items 5/6: the resume the timeout message promises, and the exact Hot policy.
#[cfg(test)]
mod resume_and_hot_policy_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    // A self-dApp address: the two 64-hex halves are equal, which is what makes it a wallet rather
    // than a contract inside somebody's DApp.
    const HOT_ACCOUNT: &str = "5cbb90f8a1d3e4f2c6b7a8091d2e3f4a5b6c7d8e9f0a1b2c3d4e5f60718293a4";
    // Two 64-hex halves that differ: canonical, but not a self-dApp account.
    const NOT_SELF_DAPP: &str =
        "0000000000000000000000000000000000000000000000000000000000000004::\
         5cbb90f8a1d3e4f2c6b7a8091d2e3f4a5b6c7d8e9f0a1b2c3d4e5f60718293a4";

    fn hot_address() -> String {
        format!("{HOT_ACCOUNT}::{HOT_ACCOUNT}")
    }

    /// A valid 12-word English phrase, taken from the wordlist by index so no fixture drifts.
    fn phrase_a() -> String {
        mnemonic_from_entropy(&[0x11; 16])
    }

    fn phrase_b() -> String {
        mnemonic_from_entropy(&[0x22; 16])
    }

    fn mnemonic_from_entropy(entropy: &[u8]) -> String {
        bip39::Mnemonic::from_entropy(entropy, bip39::Language::English)
            .expect("12-word fixture phrase")
            .phrase()
            .to_string()
    }

    /// A scripted stand-in for the terminal. It records the prompts it was shown so a test can
    /// assert which half was asked for, and it never echoes.
    struct ScriptedPrompt {
        answers: VecDeque<String>,
        prompts: Vec<&'static str>,
    }

    impl ScriptedPrompt {
        fn new<I: IntoIterator<Item = String>>(answers: I) -> Self {
            Self {
                answers: answers.into_iter().collect(),
                prompts: Vec::new(),
            }
        }
    }

    impl HiddenPrompt for ScriptedPrompt {
        fn read_hidden(&mut self, prompt: &'static str) -> Result<Zeroizing<String>> {
            self.prompts.push(prompt);
            match self.answers.pop_front() {
                Some(answer) => Ok(Zeroizing::new(answer)),
                // Exhaustion is how a test ends the loop, and it is also what production sees at
                // end of input. Note that it carries nothing the caller typed.
                None => anyhow::bail!("input ended"),
            }
        }
    }

    fn custodians_with(pubkeys: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "custodians": pubkeys
                .iter()
                .enumerate()
                .map(|(index, key)| serde_json::json!({
                    "owner_pubkey": key,
                    "owner_address": serde_json::Value::Null,
                    "index": index,
                }))
                .collect::<Vec<_>>(),
        })
    }

    const OUR_KEY: &str = "aa11bb22cc33dd44ee55ff66007788990011223344556677889900aabbccddee";
    const OTHER_KEY: &str = "1234567890abcdef1234567890abcdef1234567890abcdef1234567890abcdef";

    fn good_facts() -> ActiveHotFacts {
        ActiveHotFacts {
            code_hash: dexdo_core::canonical_multisig::CODE_HASH.to_string(),
            version: dexdo_core::canonical_multisig::VERSION.to_string(),
            contract_name: dexdo_core::canonical_multisig::CONTRACT_NAME.to_string(),
            required_txn_confirms: 1,
            required_data_confirms: 2,
            custodians: custodians_with(&[OTHER_KEY, OUR_KEY]),
        }
    }

    // -----------------------------------------------------------------------------------------
    // The two parts come out of one string, in either order
    // -----------------------------------------------------------------------------------------

    #[test]
    fn both_orders_of_address_and_phrase_yield_the_same_pair() {
        let address = hot_address();
        let phrase = phrase_a();
        let address_first = format!("{address} {phrase}");
        let phrase_first = format!("{phrase} {address}");

        for (label, input) in [
            ("address first", address_first),
            ("phrase first", phrase_first),
        ] {
            match parse_wallet_string(&input) {
                PasteOutcome::Complete(wallet) => {
                    assert_eq!(wallet.hot_address.to_string(), address, "{label}");
                    assert_eq!(wallet.phrase.as_str(), phrase, "{label}");
                }
                _ => panic!("{label}: the accepted shape must parse completely"),
            }
        }
    }

    #[test]
    fn edge_whitespace_is_trimmed_but_inner_separators_are_not_guessed_at() {
        let address = hot_address();
        let phrase = phrase_a();
        // A terminal paste routinely carries a trailing newline the user did not type.
        assert!(matches!(
            parse_wallet_string(&format!("  {address} {phrase}\n")),
            PasteOutcome::Complete(_)
        ));
        for rejected in [
            format!("{address}\t{phrase}"),
            format!("{address}\n{phrase}"),
            format!("{address}  {phrase}"),
            format!("{phrase}\r\n{address}"),
        ] {
            assert!(
                matches!(
                    parse_wallet_string(&rejected),
                    PasteOutcome::NeedWholeString {
                        why: PasteFault::Separator
                    }
                ),
                "a separator that is not one ASCII space must be refused, not guessed at"
            );
        }
    }

    #[test]
    fn a_glued_address_and_phrase_is_refused_rather_than_split() {
        let phrase = phrase_a();
        let glued = format!("{}::{HOT_ACCOUNT}{phrase}", HOT_ACCOUNT);
        // The first word of the phrase is welded onto the address: the address half is malformed
        // and the phrase is one word short, so neither half is taken.
        assert!(matches!(
            parse_wallet_string(&glued),
            PasteOutcome::NeedWholeString { .. } | PasteOutcome::NeedPhrase { .. }
        ));
    }

    #[test]
    fn an_extra_label_is_refused_even_though_both_parts_are_present() {
        let input = format!("hot: {} {}", hot_address(), phrase_a());
        assert!(matches!(
            parse_wallet_string(&input),
            PasteOutcome::NeedWholeString {
                why: PasteFault::Shape
            }
        ));
    }

    #[test]
    fn a_legacy_workchain_address_is_not_accepted_as_a_hot() {
        // `0:<hex>` carries no dApp identity, so it cannot name a self-dApp Hot.
        let input = format!("0:{HOT_ACCOUNT} {}", phrase_a());
        assert!(matches!(
            parse_wallet_string(&input),
            PasteOutcome::NeedAddress { .. } | PasteOutcome::NeedWholeString { .. }
        ));
    }

    #[test]
    fn a_canonical_address_that_is_not_self_dapp_is_refused() {
        let input = format!("{NOT_SELF_DAPP} {}", phrase_a());
        assert!(matches!(
            parse_wallet_string(&input),
            PasteOutcome::NeedAddress {
                why: PasteFault::AddressNotSelfDapp,
                ..
            }
        ));
    }

    #[test]
    fn two_addresses_or_two_phrases_are_ambiguous_and_re_prompt_that_part() {
        let address = hot_address();
        let phrase = phrase_a();

        let two_addresses = format!("{address} {address} {phrase}");
        assert!(matches!(
            parse_wallet_string(&two_addresses),
            PasteOutcome::NeedAddress {
                why: PasteFault::AddressAmbiguous,
                ..
            }
        ));

        let two_phrases = format!("{address} {phrase} {}", phrase_b());
        assert!(matches!(
            parse_wallet_string(&two_phrases),
            PasteOutcome::NeedPhrase {
                why: PasteFault::PhraseAmbiguous,
                ..
            }
        ));
    }

    #[test]
    fn twelve_wordlist_words_that_do_not_checksum_are_invalid_not_missing() {
        // Twelve real wordlist words whose checksum cannot hold.
        let words = vec!["abandon"; GOSHAI_PHRASE_WORDS];
        assert!(!is_valid_phrase(&words));
        let input = format!("{} {}", hot_address(), words.join(" "));
        assert!(matches!(
            parse_wallet_string(&input),
            PasteOutcome::NeedPhrase {
                why: PasteFault::PhraseInvalid,
                ..
            }
        ));
    }

    // -----------------------------------------------------------------------------------------
    // A partial paste re-prompts for exactly the missing half
    // -----------------------------------------------------------------------------------------

    #[test]
    fn a_phrase_only_paste_asks_only_for_the_address() {
        let mut prompt = ScriptedPrompt::new([phrase_a(), hot_address()]);
        let mut notices = Vec::new();
        let wallet = collect_wallet_string(&mut prompt, &mut notices).expect("both halves");
        assert_eq!(wallet.hot_address.to_string(), hot_address());
        assert_eq!(wallet.phrase.as_str(), phrase_a());
        assert_eq!(
            prompt.prompts,
            vec![PROMPT_WHOLE_STRING, PROMPT_ADDRESS_ONLY],
            "the phrase was already held, so only the address may be asked for"
        );
    }

    #[test]
    fn an_address_only_paste_asks_only_for_the_phrase() {
        let mut prompt = ScriptedPrompt::new([hot_address(), phrase_a()]);
        let mut notices = Vec::new();
        let wallet = collect_wallet_string(&mut prompt, &mut notices).expect("both halves");
        assert_eq!(wallet.hot_address.to_string(), hot_address());
        assert_eq!(wallet.phrase.as_str(), phrase_a());
        assert_eq!(
            prompt.prompts,
            vec![PROMPT_WHOLE_STRING, PROMPT_PHRASE_ONLY],
            "the address was already held, so only the phrase may be asked for"
        );
    }

    #[test]
    fn a_paste_with_neither_half_re_asks_for_the_whole_string() {
        let mut prompt = ScriptedPrompt::new([
            "not a wallet string at all".to_string(),
            format!("{} {}", hot_address(), phrase_a()),
        ]);
        let mut notices = Vec::new();
        collect_wallet_string(&mut prompt, &mut notices).expect("both halves");
        assert_eq!(
            prompt.prompts,
            vec![PROMPT_WHOLE_STRING, PROMPT_WHOLE_STRING]
        );
    }

    #[test]
    fn a_held_half_is_never_asked_for_again_even_after_the_other_half_is_rejected() {
        let mut prompt = ScriptedPrompt::new([
            phrase_a(),
            "still not an address".to_string(),
            hot_address(),
        ]);
        let mut notices = Vec::new();
        collect_wallet_string(&mut prompt, &mut notices).expect("both halves");
        assert_eq!(
            prompt.prompts,
            vec![
                PROMPT_WHOLE_STRING,
                PROMPT_ADDRESS_ONLY,
                PROMPT_ADDRESS_ONLY
            ],
            "the phrase is held; a failed address attempt must not re-ask for the phrase"
        );
    }

    // -----------------------------------------------------------------------------------------
    // A wrong paste never appears in any output
    // -----------------------------------------------------------------------------------------

    const ALL_FAULTS: [PasteFault; 10] = [
        PasteFault::Empty,
        PasteFault::Separator,
        PasteFault::Shape,
        PasteFault::AddressMissing,
        PasteFault::AddressAmbiguous,
        PasteFault::AddressMalformed,
        PasteFault::AddressNotSelfDapp,
        PasteFault::PhraseMissing,
        PasteFault::PhraseAmbiguous,
        PasteFault::PhraseInvalid,
    ];

    #[test]
    fn no_output_of_any_kind_repeats_what_was_entered() {
        let secret_phrase = phrase_a();
        // Five wrong pastes, each wrong in a different way, and each carrying text that must not
        // come back: a real phrase, a real address, and an invented label.
        let wrong = vec![
            format!("hot-wallet-label {} {secret_phrase}", hot_address()),
            format!("{}\t{secret_phrase}", hot_address()),
            "swordfish hunter2 correct-horse-battery-staple".to_string(),
            format!("{} {} {secret_phrase}", hot_address(), hot_address()),
            // This one is recognised as a phrase with an unusable address, so the loop switches to
            // the address-only prompt: the last answer below is what it then asks for.
            format!("{NOT_SELF_DAPP} {secret_phrase}"),
        ];
        let mut prompt =
            ScriptedPrompt::new(wrong.iter().cloned().chain(std::iter::once(hot_address())));
        let mut notices: Vec<u8> = Vec::new();
        let wallet = collect_wallet_string(&mut prompt, &mut notices).expect("both halves");
        assert_eq!(wallet.hot_address.to_string(), hot_address());
        let printed = String::from_utf8(notices).expect("notices are text");
        assert!(!printed.is_empty(), "each rejection must say something");

        // The exact property, rather than a search for fragments that could collide with ordinary
        // English: every line the user is shown is one of the ten `'static` fault messages. There is
        // no line that came from anywhere else, so there is no line that could carry input.
        let permitted: Vec<String> = ALL_FAULTS.iter().map(PasteFault::to_string).collect();
        for line in printed.lines() {
            assert!(
                permitted.iter().any(|allowed| allowed == line),
                "a line was printed that is not one of the fixed fault messages"
            );
        }
        assert_eq!(
            printed.lines().count(),
            wrong.len(),
            "one message per rejected attempt"
        );

        // And the things that matter most, checked directly.
        assert!(!printed.contains(&secret_phrase));
        assert!(!printed.contains(&hot_address()));
        assert!(!printed.contains(NOT_SELF_DAPP));
        assert!(!printed.contains("hot-wallet-label"));
        assert!(!printed.contains("swordfish"));
        for attempt in &wrong {
            assert!(
                !printed.contains(attempt.as_str()),
                "a rejected paste must never be echoed"
            );
        }
    }

    #[test]
    fn every_fault_message_is_static_text() {
        // The property that makes the test above hold for inputs nobody has thought of: the whole
        // fault vocabulary is `'static`, so there is no channel through which input could arrive.
        for fault in ALL_FAULTS {
            let _: &'static str = fault.class();
            let _: &'static str = fault.advice();
            assert!(!fault.class().is_empty());
            assert!(!fault.advice().is_empty());
            assert!(fault.to_string().starts_with(fault.class()));
        }
    }

    #[test]
    fn the_wallet_string_debug_never_carries_the_phrase() {
        let PasteOutcome::Complete(wallet) =
            parse_wallet_string(&format!("{} {}", hot_address(), phrase_a()))
        else {
            panic!("the accepted shape must parse completely");
        };
        let rendered = format!("{wallet:?}");
        assert!(rendered.contains(&hot_address()));
        assert!(!rendered.contains(wallet.phrase.as_str()));
        for word in wallet.phrase.split(' ') {
            assert!(!rendered.contains(word));
        }
    }

    // -----------------------------------------------------------------------------------------
    // Nothing reaches the disk before validation
    // -----------------------------------------------------------------------------------------

    fn count_entries(root: &std::path::Path) -> usize {
        let mut found = 0;
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                found += 1;
                if entry.path().is_dir() {
                    stack.push(entry.path());
                }
            }
        }
        found
    }

    #[test]
    fn an_unusable_paste_writes_nothing_at_all() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut prompt = ScriptedPrompt::new([
            "garbage".to_string(),
            format!("{} eleven words short of a phrase", hot_address()),
        ]);
        let mut notices: Vec<u8> = Vec::new();
        let outcome = prepare_onboarding(
            root.path(),
            crate::cli::wallet::test_network_a(),
            "binding-under-test",
            &mut prompt,
            &mut notices,
            &|_| Ok(OUR_KEY.to_string()),
        );
        assert!(outcome.is_err(), "the script ran out before both halves");
        assert_eq!(
            count_entries(root.path()),
            0,
            "no file or directory may be created before both halves have validated"
        );
    }

    #[test]
    fn a_phrase_that_will_not_derive_writes_nothing_at_all() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut prompt = ScriptedPrompt::new([format!("{} {}", hot_address(), phrase_a())]);
        let mut notices: Vec<u8> = Vec::new();
        let outcome = prepare_onboarding(
            root.path(),
            crate::cli::wallet::test_network_a(),
            "binding-under-test",
            &mut prompt,
            &mut notices,
            &|_| anyhow::bail!("derivation refused"),
        );
        assert!(outcome.is_err());
        assert_eq!(
            count_entries(root.path()),
            0,
            "the custodian key is derived before anything is written, so a failure leaves the \
             data directory untouched"
        );
    }

    #[test]
    fn a_validated_paste_writes_the_secret_and_the_draft_owner_only_and_no_binding_yet() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut prompt = ScriptedPrompt::new([format!("{} {}", phrase_a(), hot_address())]);
        let mut notices: Vec<u8> = Vec::new();
        let prepared = prepare_onboarding(
            root.path(),
            crate::cli::wallet::test_network_a(),
            "binding-under-test",
            &mut prompt,
            &mut notices,
            &|_| Ok(OUR_KEY.to_string()),
        )
        .expect("a validated paste prepares");

        let seed = std::fs::read_to_string(&prepared.paths.seed_file).expect("seed file");
        assert_eq!(
            seed,
            phrase_a(),
            "the phrase is stored, not a summary of it"
        );
        let draft: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&prepared.paths.draft_file).expect("draft file"),
        )
        .expect("draft is json");
        assert_eq!(draft["provider"], "gosh-ai");
        assert_eq!(draft["hot_address"], hot_address());
        assert_eq!(
            draft["hot_seed_file"],
            prepared.paths.seed_file.display().to_string()
        );
        assert!(
            !draft.to_string().contains(&phrase_a()),
            "the draft references the secret file and never copies the secret"
        );
        assert!(
            !prepared.paths.active_binding_file.exists(),
            "the binding is written only after the on-chain check, never before the wait"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for path in [&prepared.paths.seed_file, &prepared.paths.draft_file] {
                let mode = std::fs::metadata(path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o600, "{} must be owner-only", path.display());
            }
            let dir_mode = std::fs::metadata(&prepared.paths.binding_dir)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700, "the binding directory must be owner-only");
        }
    }

    #[test]
    fn each_binding_id_gets_its_own_directory_so_a_rebind_cannot_overwrite_old_secrets() {
        let root = tempfile::tempdir().expect("tempdir");
        let first = files::GoshAiPaths::new(
            root.path(),
            crate::cli::wallet::test_network_a(),
            "binding-one",
        );
        let second = files::GoshAiPaths::new(
            root.path(),
            crate::cli::wallet::test_network_a(),
            "binding-two",
        );
        assert_ne!(first.seed_file, second.seed_file);
        assert_ne!(first.draft_file, second.draft_file);
        // One active binding, whichever directory it points at.
        assert_eq!(first.active_binding_file, second.active_binding_file);
        assert!(first
            .active_binding_file
            .ends_with("wallet/active/net-a.json"));
    }

    // -----------------------------------------------------------------------------------------
    // The three states of an asynchronous deploy are waiting, not failure
    // -----------------------------------------------------------------------------------------

    #[test]
    fn not_found_non_exist_and_uninit_are_waiting_states() {
        let expected_deploy_states = ["NotFound", "NonExist", "Uninit"];
        // An account the chain does not have at all is `NotFound` expressed as absence.
        assert!(matches!(
            classify_hot_poll(&HotPoll::Absent),
            PollVerdict::KeepWaiting(_)
        ));
        for state in expected_deploy_states {
            assert!(
                matches!(
                    classify_hot_poll(&HotPoll::Status(state.to_string())),
                    PollVerdict::KeepWaiting(_)
                ),
                "{state} is an expected state of an asynchronous deploy, not a failure"
            );
        }
    }

    #[test]
    fn a_transient_read_error_keeps_waiting_rather_than_ending_onboarding() {
        assert!(matches!(
            classify_hot_poll(&HotPoll::ReadError),
            PollVerdict::KeepWaiting(_)
        ));
    }

    #[test]
    fn only_active_ends_the_wait() {
        assert_eq!(
            classify_hot_poll(&HotPoll::Status("Active".to_string())),
            PollVerdict::Active
        );
        assert!(matches!(
            classify_hot_poll(&HotPoll::Status("Frozen".to_string())),
            PollVerdict::KeepWaiting(_)
        ));
    }

    #[test]
    fn the_activation_wait_uses_the_canonical_parameters() {
        // The spec fixes ten minutes at five-second polling, and `params.rs` owns both.
        assert_eq!(
            dexdo_core::params::GOSHAI_HOT_ACTIVATION_TIMEOUT,
            std::time::Duration::from_secs(600)
        );
        assert_eq!(
            dexdo_core::params::GOSHAI_HOT_ACTIVATION_POLL_INTERVAL,
            std::time::Duration::from_secs(5)
        );
    }

    // -----------------------------------------------------------------------------------------
    // The refusal list, and that a refusal saves no binding
    // -----------------------------------------------------------------------------------------

    fn address() -> CanonicalAddress {
        CanonicalAddress::parse(&hot_address()).expect("fixture address")
    }

    #[test]
    fn a_supported_hot_holding_our_custodian_is_accepted() {
        assert_eq!(
            verify_active_hot(&address(), &good_facts(), OUR_KEY),
            Ok(())
        );
        // And with the `0x` prefix the chain may use on either side. The set carries the service
        // custodian too, because the subject here is prefix normalisation and a Gosh.ai Hot has
        // exactly two distinct pubkey custodians -- this fixture merely also happened to satisfy
        // the weaker rule that preceded that one.
        let mut facts = good_facts();
        facts.custodians = custodians_with(&[&format!("0x{OUR_KEY}"), OTHER_KEY]);
        assert_eq!(
            verify_active_hot(&address(), &facts, &format!("0X{OUR_KEY}")),
            Ok(())
        );
    }

    #[test]
    fn the_wrong_code_hash_is_refused() {
        let mut facts = good_facts();
        facts.code_hash = "f".repeat(64);
        assert!(matches!(
            verify_active_hot(&address(), &facts, OUR_KEY),
            Err(HotRefusal::UnsupportedCodeHash { .. })
        ));
        // Including the older spending hash dexdo still tolerates for an existing funding wallet:
        // a Gosh.ai Hot is created now, so it must be the current canonical contract.
        facts.code_hash = dexdo_core::canonical_multisig::LEGACY_SPENDING_CODE_HASH.to_string();
        assert!(matches!(
            verify_active_hot(&address(), &facts, OUR_KEY),
            Err(HotRefusal::UnsupportedCodeHash { .. })
        ));
    }

    #[test]
    fn the_wrong_version_or_contract_name_is_refused() {
        let mut facts = good_facts();
        facts.version = "2.3.0".to_string();
        assert!(matches!(
            verify_active_hot(&address(), &facts, OUR_KEY),
            Err(HotRefusal::UnsupportedVersion { .. })
        ));
        let mut facts = good_facts();
        facts.contract_name = "SomeOtherMultisig".to_string();
        assert!(matches!(
            verify_active_hot(&address(), &facts, OUR_KEY),
            Err(HotRefusal::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn a_threshold_other_than_one_is_refused() {
        for required in [0u8, 2, 3] {
            let mut facts = good_facts();
            facts.required_txn_confirms = required;
            assert_eq!(
                verify_active_hot(&address(), &facts, OUR_KEY),
                Err(HotRefusal::ThresholdNotOne {
                    required_txn_confirms: required
                })
            );
        }
    }

    #[test]
    fn a_hot_without_our_custodian_is_refused() {
        let mut facts = good_facts();
        facts.custodians = custodians_with(&[OTHER_KEY]);
        assert!(matches!(
            verify_active_hot(&address(), &facts, OUR_KEY),
            Err(HotRefusal::CustodianMissing { .. })
        ));

        // An address-only custodian set has no pubkey to match at all.
        let mut facts = good_facts();
        facts.custodians = serde_json::json!({
            "custodians": [{ "owner_pubkey": serde_json::Value::Null, "index": 0 }],
        });
        assert_eq!(
            verify_active_hot(&address(), &facts, OUR_KEY),
            Err(HotRefusal::NoCustodians)
        );

        // And a getter that does not return the array the compiled ABI declares is not a pass.
        let mut facts = good_facts();
        facts.custodians = serde_json::json!({ "value0": [] });
        assert_eq!(
            verify_active_hot(&address(), &facts, OUR_KEY),
            Err(HotRefusal::UnreadableCustodians)
        );
    }

    #[test]
    fn an_address_that_is_not_self_dapp_is_refused_even_when_everything_else_matches() {
        let not_self_dapp = CanonicalAddress::parse(NOT_SELF_DAPP).expect("fixture address");
        assert!(matches!(
            verify_active_hot(&not_self_dapp, &good_facts(), OUR_KEY),
            Err(HotRefusal::NotSelfDapp { .. })
        ));
    }

    #[test]
    fn a_refusal_saves_no_binding() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut prompt = ScriptedPrompt::new([format!("{} {}", hot_address(), phrase_a())]);
        let mut notices: Vec<u8> = Vec::new();
        let prepared = prepare_onboarding(
            root.path(),
            crate::cli::wallet::test_network_a(),
            "binding-under-test",
            &mut prompt,
            &mut notices,
            &|_| Ok(OUR_KEY.to_string()),
        )
        .expect("a validated paste prepares");

        // Every refusal, applied to the Hot this onboarding is about.
        let mut wrong_hash = good_facts();
        wrong_hash.code_hash = "0".repeat(64);
        let mut wrong_threshold = good_facts();
        wrong_threshold.required_txn_confirms = 2;
        let mut not_ours = good_facts();
        not_ours.custodians = custodians_with(&[OTHER_KEY]);

        for facts in [wrong_hash, wrong_threshold, not_ours] {
            assert!(
                verify_active_hot(&prepared.hot_address, &facts, &prepared.derived_public_key)
                    .is_err()
            );
            assert!(
                !prepared.paths.active_binding_file.exists(),
                "a refused Hot must leave no active binding behind"
            );
        }

        // The secret and the draft do survive: they are what makes a re-run resumable, and the Hot
        // may still hold funds.
        assert!(prepared.paths.seed_file.exists());
        assert!(prepared.paths.draft_file.exists());
    }

    #[test]
    fn an_accepted_hot_saves_a_gosh_ai_binding_with_no_vault_address() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut prompt = ScriptedPrompt::new([format!("{} {}", hot_address(), phrase_a())]);
        let mut notices: Vec<u8> = Vec::new();
        let prepared = prepare_onboarding(
            root.path(),
            crate::cli::wallet::test_network_a(),
            "binding-under-test",
            &mut prompt,
            &mut notices,
            &|_| Ok(OUR_KEY.to_string()),
        )
        .expect("a validated paste prepares");

        verify_active_hot(
            &prepared.hot_address,
            &good_facts(),
            &prepared.derived_public_key,
        )
        .expect("a supported Hot holding our custodian");
        // The flow BUILDS the binding; the store is the single writer and archives what it
        // replaces. Committing here is what the production path does in `run_selected`.
        let binding = files::active_binding(
            &prepared.paths,
            crate::cli::wallet::test_network_a(),
            &prepared.hot_address,
        );
        let store = crate::cli::wallet::WalletStore::at(&prepared.paths.wallet_root);
        assert_eq!(
            store.binding_path(&crate::cli::wallet::test_network_a()),
            prepared.paths.active_binding_file,
            "the store and this flow must name the same active binding file"
        );
        store
            .commit_active(&binding)
            .expect("save the active binding");

        assert_eq!(binding.provider, WalletProvider::GoshAi);
        assert_eq!(binding.hot_address, hot_address());
        assert_eq!(
            binding.hot_seed_file,
            Some(prepared.paths.seed_file.clone())
        );
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&prepared.paths.active_binding_file).expect("binding file"),
        )
        .expect("binding is json");
        assert_eq!(written["provider"], "gosh-ai");
        assert!(
            written.get("vault_address").is_none(),
            "Gosh.ai provides no Vault, so the binding must not name one"
        );
        assert!(
            !written.to_string().contains(&phrase_a()),
            "the binding references the secret file and never copies the secret"
        );
    }

    /// Decision 3 of the integration: the resume file and the binding must be spelled by the
    /// SAME types, so a change to one cannot leave the other behind.

    /// The draft used to be a `serde_json::json!` literal with a `&str` provider constant. A literal
    /// does not follow a type change, so the binding could become typed while the file the resume
    /// path reads stayed hand-spelled and the two would drift the first time anyone touched either.
    /// This asserts the file `draft_of` actually writes reads back into the typed draft with every
    /// field intact -- which is only possible while both sides share one definition.
    #[test]
    fn a_draft_written_by_draft_of_round_trips_through_the_typed_reader() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut prompt = ScriptedPrompt::new([format!("{} {}", hot_address(), phrase_a())]);
        let mut notices: Vec<u8> = Vec::new();
        let prepared = prepare_onboarding(
            root.path(),
            crate::cli::wallet::test_network_b(),
            "binding-round-trip",
            &mut prompt,
            &mut notices,
            &|_| Ok(OUR_KEY.to_string()),
        )
        .expect("a validated paste prepares");

        let written = std::fs::read(&prepared.paths.draft_file).expect("draft file");
        let read_back: GoshAiOnboardingDraft =
            serde_json::from_slice(&written).expect("the draft reads back as the typed draft");

        assert_eq!(read_back.provider, WalletProvider::GoshAi);
        assert_eq!(
            read_back.network,
            crate::cli::wallet::test_network_b(),
            "the network the operator chose must survive the round trip, not a default"
        );
        assert_eq!(read_back.id, "binding-round-trip");
        assert_eq!(read_back.version, crate::cli::wallet::BINDING_VERSION);
        assert_eq!(read_back.hot_address, hot_address());
        assert_eq!(read_back.hot_seed_file, prepared.paths.seed_file);
        assert_eq!(read_back.phase, GOSHAI_DRAFT_PHASE_AWAITING_HOT);

        // The wire spelling is the binding's spelling, because it is the binding's type.
        let raw: serde_json::Value = serde_json::from_slice(&written).expect("draft is json");
        assert_eq!(raw["provider"], "gosh-ai");
        assert_eq!(raw["network"], "net-b");
        assert!(
            !String::from_utf8_lossy(&written).contains(&phrase_a()),
            "the draft references the secret file and never copies the secret"
        );
    }

    /// The draft and the binding agree on provider and network because neither can be spelled
    /// independently of the other.
    #[test]
    fn the_draft_and_the_binding_cannot_disagree_about_provider_or_network() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut prompt = ScriptedPrompt::new([format!("{} {}", hot_address(), phrase_a())]);
        let mut notices: Vec<u8> = Vec::new();
        let prepared = prepare_onboarding(
            root.path(),
            crate::cli::wallet::test_network_b(),
            "binding-agreement",
            &mut prompt,
            &mut notices,
            &|_| Ok(OUR_KEY.to_string()),
        )
        .expect("prepares");
        let draft: GoshAiOnboardingDraft =
            serde_json::from_slice(&std::fs::read(&prepared.paths.draft_file).expect("draft"))
                .expect("typed draft");
        let binding = files::active_binding(
            &prepared.paths,
            crate::cli::wallet::test_network_b(),
            &prepared.hot_address,
        );

        assert_eq!(draft.provider, binding.provider);
        assert_eq!(draft.network, binding.network);
        assert_eq!(draft.id, binding.id);
        assert_eq!(draft.hot_address, binding.hot_address);
        assert_eq!(Some(draft.hot_seed_file), binding.hot_seed_file);
    }

    /// The store archives what it replaces; this flow must never write the active binding itself.
    #[test]
    fn a_second_gosh_ai_binding_archives_the_first_instead_of_clobbering_it() {
        let root = tempfile::tempdir().expect("tempdir");
        let wallet_root = root.path().join("wallet");
        let store = crate::cli::wallet::WalletStore::at(&wallet_root);

        let reserved_first = store.open_draft().expect("reserve the first binding's id");
        let first = files::active_binding(
            &files::GoshAiPaths::new(
                root.path(),
                crate::cli::wallet::test_network_a(),
                reserved_first.id(),
            ),
            crate::cli::wallet::test_network_a(),
            &dexdo_core::CanonicalAddress::parse(&hot_address()).expect("hot"),
        );
        assert!(
            store.commit_active(&first).expect("first commit").is_none(),
            "there was nothing to archive the first time"
        );

        let reserved_second = store.open_draft().expect("reserve the second binding's id");
        let second = files::active_binding(
            &files::GoshAiPaths::new(
                root.path(),
                crate::cli::wallet::test_network_a(),
                reserved_second.id(),
            ),
            crate::cli::wallet::test_network_a(),
            &dexdo_core::CanonicalAddress::parse(&hot_address()).expect("hot"),
        );
        let archived = store
            .commit_active(&second)
            .expect("second commit")
            .expect("the replaced binding must be archived, never clobbered");

        let kept: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&archived).expect("archive file"))
                .expect("archive is json");
        assert_eq!(
            kept["id"], first.id,
            "funds can still sit in the old Hot, so its address must remain recoverable"
        );
        assert_eq!(
            store
                .load_active(&crate::cli::wallet::test_network_a())
                .expect("load")
                .expect("active")
                .id,
            second.id
        );
    }

    /// The link is the deployment's to state, and the client keeps no copy of it.

    /// This replaced `the_gosh_ai_link_is_a_single_placeholder_constant`, which pinned the
    /// placeholder and was correct while the URL lived in this file. moved it into the
    /// manifest, so what is worth holding now is the opposite: that no URL survives here to be
    /// picked up again by whoever next needs "the Gosh.ai link".
    #[test]
    fn no_goshai_url_is_compiled_into_this_module() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/cli/wallet_goshai.rs"),
        )
        .expect("read this module");

        // The HOST, built at runtime so this guard does not match its own source line. The host is
        // what makes a string a link: `"https://` alone would also match the scheme CHECK in
        // `goshai_invitation_url`, which is code doing its job rather than a link left behind.
        // Every spelling that puts the removed link back trips this -- the literal,
        // `concat!("https", "://gosh.ai")`, and `format!("{scheme}://gosh.ai")` alike, because all
        // three have to name the host somewhere.
        // The host, lowercase and whole, built at runtime so this guard does not match its own
        // source line. `://gosh` was weaker than its own claim: `concat!("https:", "//gosh.ai")`
        // splits at a different point and slips through. The module's prose says `Gosh.ai` with a
        // capital G, so the lowercase form has no false positives here.
        let host = format!("gosh{}ai", '.');

        for line in source.lines() {
            let statement = line.trim_start();
            if statement.starts_with("//") {
                continue;
            }
            assert!(
                !statement.contains(&host),
                "a Gosh.ai link is back in this module as code, and the manifest is where it \
                 belongs: {line}"
            );
        }
    }
}
