//! The shared Hot check-and-fund mechanism and its durable funding journal.

//! Checking the Hot's balance and asking the Vault for a top-up is not `note deploy`'s business, or
//! `wallet onboard`'s: every operation that spends Hot funds directly needs the same eight steps, so
//! they live here once. The specification names the entry point `ensure_hot_funded(binding,
//! requirements, operation)` and that is [`ensure_hot_funded`].

//! Two commands spend a Hot directly - `note deploy` and `note topup`. Everything else in dexdo is
//! note-funded. Those two are therefore the callers this is built for, and both of them reach this
//! module through [`ensure_hot_funded_with_turn`] while already holding the funding-wallet lock they
//! have shared since.

//! What this module owns:

//! - the preflight read of the Hot's on-chain balances and the per-currency shortfall;
//! - the bounded wait for those balances to reach the required level;
//! - the re-check immediately before the caller spends, serialized per Hot;
//! - the durable journal that keeps distinct live request amounts and de-duplicates exact repeats.

//! What this module does NOT own: how any particular provider gets money into the Hot. That is
//! [`HotFundingProvider`], one implementation per provider, in [`providers`]. The specification is
//! explicit that a provider's own answer is never proof of funding - in the Gosh.ai flow there is no
//! answer at all - so the only thing this module will accept as proof is the Hot's observed
//! on-chain balance.

//! # The binding this reads

//! [`HotFundingBinding`] is a VIEW of the production binding in [`crate::cli::wallet`], built by
//! [`HotFundingBinding::from_active`], and [`WalletProvider`] IS the production enum re-exported.
//! There is no second binding schema and no second provider enum: a funding flow chosen from a
//! provider this module invented for itself would be a funding flow the operator never bound.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use dexdo_core::CanonicalAddress;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The provider model is the production one. A funding flow is selected from the binding the
/// operator committed, never from a parallel enum that could drift from it.
pub(crate) use crate::cli::wallet::WalletProvider;

/// Journal schema version. Bumped only when an older file can no longer be read safely.

/// Version 3 stores multiple requests and an explicit UTC deadline for each one. Version 2 already
/// carries every fact needed to migrate its single record: the deadline is its `created_at_unix`
/// plus the canonical lifetime. A version-1 record cannot be upgraded in place by guessing the
/// immutable fingerprint and generation: its
/// fingerprint is exactly what a repeat needs in order to recognise a request already on chain, and
/// inventing one would make an unrecognisable request look absent - the single state that authorizes
/// a second transfer out of a cold Vault. A version-1 file is therefore REFUSED, loudly, rather than
/// migrated.
const FUNDING_JOURNAL_VERSION: u32 = 3;

// ---------------------------------------------------------------------------------------------
// The shape of a Vault -> Hot transfer
// ---------------------------------------------------------------------------------------------

/// `sendFlags` for the Vault -> Hot transfer.

/// Flag 1 pays the message's fees from the Vault's balance rather than out of the amount being
/// sent, so the Hot receives exactly the figure the operator confirmed in the wallet application.
/// Flag 16 - the one the uninit-deploy funding path uses - would collapse the ECC[2] SHELL into the
/// destination's NATIVE balance, and a Hot holding gas where it needs currency is a Hot that still
/// cannot pay for a note. `note topup` sends to a PrivateNote with flag 1 for the same reason
/// (`note_cmd.rs`), and this is the same kind of transfer to a different account.
pub(crate) const VAULT_TO_HOT_SEND_FLAGS: u16 = 1;

/// `bounce` for the Vault -> Hot transfer: the message carries money, so on any refusal it comes
/// home to the Vault instead of resting on an address that did not take it.
pub(crate) const VAULT_TO_HOT_BOUNCE: bool = true;

/// The payload of the Vault -> Hot transfer: empty.

/// An empty body is what makes this a plain currency transfer - the Hot sees no function to run, so
/// its `receive()` takes the message and the ECC[2] stays ECC[2].
pub(crate) const VAULT_TO_HOT_PAYLOAD: &str = "";

// ---------------------------------------------------------------------------------------------
// The binding view
// ---------------------------------------------------------------------------------------------

/// The four facts the shared funding mechanism needs from the active binding.

/// Derived from [`crate::cli::wallet::WalletBinding`] and never assembled from anywhere else. It is
/// a view rather than the binding itself because everything else the binding carries - its id, its
/// schema version, the PATHS to owner-only secret files - is either irrelevant to the decision made
/// here or must not travel into a journal record that is non-secret by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HotFundingBinding {
    /// Recorded at binding time, never guessed afterwards.
    pub(crate) provider: WalletProvider,
    /// The network the addresses below are on. It is part of the journal key, so a Hot address that
    /// happens to repeat across networks does not collide.
    pub(crate) network: String,
    /// The full canonical `<dapp_id>::<account_id>` Hot address.
    pub(crate) hot_address: String,
    /// The full canonical Vault address, when the provider has one. Gosh.ai and manual do not.
    pub(crate) vault_address: Option<String>,
}

/// The name the mechanism and its tests have always used for the view above.
pub(crate) type WalletBinding = HotFundingBinding;

impl HotFundingBinding {
    /// The view of the binding the operator actually committed.
    pub(crate) fn from_active(binding: &crate::cli::wallet::WalletBinding) -> Self {
        Self {
            provider: binding.provider,
            network: binding.network.as_str().to_string(),
            hot_address: binding.hot_address.clone(),
            vault_address: binding.vault_address.clone(),
        }
    }

    /// The Hot as a parsed canonical address.

    /// Parsing is not a formality here: the DApp id half is what a Vault -> Hot transfer has to be
    /// addressed into, and it is only available because the binding stores the canonical form
    /// rather than the legacy `0:<account_id>` one.
    pub(crate) fn hot(&self) -> Result<CanonicalAddress> {
        CanonicalAddress::parse(&self.hot_address)
            .map_err(|e| anyhow!("wallet binding hot_address is not a canonical address: {e}"))
    }

    /// The Vault as a parsed canonical address, when the provider has one.
    pub(crate) fn vault(&self) -> Result<Option<CanonicalAddress>> {
        self.vault_address
            .as_deref()
            .map(|vault| {
                CanonicalAddress::parse(vault).map_err(|e| {
                    anyhow!("wallet binding vault_address is not a canonical address: {e}")
                })
            })
            .transpose()
    }
}

impl WalletProvider {
    /// Whether this provider can put a durable funding request on chain.

    /// Only the Acki Nacki Wallet flow can: it has a Vault, and a `submitTransaction` sitting in
    /// that Vault's queue with one signature IS the request. Gosh.ai and manual have no server-side
    /// request to create, so for them there is nothing that a repeat of the command could
    /// duplicate.
    pub(crate) fn creates_vault_request(self) -> bool {
        matches!(self, Self::AckinackiWallet)
    }
}

// ---------------------------------------------------------------------------------------------
// Requirements and balances
// ---------------------------------------------------------------------------------------------

/// What the calling operation needs the Hot to hold, per currency, at the moment it spends.

/// These are required FINAL balances, not deltas: the specification asks the caller to compute "the
/// exact need per currency", and a final balance is the only form of that which stays correct while
/// the balance is moving underneath the wait.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FundingRequirements {
    /// Required final native vmshell balance. Native is not an ECC currency and must never be
    /// represented by an invented currency id.
    pub(crate) required_native: u128,
    /// Currency id -> required total balance.
    pub(crate) required: BTreeMap<u32, u128>,
}

impl Default for FundingRequirements {
    fn default() -> Self {
        Self::new(std::iter::empty())
    }
}

impl FundingRequirements {
    pub(crate) fn new(required: impl IntoIterator<Item = (u32, u128)>) -> Self {
        Self {
            required_native: vault_to_hot_native_value(),
            required: required.into_iter().collect(),
        }
    }

    /// Native vmshell shortfall against the final floor required by this money path.
    pub(crate) fn native_shortfall(&self, balances: &HotBalances) -> u128 {
        self.required_native.saturating_sub(balances.native)
    }

    /// Per-currency shortfall against `balances`. Empty means the requirement is met.
    pub(crate) fn shortfall(&self, balances: &HotBalances) -> BTreeMap<u32, u128> {
        self.required
            .iter()
            .filter_map(|(currency, required)| {
                let missing = required.saturating_sub(balances.get(*currency));
                (missing > 0).then_some((*currency, missing))
            })
            .collect()
    }

    /// Whether `balances` satisfies every requirement.
    pub(crate) fn met_by(&self, balances: &HotBalances) -> bool {
        self.native_shortfall(balances) == 0 && self.shortfall(balances).is_empty()
    }
}

/// A Hot's observed balances. Native vmshell is disjoint from every ECC currency.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HotBalances {
    pub(crate) native: u128,
    pub(crate) balances: BTreeMap<u32, u128>,
}

impl HotBalances {
    pub(crate) fn new(native: u128, balances: impl IntoIterator<Item = (u32, u128)>) -> Self {
        Self {
            native,
            balances: balances.into_iter().collect(),
        }
    }

    pub(crate) fn get(&self, currency: u32) -> u128 {
        self.balances.get(&currency).copied().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------------------------
// The seams
// ---------------------------------------------------------------------------------------------

/// Reads a Hot's balances from the chain.

/// A read failure must surface as `Err`. It must never be reported as a zero balance: a zero
/// balance is a fact that keeps the wait going, whereas an unreadable chain is the state in which
/// nothing may be concluded and nothing may be submitted.
#[async_trait::async_trait(?Send)]
pub(crate) trait HotBalanceReader {
    async fn hot_balances(&self, hot: &CanonicalAddress) -> Result<HotBalances>;
}

/// Everything a provider needs to recognise or create one funding request.

/// The recognition fields are exactly the ones the specification names: destination address,
/// creator public key, and the exact currencies and amounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FundingRequest {
    pub(crate) provider: WalletProvider,
    pub(crate) network: String,
    /// Full canonical Vault address, when the provider has one.
    pub(crate) vault_address: Option<String>,
    /// Full canonical Hot address - the destination.
    pub(crate) hot_address: String,
    /// The DApp half of the Hot's canonical address.

    /// A Vault -> Hot transfer is addressed with this, NOT with the dexdo DApp. The canonical
    /// multisig `submitTransaction`/`sendTransaction` parameter builders in
    /// `crates/core/src/canonical_multisig.rs` hard-code `dapp_id = ROOT_PN_DAPP_ID` ("4"), which is
    /// right for every caller they have today - all of them address a dexdo contract (RootPN, a
    /// PrivateNote), and dexdo contracts all live in DApp 4. A Hot does not: it is a self-DApp
    /// multisig, so its DApp half equals its own account id. Carrying the Hot's own DApp id on the
    /// request is what keeps a provider from inheriting the constant by accident.
    pub(crate) hot_dapp_id: String,
    /// Public key of the agent that creates the Vault request, as recorded and matched later.
    pub(crate) creator_pubkey: String,
    /// The required final balances this request is meant to reach.
    pub(crate) required: BTreeMap<u32, u128>,
    /// Required final native vmshell balance, kept separate from ECC currencies.
    pub(crate) required_native: u128,
    /// The shortfall computed when the request was prepared.
    pub(crate) shortfall: BTreeMap<u32, u128>,
    /// Exact native vmshell shortfall computed when the request was prepared.
    pub(crate) native_shortfall: u128,
}

/// The immutable identity of ONE Vault -> Hot transfer.

/// Every field the specification names, and they are DERIVED from the request rather than chosen
/// separately: `FundingFingerprint::of(&request)` is what the journal records and it is also what
/// the provider builds its `submitTransaction` parameters from. One derivation feeding both is the
/// only arrangement in which the request that went on the wire and the request the journal claims
/// went on the wire cannot disagree - and a fingerprint that does not describe the real transfer
/// would fail to recognise it in the queue, which reads as absence, which authorizes a second
/// transfer out of a cold Vault.

/// Frozen for one GENERATION. A later run with the same amount keeps looking for this transfer; a
/// different native value or ECC map gets a separate generation without editing this one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FundingFingerprint {
    /// `creator.owner_pubkey` as the Vault's queue reports it.
    pub(crate) creator: String,
    /// `dest` - the full canonical Hot address.
    pub(crate) dest: String,
    /// `dapp_id` - the Hot's own DApp half.
    pub(crate) dapp_id: String,
    /// `value` - the native part of the transfer.
    pub(crate) value: u128,
    /// `cc` - the extra-currency part, currency id -> amount.
    pub(crate) cc: BTreeMap<u32, u128>,
    pub(crate) send_flags: u16,
    pub(crate) bounce: bool,
    /// sha256 of the transfer's payload. The payload is empty for a plain currency transfer, so
    /// this is a constant today - and it is recorded rather than assumed so that a client which
    /// ever attaches a body cannot silently match a request that carried a different one.
    pub(crate) payload_hash: String,
}

impl FundingFingerprint {
    /// The fingerprint of the transfer `request` describes.
    pub(crate) fn of(request: &FundingRequest, native_floor: u128) -> Self {
        // The second argument is the canonical target/cap, not a fixed transfer amount. The
        // request freezes the observed shortfall; capping it at that target keeps a corrupt request
        // from asking for more native than the path's entire final floor.
        let native_shortfall = request.native_shortfall.min(native_floor);
        Self {
            creator: request.creator_pubkey.clone(),
            dest: request.hot_address.clone(),
            dapp_id: request.hot_dapp_id.clone(),
            value: native_shortfall,
            cc: request.shortfall.clone(),
            send_flags: VAULT_TO_HOT_SEND_FLAGS,
            bounce: VAULT_TO_HOT_BOUNCE,
            payload_hash: payload_hash(VAULT_TO_HOT_PAYLOAD),
        }
    }
}

impl FundingFingerprint {
    /// The payload to put on the wire for this fingerprint.

    /// Checked rather than assumed: the wire payload and the recorded `payload_hash` describe the
    /// same transfer, and a client that sent a body its own record did not describe would create a
    /// request no later run could recognise - which reads as absence, which authorizes a second
    /// transfer out of a cold Vault.
    pub(crate) fn payload_for_wire(&self) -> Result<&'static str> {
        if self.payload_hash != payload_hash(VAULT_TO_HOT_PAYLOAD) {
            bail!(
                "the recorded funding request carries payload hash {} but this client only sends \
                 the empty payload; refusing to submit a transfer that does not match the request \
                 it is recorded as",
                self.payload_hash
            );
        }
        Ok(VAULT_TO_HOT_PAYLOAD)
    }
}

/// sha256 of a transfer payload, hex.
pub(crate) fn payload_hash(payload: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(payload.as_bytes());
    hex::encode(hasher.finalize())
}

/// What the chain proved about a request that is no longer in the Vault's queue.

/// Recorded so the conclusion can be audited later: "the client decided this had executed" is not
/// the same claim as "the chain showed this executed, here is what it showed".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FundingEvidence {
    /// What the evidence establishes, in one word, for the record and for the operator.
    pub(crate) verdict: String,
    /// Where it came from - the finalized message or transaction that carried it.
    pub(crate) source: String,
    /// The chain time the evidence was observed at, when the source carries one.
    pub(crate) observed_at_unix: Option<u64>,
    /// A human-readable rendering of the fact itself.
    pub(crate) detail: String,
    /// The INTERNAL message that carried this transfer to the Hot, once the Hot's own finalized
    /// receipt for it has been read.

    /// Structural rather than a phrase inside `source`, because a decision is taken from it: it is
    /// the only fact that says the credit which landed on the Hot is THIS generation's delivery and
    /// not an unrelated incoming transfer of the same size. `None` is "not established", which is
    /// where a fresh generation is refused - never "no delivery".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) delivery_message_id: Option<String>,
}

/// Whether a funding request matching [`FundingRequest`] is on chain.

/// These are not two answers plus an error. `Unknown` is load-bearing: the specification says a
/// chain read failure means "unknown" and forbids a repeat submit, and it says that a request leaving
/// the live queue proves nothing on its own. Finalized history can prove execution and dates an
/// external queue entry before it is adopted; expiry then follows from the one canonical lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RequestPresence {
    /// Proven present in the Vault's queue. `chain_created_at_unix` is present when the request was
    /// found by fingerprint without an already recorded queue id, so adoption can replace a
    /// provisional local timestamp with the finalized `TransactionSubmitted` message's chain time.
    Present {
        transaction_hash: Option<String>,
        pending_transaction_id: Option<String>,
        chain_created_at_unix: Option<u64>,
    },
    /// Gone from the queue, and finalized history proves it EXECUTED. The money left the Vault.
    /// A verdict bound to this generation's parseable queue id retires that generation; an
    /// id-less history fallback remains conservative and never permits another submit.
    Executed { evidence: FundingEvidence },
    /// Proven absent, with nothing of ours ever having been in the queue. This permits an initial
    /// submit; it never overrides a journal record that says a generation may still execute.
    Absent,
    /// Could not be established. Never permits a submit.
    Unknown { reason: String },
}

/// The result of asking a provider to create the funding request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubmitOutcome {
    /// The request is proven to be on chain, with whatever identifiers were returned.
    Accepted {
        transaction_hash: Option<String>,
        pending_transaction_id: Option<String>,
    },
    /// The submit's result could not be established. The journal stays at `prepared` and no second
    /// submit is made; the next run reconciles.
    Indeterminate { reason: String },
}

/// A provider's half of the funding flow.
#[async_trait::async_trait(?Send)]
pub(crate) trait HotFundingProvider {
    /// Which provider this is. Checked against the journal so a run cannot continue a request that
    /// a different provider created.
    fn provider(&self) -> WalletProvider;

    /// Keep the provider's chain probe aligned with the journal the mechanism just read.

    /// Normally this is the same record supplied when the production provider is constructed. A
    /// request may also be created while one command is waiting, though, so the provider must see
    /// that newly written queue id before the final balance observation is allowed to retire it.
    fn refresh_recorded_request(&self, _recorded: Option<RecordedRequest>) {}

    /// Prove whether a request matching `request` is already on chain.

    /// Only called for a provider whose [`WalletProvider::creates_vault_request`] is true.
    /// Returning `Absent` is a claim that the queue was read, that nothing of ours is in it AND
    /// that nothing of ours was ever there; a request that HAS been there and is now gone must come
    /// back as `Executed` or `Unknown`, never as `Absent`.

    /// What an earlier run recorded - the frozen fingerprint and, once the chain has ever reported
    /// one, the pending transaction id that is the primary key from then on - reaches a provider
    /// through its constructor as a [`RecordedRequest`], not through this call. The production
    /// caller reads the journal under the same held turn this runs under, so the two readings
    /// cannot disagree.
    async fn probe_existing_request(&self, request: &FundingRequest) -> Result<RequestPresence>;

    /// Create the request on chain.
    async fn create_request(&self, request: &FundingRequest) -> Result<SubmitOutcome>;

    /// What to tell the operator when there is no request to create - the Gosh.ai link, or the
    /// manual shortfall and Hot address.
    fn manual_instruction(&self, request: &FundingRequest) -> String;
}

/// What the top-up prints a payment code for: the destination, the amount, and the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TopUpPaymentCode {
    pub(crate) address: String,
    /// Whole SHELL, rounded UP: the link carries a display-unit decimal, and asking for less than
    /// the shortfall leaves the command waiting on money that will never be enough.
    pub(crate) whole_shell: u128,
    /// The chain the request was built for -- NOT `current_network()`. The request already carries
    /// it, and reading the environment a second time is how the two could come to differ.
    pub(crate) network: String,
}

/// Decide what code a top-up prints, or that it prints none.

/// A named decision rather than two nested `if`s inside `resolve_hot_funding`, for the reason
/// [`crate::cli::wallet_manual::ManualFundingWait`] gives at the same kind of fork: the function
/// holding it is reachable only from the product, so everything decided inside it is untestable and
/// therefore unprotected. Measured by review on the first version of: replacing the network
/// argument at that call site with an empty string left every one of the eleven new tests green,
/// because they all drove the builder with a label handed in by hand. This is the wiring the issue
/// is about, and it is now the thing under test.

/// `None` in two cases, and both are correct silence: a provider that tops up somewhere other than
/// this address (Gosh.ai does it on a web page), and no ECC[2] SHELL shortfall at all. Native
/// shortfall is deliberately not offered as a code -- see the block comment at the call site.
pub(crate) fn top_up_payment_code(
    provider: WalletProvider,
    request: &FundingRequest,
) -> Option<TopUpPaymentCode> {
    if !matches!(provider, WalletProvider::Manual) {
        return None;
    }
    let ecc_shell = request
        .shortfall
        .get(&dexdo_core::params::SHELL_CURRENCY_ID)
        .copied()
        .filter(|shortfall| *shortfall > 0)?;

    Some(TopUpPaymentCode {
        address: request.hot_address.clone(),
        // No `.max(1)`: the filter above already dropped a zero shortfall, so `div_ceil` of a
        // positive value is at least 1. The clamp used to guard a path that no longer exists.
        whole_shell: ecc_shell.div_ceil(dexdo_core::params::SHELL_UNIT),
        network: request.network.clone(),
    })
}

/// What an earlier run recorded about the request it may have put on chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecordedRequest {
    /// The generation this request belongs to.
    pub(crate) generation: u32,
    /// The frozen transfer identity.
    pub(crate) fingerprint: FundingFingerprint,
    /// The queue id, once the chain has ever reported one. From that moment it is the primary key
    /// and the fingerprint is the corroboration, exactly as the specification fixes it.
    pub(crate) pending_transaction_id: Option<String>,
    /// The transaction hash of our own submit, when its receipt was seen.
    pub(crate) transaction_hash: Option<String>,
    /// Whether this journal durably recorded that its own submit call was about to begin.
    pub(crate) submit_attempted: bool,
    /// The canonical UTC age anchor: local creation time for this process's durable `Prepared`
    /// record, or finalized chain admission time after adopting an external queue request.
    pub(crate) created_at_unix: u64,
}

// ---------------------------------------------------------------------------------------------
// The durable journal
// ---------------------------------------------------------------------------------------------

/// The states one funding request moves through.

/// `Prepared` is written and flushed BEFORE any `submitTransaction`, and that ordering is the whole
/// basis of repeat safety - see [`ensure_hot_funded`]. For a provider that never creates a request
/// it degenerates to "an open funding need for this Hot", which is a superset of the same meaning
/// and keeps one state machine instead of two.

/// `Executed` means the transfer left the Vault. `Expired` remains readable for version-2 journal
/// compatibility; version 3 removes unexecuted entries whose canonical local deadline has passed
/// instead of writing that state. Execution is never concluded from the request's absence, only
/// from finalized chain evidence retained on the record as [`FundingEvidence`].

/// `Satisfied` is reached from any of the others and only ever by an observed Hot balance that
/// meets the requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FundingState {
    Prepared,
    Submitted,
    Executed,
    Expired,
    Satisfied,
}

/// One request in a Hot's funding journal. Non-secret by construction: addresses, a public key,
/// amounts, states and timestamps. No key material and no seed phrase ever reaches this file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FundingJournalRecord {
    /// Which attempt at funding this Hot the record describes.

    /// A generation is the unit over which one exact native value and ECC map are frozen. A new
    /// generation is created when no live record has that exact amount; other live generations stay
    /// independently confirmable. A timeout or queue disappearance alone never replaces an exact
    /// live generation.
    pub(crate) generation: u32,
    pub(crate) provider: WalletProvider,
    pub(crate) network: String,
    pub(crate) vault_address: Option<String>,
    pub(crate) hot_address: String,
    pub(crate) creator_pubkey: String,
    pub(crate) required: BTreeMap<u32, u128>,
    #[serde(default)]
    pub(crate) required_native: u128,
    pub(crate) shortfall: BTreeMap<u32, u128>,
    #[serde(default)]
    pub(crate) native_shortfall: u128,
    /// The frozen identity of this generation's transfer.
    pub(crate) fingerprint: FundingFingerprint,
    pub(crate) state: FundingState,
    pub(crate) transaction_hash: Option<String>,
    pub(crate) pending_transaction_id: Option<String>,
    /// Written before the submit call begins. It distinguishes an unresolved local submit from a
    /// merely observed external queue entry, whose age must come from finalized chain history.
    #[serde(default)]
    pub(crate) submit_attempted: bool,
    /// What the chain showed about this request leaving the queue, when it has left it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) evidence: Option<FundingEvidence>,
    pub(crate) created_at_unix: u64,
    /// UTC Unix deadline. The request is live at this second and expires one second later.
    pub(crate) expires_at_unix: u64,
    /// When the Hot's balances were last read against this record. A local timeout does not move
    /// it: it records reconciliation with the chain, not the passage of time.
    pub(crate) last_checked_at_unix: Option<u64>,
    /// The balances that closed the record. Present only in `Satisfied`, and only ever the
    /// balances that were actually read.
    pub(crate) satisfied_balances: Option<BTreeMap<u32, u128>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) satisfied_native_balance: Option<u128>,
}

impl FundingJournalRecord {
    fn open(request: &FundingRequest, now: u64) -> Self {
        Self::open_generation(request, now, 1)
    }

    pub(crate) fn open_generation(request: &FundingRequest, now: u64, generation: u32) -> Self {
        Self {
            generation,
            provider: request.provider,
            network: request.network.clone(),
            vault_address: request.vault_address.clone(),
            hot_address: request.hot_address.clone(),
            creator_pubkey: request.creator_pubkey.clone(),
            required: request.required.clone(),
            required_native: request.required_native,
            shortfall: request.shortfall.clone(),
            native_shortfall: request.native_shortfall,
            fingerprint: FundingFingerprint::of(request, vault_to_hot_native_value()),
            state: FundingState::Prepared,
            transaction_hash: None,
            pending_transaction_id: None,
            submit_attempted: false,
            evidence: None,
            created_at_unix: now,
            expires_at_unix: funding_request_deadline(now),
            last_checked_at_unix: None,
            satisfied_balances: None,
            satisfied_native_balance: None,
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        matches!(
            self.state,
            FundingState::Prepared | FundingState::Submitted | FundingState::Executed
        )
    }

    pub(crate) fn is_expired_at(&self, now: u64) -> bool {
        now > self.expires_at_unix
    }

    fn blocks_duplicate_at(&self, now: u64) -> bool {
        match self.state {
            FundingState::Prepared | FundingState::Submitted => !self.is_expired_at(now),
            // Finalized execution says the request left the Vault, not that its internal delivery
            // has reached the Hot. Keep selecting this exact amount until a Hot reading proves the
            // credit and the record is closed. The queue deadline cannot expire a transfer that
            // already executed.
            FundingState::Executed => true,
            FundingState::Expired | FundingState::Satisfied => false,
        }
    }

    fn carries_same_amount(&self, fingerprint: &FundingFingerprint) -> bool {
        self.fingerprint.value == fingerprint.value && self.fingerprint.cc == fingerprint.cc
    }

    /// A Vault-created generation may be retired only after finalized history proves how it left
    /// the queue. The submit may have reached the Vault even when its queue id was never observed.
    fn needs_reconciliation_before_close(&self) -> bool {
        if !self.provider.creates_vault_request() {
            return false;
        }
        match self.state {
            // Neither state is a finalized verdict. In particular, an earlier conservative
            // fallback must never make a now-visible submitted request safe to retire.
            FundingState::Prepared | FundingState::Submitted => true,
            // An execution match with no parseable id can come from a generation-invariant history
            // fallback. It may forbid a submit, but cannot prove this generation safe to forget.
            FundingState::Executed => {
                self.pending_transaction_id
                    .as_deref()
                    .and_then(|id| id.parse::<u64>().ok())
                    .is_none()
                    || self.evidence.is_none()
            }
            FundingState::Expired | FundingState::Satisfied => false,
        }
    }

    /// The generation a finalized execution retires, when the verdict can be bound to it.

    /// Only a parseable recorded queue id binds a `TransactionSent` to THIS generation; a
    /// generation-invariant history fallback may forbid a submit, but can never say which
    /// generation executed.
    fn retirable_generation(&self) -> Option<u32> {
        self.pending_transaction_id
            .as_deref()
            .and_then(|id| id.parse::<u64>().ok())
            .map(|_| self.generation)
    }

    /// Whether `observed` was read AFTER this generation's executed transfer reached the Hot.

    /// `Executed` is a fact about the VAULT: the message left it. The credit lands on the Hot in a
    /// later transaction, and until it does the Hot's balance is still the balance this generation
    /// was sized against - so a shortfall computed from it is the OLD shortfall, and a request for
    /// it asks the Vault for the same money a second time.

    /// Two facts, and both are needed, because they answer two different questions.

    /// IDENTITY comes first: the chain must have named the internal message that carried THIS
    /// generation's transfer to the Hot and shown the Hot's own finalized receipt for it. A balance
    /// that grew by the expected amount does not say which transfer grew it - an unrelated incoming
    /// transfer of the same size produces exactly the same reading - and it is identity, not size,
    /// that says the executed generation can be retired. Without it the credit is unproven and the
    /// window is still open.

    /// SUFFICIENCY comes second, and it is about the READING rather than the chain: the next
    /// generation is sized from the balance this run already observed, and that observation may
    /// pre-date the credit even when the credit is proven. The record carries both halves needed to
    /// tell: the balance the shortfall was computed from (`required` minus `shortfall`) and the
    /// amount the frozen transfer carries. A reading showing at least their sum is a reading the
    /// credit has reached; anything less is the window.
    fn executed_delivery_is_credited(&self, observed: &HotBalances) -> bool {
        let delivered = self
            .evidence
            .as_ref()
            .is_some_and(|evidence| evidence.delivery_message_id.is_some());
        if !delivered {
            return false;
        }
        let native_before = self.required_native.saturating_sub(self.native_shortfall);
        if observed.native < native_before.saturating_add(self.fingerprint.value) {
            return false;
        }
        self.fingerprint.cc.iter().all(|(currency, carried)| {
            let required = self.required.get(currency).copied().unwrap_or_default();
            let shortfall = self.shortfall.get(currency).copied().unwrap_or_default();
            observed.get(*currency) >= required.saturating_sub(shortfall).saturating_add(*carried)
        })
    }

    /// Whether this record is parked on a transfer the chain proved EXECUTED whose credit the Hot
    /// has not shown yet. In that window there is no balance a residual can be sized from.
    fn awaits_executed_credit(&self, observed: &HotBalances) -> bool {
        matches!(self.state, FundingState::Executed)
            && self.retirable_generation().is_some()
            && !self.executed_delivery_is_credited(observed)
    }

    /// Whether this record still describes a request that may yet move money.

    /// `Prepared` and `Submitted` both do - a prepared record is the trace of a submit whose result
    /// was never observed, which is precisely the request that may be sitting in the queue.
    /// `Executed` is retired only while handling that exact finalized verdict; a later contradictory
    /// `Absent` answer must still fail closed rather than overwrite the recorded generation.
    pub(crate) fn generation_may_still_execute(&self) -> bool {
        matches!(
            self.state,
            FundingState::Prepared | FundingState::Submitted | FundingState::Executed
        )
    }

    /// What an earlier run recorded, for the probe.
    pub(crate) fn recorded_request(&self) -> RecordedRequest {
        RecordedRequest {
            generation: self.generation,
            fingerprint: self.fingerprint.clone(),
            pending_transaction_id: self.pending_transaction_id.clone(),
            transaction_hash: self.transaction_hash.clone(),
            submit_attempted: self.submit_attempted,
            created_at_unix: self.created_at_unix,
        }
    }

    /// The request this record describes, for probing the queue on a later run.

    /// A repeat must look for the request the EARLIER run may have created, which is this one - not
    /// for whatever today's shortfall happens to be. That is precisely what the journal is for:
    /// without it the destination, creator key and exact amounts of the earlier request are not
    /// recoverable, and an unrecognisable request cannot be de-duplicated.
    pub(crate) fn recorded_funding_request(&self, hot_dapp_id: String) -> FundingRequest {
        FundingRequest {
            provider: self.provider,
            network: self.network.clone(),
            vault_address: self.vault_address.clone(),
            hot_address: self.hot_address.clone(),
            hot_dapp_id,
            creator_pubkey: self.creator_pubkey.clone(),
            required: self.required.clone(),
            required_native: self.required_native,
            shortfall: self.shortfall.clone(),
            native_shortfall: self.native_shortfall,
        }
    }
}

/// Versioned on-disk container for all requests associated with one `(network, Hot)` pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FundingJournal {
    version: u32,
    requests: Vec<FundingJournalRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FundingJournalSelection {
    Existing(FundingJournalRecord),
    Prepared(FundingJournalRecord),
}

fn funding_request_deadline(created_at_unix: u64) -> u64 {
    created_at_unix.saturating_add(dexdo_core::params::VAULT_FUNDING_REQUEST_LIFETIME.as_secs())
}

/// The final native vmshell floor this money path requires the Hot to hold.

/// The specification fixes that a single `submitTransaction` carries both halves of the shortfall -
/// the exact native vmshell shortfall in `value`, SHELL in `cc[2]`. The floor is what the money path
/// can ATTACH out of the Hot before it next reads a balance, and an already-held native balance is
/// subtracted rather than transferred again.

/// It is built from three canonical facts, and it is the whole of what the two submits take:

/// > [`dexdo_core::params::NOTE_DEPLOY_WALLET_SUBMITS`] submits x ([`dexdo_core::params::NOTE_DEPLOY_SUBMIT_NATIVE_VALUE`] attached +
/// > [`dexdo_core::params::WALLET_SUBMIT_NATIVE_FEE_BOUND_RAW`] fee)

/// A fresh `note deploy` submits twice - the deposit voucher and the SHELL gas voucher - and EACH of
/// those submits takes two separate amounts out of the Hot: the value it attaches, which never
/// returns (`RootPN.generateVoucher` accepts the message and sends no change back), and the fee its
/// own transaction charges, which `flag: 1` pays from the wallet's balance rather than out of the
/// amount being sent. A floor counting either half alone leaves the Hot able to start the deploy and
/// unable to finish it: it stops after the first voucher with the deposit already spent and a halo2
/// proof already made.

/// Two deliberate choices, so that neither is quietly optimized back later.

/// **Over-funding is preferred to under-funding.** The fee half is an upper bound rather than the
/// fee itself, and the floor is sized to the LARGEST of the paths sharing this mechanism rather than
/// to each separately - the funding step runs before either command reads its recovery file, so it
/// cannot know whether this is a fresh deploy with two submits left or a resumed one with one. What
/// it can know is the most the path can spend. Every raw unit of the difference moves the operator's
/// own money into the operator's own Hot, which is not a loss; being short stops a money path that
/// has already paid for its first leg, which is.

/// **It never exceeds the recipe the operator was already told to send.** The figure is exactly the
/// "sends" half of [`dexdo_core::params::OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE`]'s own budget - that budget is one
/// wallet deploy plus these same two submits - so asking a Hot to reach this floor can never ask for
/// more native than `note wallet` already funds a fresh wallet with.

/// moved the arithmetic itself to [`dexdo_core::params::FUNDING_WALLET_NATIVE_FLOOR_RAW`],
/// where the same three constants now produce it once. The two were always the same figure for the
/// same reason -- what a funding wallet's own outgoing messages cost it -- and the other commands
/// that spend a funding wallet need to state it too. Restating the product here would let the floor
/// this gate waits for and the floor those commands report drift apart.
pub(crate) fn vault_to_hot_native_value() -> u128 {
    dexdo_core::params::FUNDING_WALLET_NATIVE_FLOOR_RAW
}

/// The journal file name for one Hot: `sha256(network, hot_address)` in hex.

/// The specification writes the key as `sha256(network + hot_address)`. The two INPUTS are what it
/// fixes; a bare concatenation of them is not injective, because a network name may end in hex and
/// a canonical address begins with it, so two different (network, Hot) pairs could in principle
/// share a file. A NUL separator cannot occur in either input and makes the encoding injective,
/// which is the property the key is relied on for: one file per Hot, and never one file for two.
pub(crate) fn funding_journal_key(network: &str, hot_address: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(network.as_bytes());
    hasher.update([0u8]);
    hasher.update(hot_address.as_bytes());
    hex::encode(hasher.finalize())
}

/// `<data-dir>/wallet/funding-requests`.
pub(crate) fn funding_requests_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("wallet").join("funding-requests")
}

/// `<data-dir>/wallet/funding-requests/<key>.json`.
pub(crate) fn funding_journal_path(data_dir: &Path, network: &str, hot_address: &str) -> PathBuf {
    funding_requests_dir(data_dir).join(format!(
        "{}.json",
        funding_journal_key(network, hot_address)
    ))
}

/// Create the journal directory tree with owner-only permissions.
pub(crate) fn ensure_funding_requests_dir(data_dir: &Path) -> Result<PathBuf> {
    let dir = funding_requests_dir(data_dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow!("create funding journal directory {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [data_dir.join("wallet"), dir.clone()] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).map_err(
                |e| anyhow!("restrict funding journal directory {}: {e}", path.display()),
            )?;
        }
    }
    Ok(dir)
}

/// Read every record for one Hot, migrating the version-2 single-record shape in memory.
pub(crate) fn load_funding_journal_records(
    data_dir: &Path,
    network: &str,
    hot_address: &str,
) -> Result<Option<Vec<FundingJournalRecord>>> {
    let path = funding_journal_path(data_dir, network, hot_address);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => bail!("read funding journal {}: {e}", path.display()),
    };
    // The version is read BEFORE the rest, and that order matters. A record written by a newer
    // client will not have this client's shape, so deserializing first turns "a version I do not
    // understand" into "corrupt JSON" - and a record that looks corrupt invites deleting it, which
    // is precisely the record that may be the only trace of a Vault request already on chain.
    let mut value = serde_json::from_slice::<serde_json::Value>(&bytes).map_err(|e| {
        anyhow!(
            "funding journal {} is not valid JSON: {e}. Do not delete or rewrite it: it may be the \
             only local trace of a funding request that is already on chain.",
            path.display()
        )
    })?;
    let version = value.get("version").and_then(serde_json::Value::as_u64);
    match version {
        Some(2) => {
            let created_at_unix = value
                .get("created_at_unix")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| {
                    anyhow!(
                        "funding journal {} version 2 has no readable created_at_unix; refusing to \
                         migrate it. Do not delete it: it may be the only local trace of a funding \
                         request that is already on chain.",
                        path.display()
                    )
                })?;
            let object = value.as_object_mut().ok_or_else(|| {
                anyhow!(
                    "funding journal {} version 2 is not a JSON object; refusing to migrate it. Do \
                     not delete it: it may be the only local trace of a funding request that is \
                     already on chain.",
                    path.display()
                )
            })?;
            object.remove("version");
            object.insert(
                "expires_at_unix".to_string(),
                serde_json::Value::from(funding_request_deadline(created_at_unix)),
            );
            let record: FundingJournalRecord = serde_json::from_value(value).map_err(|e| {
                anyhow!(
                    "funding journal {} version 2 cannot be migrated safely: {e}. Do not delete it: \
                     it may be the only local trace of a funding request that is already on chain.",
                    path.display()
                )
            })?;
            return Ok(Some(vec![record]));
        }
        Some(version) if version == u64::from(FUNDING_JOURNAL_VERSION) => {}
        Some(version) => bail!(
            "funding journal {} has version {version} but this client understands {}; refusing to \
             act on a record it cannot read. Do not delete it: it may be the only local trace of a \
             funding request that is already on chain.",
            path.display(),
            FUNDING_JOURNAL_VERSION
        ),
        None => bail!(
            "funding journal {} has no readable version field; refusing to act on it. Do not \
             delete it: it may be the only local trace of a funding request that is already on \
             chain.",
            path.display()
        ),
    }
    let journal: FundingJournal = serde_json::from_value(value).map_err(|e| {
        anyhow!(
            "funding journal {} is not valid version-3 JSON: {e}",
            path.display()
        )
    })?;
    for record in &journal.requests {
        let expected = funding_request_deadline(record.created_at_unix);
        if record.expires_at_unix != expected {
            bail!(
                "funding journal {} records expires_at_unix={} for generation {} but the canonical \
                 deadline is {expected}; refusing to act on an inconsistent request",
                path.display(),
                record.expires_at_unix,
                record.generation
            );
        }
    }
    Ok(Some(journal.requests))
}

/// Compatibility view used by single-request reconciliation tests and call sites.

/// Production selection reads the complete array through [`load_funding_journal_records`].
pub(crate) fn load_funding_journal(
    data_dir: &Path,
    network: &str,
    hot_address: &str,
) -> Result<Option<FundingJournalRecord>> {
    Ok(
        load_funding_journal_records(data_dir, network, hot_address)?
            .and_then(|records| records.into_iter().max_by_key(|record| record.generation)),
    )
}

fn write_funding_journal(
    data_dir: &Path,
    network: &str,
    hot_address: &str,
    requests: Vec<FundingJournalRecord>,
) -> Result<()> {
    ensure_funding_requests_dir(data_dir)?;
    let path = funding_journal_path(data_dir, network, hot_address);
    let bytes = serde_json::to_vec_pretty(&FundingJournal {
        version: FUNDING_JOURNAL_VERSION,
        requests,
    })
    .map_err(|e| anyhow!("serialize funding journal: {e}"))?;
    crate::cli::note::write_private_atomic(&path, &bytes)
}

/// Insert or replace one generation atomically, owner-only.

/// Atomic and flushed, both load-bearing: a `prepared` record that a crash could lose would break
/// the ordering that repeat safety rests on, and a half-written record would be unreadable exactly
/// when it is needed most.
pub(crate) fn store_funding_journal(data_dir: &Path, record: &FundingJournalRecord) -> Result<()> {
    let mut requests =
        load_funding_journal_records(data_dir, &record.network, &record.hot_address)?
            .unwrap_or_default();
    requests.retain(|existing| existing.generation != record.generation);
    requests.push(record.clone());
    requests.sort_by_key(|request| request.generation);
    write_funding_journal(data_dir, &record.network, &record.hot_address, requests)
}

/// Select a live request with exactly the same native and ECC amounts, or atomically prepare a new
/// generation after removing locally expired records that never executed.
fn select_or_prepare_funding_request(
    data_dir: &Path,
    request: &FundingRequest,
    now: u64,
) -> Result<FundingJournalSelection> {
    let fingerprint = FundingFingerprint::of(request, vault_to_hot_native_value());
    let mut records =
        load_funding_journal_records(data_dir, &request.network, &request.hot_address)?
            .unwrap_or_default();
    ensure_no_duplicate_live_amounts(&records, now)?;
    if let Some(existing) = records
        .iter()
        .find(|record| record.blocks_duplicate_at(now) && record.carries_same_amount(&fingerprint))
    {
        return Ok(FundingJournalSelection::Existing(existing.clone()));
    }

    let generation = records
        .iter()
        .map(|record| record.generation)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| anyhow!("funding journal generation overflow"))?;
    records.retain(|record| {
        matches!(record.state, FundingState::Executed) || !record.is_expired_at(now)
    });
    let prepared = FundingJournalRecord::open_generation(request, now, generation);
    records.push(prepared.clone());
    records.sort_by_key(|record| record.generation);
    ensure_no_duplicate_live_amounts(&records, now)?;
    write_funding_journal(data_dir, &request.network, &request.hot_address, records)?;
    Ok(FundingJournalSelection::Prepared(prepared))
}

fn ensure_no_duplicate_live_amounts(records: &[FundingJournalRecord], now: u64) -> Result<()> {
    for (index, left) in records.iter().enumerate() {
        if !left.blocks_duplicate_at(now) {
            continue;
        }
        if let Some(right) = records[index + 1..].iter().find(|right| {
            right.blocks_duplicate_at(now) && right.carries_same_amount(&left.fingerprint)
        }) {
            bail!(
                "funding journal contains two live requests with exactly the same amount \
                 (generations {} and {}); refusing to create or select another request",
                left.generation,
                right.generation
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The per-Hot lock
// ---------------------------------------------------------------------------------------------

/// Serializes the final balance check and the spend that follows it, per Hot.

/// Without it two commands read the same sufficient balance at the same moment and both go on to
/// spend it.

/// In PRODUCTION this lock is not taken here. `note deploy` and `note topup` have shared one
/// funding-wallet lock since, taken before either reads anything, and they reach this module
/// through [`ensure_hot_funded_with_turn`] with [`HotTurn::AlreadyHeldByCaller`]. A second lock
/// around the same spend would serialize nothing the first does not and would deadlock the moment
/// the two keys were ever unified. The acquisition below is what a caller that holds no turn of its
/// own uses.

/// Mechanism. An OS advisory lock (`fs2`), the same call `acquire_seller_pool_lock`, the pool write
/// lock and the note-deploy prover lock already use. Advisory rather than a `create_new`
/// sentinel because of the cancel requirement: the kernel releases an advisory lock when the holder
/// dies however it died, including under SIGKILL where no `Drop` runs, so an interrupted run cannot
/// leave behind a lock that changes what the next run does. The lock FILE is left in place on
/// release, deliberately - an unlocked file carries no meaning and re-creating it costs a syscall
/// that would only add a window in which two processes both think they created it.
// Exercised by this module's own tests, not by production: both money commands reach the mechanism
// with the funding-wallet turn of already held, so nothing in production takes this second
// lock. It is kept because it is the only turn a caller that holds none of its own can take.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(crate) struct HotLock {
    path: PathBuf,
    file: std::fs::File,
}

impl HotLock {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for HotLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

/// `<data-dir>/wallet/funding-requests/<key>.lock` - the journal's key, so the two name one Hot.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn hot_lock_path(data_dir: &Path, network: &str, hot_address: &str) -> PathBuf {
    funding_requests_dir(data_dir).join(format!(
        "{}.lock",
        funding_journal_key(network, hot_address)
    ))
}

fn hot_lock_is_contended(error: &std::io::Error) -> bool {
    error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

/// Take the per-Hot lock, waiting up to `timeout`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn acquire_hot_lock(
    data_dir: &Path,
    network: &str,
    hot_address: &str,
    timeout: Duration,
    poll: Duration,
) -> Result<HotLock> {
    ensure_funding_requests_dir(data_dir)?;
    let path = hot_lock_path(data_dir, network, hot_address);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(|e| anyhow!("open hot wallet lock {}: {e}", path.display()))?;
    let started = std::time::Instant::now();
    let mut announced = false;
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(HotLock { path, file }),
            Err(e) if hot_lock_is_contended(&e) => {
                if started.elapsed() >= timeout {
                    bail!(
                        "hot wallet busy: another dexdo command is spending from Hot \
                         {hot_address}; waited {}s for {}. Retry after that command reaches a \
                         terminal state.",
                        started.elapsed().as_secs(),
                        path.display()
                    );
                }
                if !announced {
                    eprintln!(
                        "hot funding: Hot {hot_address} is already in use locally; waiting for {} \
                         (timeout {}s)",
                        path.display(),
                        timeout.as_secs()
                    );
                    announced = true;
                }
                let remaining = timeout.saturating_sub(started.elapsed());
                std::thread::sleep(poll.min(remaining).max(Duration::from_millis(1)));
            }
            Err(e) => bail!("try lock hot wallet {}: {e}", path.display()),
        }
    }
}

pub(crate) fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether a read failed because the chain never answered, as opposed to answering something.

/// The single shared reader ([`dexdo_core::chain::retry_transient_read`]) is the thing that
/// decides which transport failures are worth repeating; when its own budget for ONE read runs out
/// it marks the error with [`dexdo_core::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX`]. That marker is the
/// whole discrimination, and it is deliberately the shared helper's verdict rather than a second
/// opinion formed here: "no answer yet" is the only failure a longer wait can change.

/// Everything without the marker is an answer and stays final - a parameter encoding fault, an
/// account that is not Active, a malformed address. Waiting ten minutes to be told the same thing
/// is the failure mode this predicate exists to avoid.
fn read_got_no_answer(error: &anyhow::Error) -> bool {
    format!("{error:#}").contains(dexdo_core::CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX)
}

/// Leave the wait carrying the funding state, so the machine error envelope can name it.

/// Wrapped so the failure keeps BOTH things a failed money command is judged on.

/// The first shape of this rendered the error to a string and rebuilt around it
/// (`anyhow::Error::new(state).context(format!("{error:#}"))`). That was a defect: `classify_error`
/// picks the machine code by downcasting the causes - `DexdoError` for `E_GATEWAY_UNREACHABLE` /
/// `E_GATEWAY_WRONG_ENDPOINT`, `DealHandleSchemaTooNew`, `ChainError` - and flattening threw every
/// one of them away. Adding a field to the envelope while silently moving `code` is not a fix.

/// The obvious repair, `error.context(state)`, restores the causes but makes the STATE the error's
/// `Display`, and four accepted tests read the operator's message through `to_string()`. So the
/// state travels in a wrapper that renders as its own source and keeps that source as a real cause:
/// `to_string()` is still the operator's message, and every typed cause is still downcastable.

/// Only the stable event travels - no address, no provider response, no local path, no key
/// material.

/// `notice` is `None` when this run has not arranged anything yet, and then nothing is attached:
/// an absent `funding_notice` means "no funding request of this run exists", which is an answer in
/// its own right and must not be confused with `already_funded`.
fn carrying_funding_state(error: anyhow::Error, notice: Option<&FundingNotice>) -> anyhow::Error {
    let Some(notice) = notice else {
        return error;
    };
    crate::cli::machine::FundingContext::wrap(notice.machine_notice(), error)
}

// ---------------------------------------------------------------------------------------------
// ensure_hot_funded
// ---------------------------------------------------------------------------------------------

/// What the mechanism did about the shortfall, for the caller's output.

/// Returned rather than printed so a machine-readable caller can put it in a structured field and
/// leave stdout alone; the human line this module writes goes to stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FundingNotice {
    /// The Hot already held enough. No request, no wait.
    AlreadyFunded,
    /// A Vault request was created and is proven to be in the queue.
    RequestSubmitted,
    /// A request created by an earlier run is still in the queue. No second one was created.
    RequestAlreadyPending,
    /// An earlier request is proven to have EXECUTED. The transfer left the Vault; nothing was
    /// submitted, because a second one here would be the double transfer.
    RequestExecuted { evidence: FundingEvidence },
    /// An earlier submit's result cannot be established, so nothing was submitted. The operator is
    /// asked to re-run for reconciliation.
    RequestIndeterminate { reason: String },
    /// This provider has no request to create; the operator tops the Hot up themselves.
    ManualTopUpRequested,
}

impl FundingNotice {
    /// Stable, secret-free machine form. Free-form provider reasons/evidence stay out of stdout.
    pub(crate) fn machine_notice(&self) -> crate::cli::machine::MachineFundingNotice {
        use crate::cli::machine::MachineFundingNotice;
        match self {
            Self::AlreadyFunded => MachineFundingNotice::AlreadyFunded,
            Self::RequestSubmitted => MachineFundingNotice::RequestSubmitted,
            Self::RequestAlreadyPending => MachineFundingNotice::RequestAlreadyPending,
            Self::RequestExecuted { .. } => MachineFundingNotice::RequestExecuted,
            Self::RequestIndeterminate { .. } => MachineFundingNotice::RequestIndeterminate,
            Self::ManualTopUpRequested => MachineFundingNotice::ManualTopUpRequested,
        }
    }

    /// What the wait is on, and whether the OPERATOR is the one who has to act.

    /// The display said "waiting for you to confirm the Vault -> Hot transfer" on every pass of the
    /// wait, whatever the arrangement had actually done. On the path where finalized history shows
    /// an executed transfer that no recorded queue id binds to a generation, nothing was submitted
    /// on purpose -- the conservative branch that stops a double transfer, and it stays. The
    /// operator who opened the wallet found an empty pending list and waited out the whole budget
    /// for a confirmation that was never asked for.

    /// The second half of the answer is not cosmetic: `needs_you` is the client saying it is
    /// stopped ON the operator. Where it is waiting on the chain instead, that is a different fact,
    /// and someone who walked away has to be able to tell the two apart.
    fn wait_step(&self) -> (&'static str, bool) {
        match self {
            Self::RequestSubmitted | Self::RequestAlreadyPending => (AWAITING_CONFIRMATION, true),
            Self::ManualTopUpRequested => (AWAITING_MANUAL_TOP_UP, true),
            Self::RequestExecuted { .. } => (AWAITING_EXECUTED_CREDIT, false),
            Self::RequestIndeterminate { .. } => (AWAITING_UNRESOLVED_SUBMIT, false),
            // Never reaches the wait -- a met requirement returns before it. Answered rather than
            // left to panic: a later caller that does reach it gets a true line.
            Self::AlreadyFunded => (AWAITING_HOT_BALANCE, false),
        }
    }
}

/// Render the Acki confirmation notice under the already accepted ANSI policy.

/// `stderr_is_terminal` names the stream this line is written to. Passing the two facts in keeps
/// all three policy outcomes functional-testable without mutating process-wide environment state.
fn render_ackinacki_funding_notice(
    message: &str,
    stderr_is_terminal: bool,
    no_color: bool,
) -> String {
    // The notice says a request is out and the next move is the operator's -- it is a call to act,
    // so it is drawn like every other one in the client: amber, and bold. It used to be its own
    // `\x1b[33m`, a yellow that belonged to no role and matched nothing else on the screen.
    crate::cli::style::action(
        crate::cli::style::Palette::resolved(stderr_is_terminal, no_color),
        message,
    )
}

/// Say that the funding request is out and the next move is the operator's.

/// Where the running command shows a checklist, this IS one of its ticks: the request being sent is
/// a step passed, and printing it as its own framed paragraph while the line right under it already
/// says "waiting for you to confirm" states the same fact twice. Where there is no display -- every
/// other command that funds a Hot -- it stays the framed notice it has always been.
fn print_ackinacki_funding_notice(message: &str) {
    if crate::cli::progress::tick(message) {
        return;
    }
    eprintln!(
        "{}",
        render_ackinacki_funding_notice(
            message,
            // The operator's screen as it was, not descriptor 2 as it is: `note deploy` points that
            // at the prover's fold, and this notice is drawn under it.
            crate::cli::interaction::screen_is_terminal(),
            crate::cli::no_color_requested(),
        )
    );
}

/// Bounds for the wait. Separate from the call so a test can drive Tokio's monotonic clock.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FundingWaitBounds {
    pub(crate) timeout: Duration,
    pub(crate) poll: Duration,
    pub(crate) lock_timeout: Duration,
    pub(crate) lock_poll: Duration,
}

impl Default for FundingWaitBounds {
    fn default() -> Self {
        Self {
            timeout: dexdo_core::params::HOT_FUNDING_TIMEOUT,
            poll: dexdo_core::params::HOT_FUNDING_POLL_INTERVAL,
            lock_timeout: Duration::from_secs(dexdo_core::params::HOT_FUNDING_LOCK_TIMEOUT_SECS),
            lock_poll: dexdo_core::params::HOT_FUNDING_LOCK_POLL_INTERVAL,
        }
    }
}

/// The operator's funding window ended while a post-receipt Hot read was still in flight.
#[derive(Debug)]
struct FundingBalanceReadTimedOut {
    evidence: FundingEvidence,
}

impl std::fmt::Display for FundingBalanceReadTimedOut {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the funding wait ended during the post-receipt Hot balance read")
    }
}

impl std::error::Error for FundingBalanceReadTimedOut {}

/// A post-receipt Hot read got no chain answer while the funding window remains open.
#[derive(Debug)]
struct FundingBalanceReadNeedsRetry {
    evidence: FundingEvidence,
}

impl std::fmt::Display for FundingBalanceReadNeedsRetry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the post-receipt Hot balance read got no chain answer")
    }
}

impl std::error::Error for FundingBalanceReadNeedsRetry {}

/// Who holds the Hot's turn while this runs.

/// The specification requires the final balance check and the spend that follows it to be
/// serialized per Hot. It does not require that THIS module be the thing that serializes them, and
/// in production it is not: both money commands take the funding-wallet lock they have shared since
/// before they read anything at all, so the turn is already held by the time the funding flow
/// starts. Passing that fact in - rather than taking a second lock on a second key - is what keeps
/// one Hot under one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HotTurn {
    /// Take the per-Hot lock here and hold it inside the returned [`FundedHot`].
    #[cfg_attr(not(test), allow(dead_code))]
    AcquireOwn,
    /// The caller already holds it. Nothing is locked here, and the caller must keep holding it
    /// until after it has spent.
    AlreadyHeldByCaller,
}

/// Everything one call to [`ensure_hot_funded`] needs about the caller.

/// The specification writes the entry point as `ensure_hot_funded(binding, requirements,
/// operation)`; the other three - who is asking on chain, where its durable state lives, and how
/// long it may wait - are the same for the whole call and travel together rather than as loose
/// positional arguments a caller could transpose.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HotFundingContext<'a> {
    /// The active wallet binding. Its `provider` selects the funding flow and is never guessed.
    pub(crate) binding: &'a WalletBinding,
    /// The exact need, per currency, computed by the calling operation.
    pub(crate) requirements: &'a FundingRequirements,
    /// The operation's name, for the operator-facing messages ("note deploy", "note topup").
    pub(crate) operation: &'a str,
    /// The public key of the agent that creates a Vault request, recorded in the journal so a later
    /// run can recognise its own request in the queue.
    pub(crate) creator_pubkey: &'a str,
    /// The effective data directory. The journal and the per-Hot lock live under it.
    pub(crate) data_dir: &'a Path,
    pub(crate) bounds: FundingWaitBounds,
}

/// A Hot proven to hold enough, with the per-Hot turn still held.

/// When this module took the lock, it is inside on purpose. The specification asks for the final
/// check AND the spend to be serialized; handing back a value that proves the check while releasing
/// the lock would serialize only the check, and two commands would still spend the same balance.
/// The caller spends while it holds this and drops it afterwards. When the CALLER already held the
/// turn there is nothing here to hold, and the same rule falls on the caller's own lock.
#[derive(Debug)]
pub(crate) struct FundedHot {
    _lock: Option<HotLock>,
    /// The balances the final check actually read.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) observed: HotBalances,
    /// What was done about the shortfall on the way here.
    pub(crate) notice: FundingNotice,
}

/// Ensure the Hot holds `requirements`, taking the Hot's turn here.

/// Production does not use this form - see [`HotLock`] - and reaches
/// [`ensure_hot_funded_with_turn`] holding the turn it already took.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn ensure_hot_funded<R, P>(
    context: &HotFundingContext<'_>,
    reader: &R,
    provider: &P,
) -> Result<FundedHot>
where
    R: HotBalanceReader,
    P: HotFundingProvider,
{
    ensure_hot_funded_with_turn(context, HotTurn::AcquireOwn, reader, provider).await
}

/// Ensure the Hot holds `requirements`, arranging a top-up through the binding's provider if not.

/// The eight steps of the specification, in order: the caller has computed its exact need; this
/// reads the Hot's balances; if they suffice it returns; otherwise it selects the flow by
/// `binding.provider`, creates a provider-supported request or points the operator at a manual
/// top-up, waits for the balances to reach the required level, re-checks immediately before the
/// caller spends, and only then returns.

/// # The invariant a repeat rests on

/// **For one `(network, Hot)` pair, two live requests with exactly the same native value and ECC
/// map never coexist.** Different exact amounts are independent and may be live together.

/// The per-Hot lock serializes selection and persistence. A matching request is live through its
/// exact UTC deadline and is reused. If no live exact match exists, expired records are removed and
/// a new `prepared` record is written and flushed BEFORE the submit, so a spend that lands while
/// this client never learns it did still leaves a durable trace. Queue or history failures never
/// authorize a duplicate of that same live amount.

/// The record is also what makes the probe possible at all: it carries the frozen fingerprint - the
/// creator key, destination, DApp, value, currencies, flags, bounce and payload hash - of the
/// request an earlier run may have created, and without those the request in the queue is not
/// recognisable as ours.

/// The path is pinned too. An open record carries the provider that opened it; if the binding now
/// names a different one, this refuses rather than resuming down the other provider's flow, because
/// a request the first provider created may still be pending and this run cannot reason about it.

/// # What closes the journal

/// Only an observed on-chain balance that meets every requirement. Not a timeout, not a `Ctrl-C`,
/// not a provider's own answer, and not a chain read that failed.
pub(crate) async fn ensure_hot_funded_with_turn<R, P>(
    context: &HotFundingContext<'_>,
    turn: HotTurn,
    reader: &R,
    provider: &P,
) -> Result<FundedHot>
where
    R: HotBalanceReader,
    P: HotFundingProvider,
{
    let HotFundingContext {
        binding,
        requirements,
        operation,
        creator_pubkey,
        data_dir,
        bounds,
    } = *context;

    if provider.provider() != binding.provider {
        bail!(
            "wallet funding provider mismatch: the binding names {} but the funding flow supplied \
             is {}; refusing to fund Hot {} through a provider it is not bound to",
            binding.provider.as_str(),
            provider.provider().as_str(),
            binding.hot_address
        );
    }
    let hot = binding.hot()?;
    let hot_address = hot.to_string();
    let network = binding.network.clone();
    let vault_address = binding
        .vault()?
        .map(|vault| vault.to_string())
        .or_else(|| binding.vault_address.clone());

    let started = tokio::time::Instant::now();
    let funding_deadline = started + bounds.timeout;
    let mut arranged = false;
    // The request this invocation selected or created. Other amounts may be live for the same Hot,
    // but they belong to other commands and must neither block nor be retired by this one.
    let mut active_generation: Option<u32> = None;
    // Two questions, and reusing one flag for both is what turned out to be.

    // `arranged` answers "may this pass size a residual from a balance it read?" -- loop control.
    // `notice_is_this_run_s` answers "did this run arrange anything, so is there an ending to
    // report?" -- what the operator and a runtime are told.

    // They agree everywhere except the one path this issue is about: a generation the chain proved
    // EXECUTED whose credit the Hot has not shown yet clears `arranged` deliberately (nothing to
    // size a residual from) while an arrangement HAS happened. Gating the notice on `arranged`
    // therefore reported "no funding request of this run exists" for a run that had one, and the
    // timeout refusal fell to its default arm -- "Confirm the Vault -> Hot transfer" -- which is
    // the sentence this issue exists against.
    let mut notice_is_this_run_s = false;
    let mut notice = FundingNotice::AlreadyFunded;
    // the last balance this run actually read. The timeout message names what is still
    // missing, and that figure has to come from a reading that happened - so when the operator's
    // window runs out INSIDE a read, the wait reports the last real one rather than inventing a
    // fresh number or degrading into a transient chain error.
    let mut last_observed: Option<HotBalances> = None;

    loop {
        // Everything that reads the balance for a decision, and everything that writes the journal,
        // happens while the Hot's turn is held. Taking it only around the spend would leave the
        // check itself unserialized, which is the race it exists to close.
        let lock = match turn {
            HotTurn::AcquireOwn => Some(acquire_hot_lock(
                data_dir,
                &network,
                &hot_address,
                bounds.lock_timeout,
                bounds.lock_poll,
            )?),
            HotTurn::AlreadyHeldByCaller => None,
        };

        // (rymkapro, 2026-08-17): `--funding-timeout` is the operator's whole window, so no
        // single read may outlive what is left of it. The budget used to be consulted only AFTER a
        // read returned, and `hot_balances` carries its own retry budget of up to forty-five
        // seconds; a read begun just before the deadline therefore ran past it, and when its inner
        // retry ran out it produced a transient read error that took the arm below instead of the
        // documented timeout. Two different wrongs from one missing bound: a wait longer than
        // asked for, and the wrong verdict at the end of it.

        // The first pass is deliberately unbounded. The budget check further down is placed after a
        // full check-and-arrange pass precisely so that `--funding-timeout 0` still performs one
        // check and an already-funded Hot never fails on a timeout; bounding the first read to a
        // zero remainder would take that away.
        let first_pass = last_observed.is_none();
        let balance_read_deadline = (!first_pass).then_some(funding_deadline);
        let remaining = bounds.timeout.saturating_sub(started.elapsed());
        let read = if first_pass {
            reader.hot_balances(&hot).await
        } else {
            match tokio::time::timeout(remaining, reader.hot_balances(&hot)).await {
                Ok(answer) => answer,
                // The window ran out inside the read. That is the operator's timeout, not a chain
                // fault, so it must not be reported as one -- fall through to the documented
                // timeout with the last reading this run actually took.
                Err(_elapsed) => {
                    drop(lock);
                    break;
                }
            }
        };
        let mut observed = match read {
            Ok(observed) => observed,
            // A read that never got an answer is not a verdict about the balance, and the operator
            // asked to wait for that balance for `bounds.timeout`. The shared per-read retry has
            // its own, much smaller budget - it is sized for ONE read and is right for every other
            // caller - so letting its exhaustion end the wait capped a ten-minute
            // `--funding-timeout` at forty-five seconds. Keep reading on the cadence the wait
            // already polls at, and stop the moment the operator's window is actually spent.
            Err(error) if read_got_no_answer(&error) && started.elapsed() < bounds.timeout => {
                drop(lock);
                tokio::time::sleep(bounds.poll).await;
                continue;
            }
            // Everything else is an answer: an encoding fault, an account that is not Active, a
            // rejected request. It will read the same in ten minutes, so it leaves now.
            Err(error) => {
                return Err(carrying_funding_state(
                    error,
                    notice_is_this_run_s.then_some(&notice),
                ))
            }
        };
        last_observed = Some(observed.clone());
        let mut reconciled_this_pass = false;
        if requirements.met_by(&observed) {
            if active_generation.is_none() {
                active_generation = load_funding_journal_records(data_dir, &network, &hot_address)?
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|record| record.blocks_duplicate_at(unix_now_secs()))
                    .filter(|record| {
                        record.required_native == requirements.required_native
                            && record.required == requirements.required
                    })
                    .max_by_key(|record| record.generation)
                    .map(|record| record.generation);
            }
            // A sufficient balance proves the command can spend; it says nothing about a Vault
            // request that is still confirmable. Probe a recorded queue id before retiring it. If
            // finalized history proves execution, `arrange_funding` records that evidence and this
            // closes immediately. Present/unknown stays evidence-free and the close below refuses,
            // naming the request the operator must settle from the Vault's pending list.
            let active_record = match active_generation {
                Some(generation) => load_funding_journal_records(data_dir, &network, &hot_address)?
                    .unwrap_or_default()
                    .into_iter()
                    .find(|record| record.generation == generation),
                None => None,
            };
            if let Some(record) =
                active_record.filter(FundingJournalRecord::needs_reconciliation_before_close)
            {
                let reconciliation = arrange_funding(
                    binding,
                    requirements,
                    &mut observed,
                    operation,
                    creator_pubkey,
                    &hot,
                    vault_address.clone(),
                    data_dir,
                    &mut active_generation,
                    balance_read_deadline,
                    funding_deadline,
                    reader,
                    provider,
                )
                .await;
                match reconciliation {
                    Ok(reconciled_notice) => {
                        reconciled_this_pass = true;
                        last_observed = Some(observed.clone());
                        // The first observation was sufficient, so this probe was bookkeeping for
                        // an earlier run and its notice stays out of this run's result. If the
                        // mandatory post-receipt read is no longer sufficient, however, it is the
                        // current money state and the wait must carry it.
                        if !requirements.met_by(&observed) {
                            notice = reconciled_notice;
                            notice_is_this_run_s = true;
                        }
                    }
                    Err(error) => {
                        if let Some(timed_out) = error.downcast_ref::<FundingBalanceReadTimedOut>()
                        {
                            notice = FundingNotice::RequestExecuted {
                                evidence: timed_out.evidence.clone(),
                            };
                            notice_is_this_run_s = true;
                            drop(lock);
                            break;
                        }
                        if let Some(retry) = error.downcast_ref::<FundingBalanceReadNeedsRetry>() {
                            notice = FundingNotice::RequestExecuted {
                                evidence: retry.evidence.clone(),
                            };
                            notice_is_this_run_s = true;
                            drop(lock);
                            tokio::time::sleep(bounds.poll).await;
                            continue;
                        }
                        let pending = record
                            .pending_transaction_id
                            .as_deref()
                            .unwrap_or("unknown");
                        bail!(
                            "{operation}: could not reconcile Vault -> Hot queue transaction \
                             {pending} for generation {} before retiring it ({error}). Refusing to \
                             continue while that transfer may still be confirmable. Settle it from \
                             the Vault pending list, then re-run the same command.",
                            record.generation
                        );
                    }
                }
            }
        }

        if !requirements.met_by(&observed) && !arranged && !reconciled_this_pass {
            match arrange_funding(
                binding,
                requirements,
                &mut observed,
                operation,
                creator_pubkey,
                &hot,
                vault_address.clone(),
                data_dir,
                &mut active_generation,
                balance_read_deadline,
                funding_deadline,
                reader,
                provider,
            )
            .await
            {
                Ok(arranged_notice) => notice = arranged_notice,
                Err(error) => {
                    if let Some(timed_out) = error.downcast_ref::<FundingBalanceReadTimedOut>() {
                        notice = FundingNotice::RequestExecuted {
                            evidence: timed_out.evidence.clone(),
                        };
                        notice_is_this_run_s = true;
                        drop(lock);
                        break;
                    }
                    if let Some(retry) = error.downcast_ref::<FundingBalanceReadNeedsRetry>() {
                        notice = FundingNotice::RequestExecuted {
                            evidence: retry.evidence.clone(),
                        };
                        notice_is_this_run_s = true;
                        drop(lock);
                        tokio::time::sleep(bounds.poll).await;
                        continue;
                    }
                    return Err(error);
                }
            };
            reconciled_this_pass = true;
            last_observed = Some(observed.clone());
            notice_is_this_run_s = true;
        }

        if requirements.met_by(&observed) {
            // Close only the generation this invocation selected. Other amounts belong to other
            // commands and remain independently confirmable. `observed` may be the mandatory read
            // taken after a finalized destination receipt; no pre-receipt balance reaches here.
            if let Some(generation) = active_generation {
                close_journal_if_open(data_dir, &network, &hot_address, generation, &observed)?;
            }
            return Ok(FundedHot {
                _lock: lock,
                observed,
                notice,
            });
        }

        if reconciled_this_pass {
            // A generation the chain proved EXECUTED whose credit the Hot has not shown yet leaves
            // this pass with no balance it could size a residual from, so it is not the one
            // arrangement this run is allowed. The credit - or this wait's own budget running out -
            // is what ends that window, and only a pass that reads a balance after it may size
            // anything.
            notice_is_this_run_s = true;
            arranged = !load_funding_journal_records(data_dir, &network, &hot_address)?
                .unwrap_or_default()
                .into_iter()
                .find(|record| Some(record.generation) == active_generation)
                .is_some_and(|record| record.awaits_executed_credit(&observed));
        }

        drop(lock);

        // Checked AFTER a full check-and-arrange pass, so a zero budget still performs one check
        // and an already-funded Hot never fails on a timeout.
        if started.elapsed() >= bounds.timeout {
            break;
        }
        // Reaching here means the balance is short and the request has been arranged: the client is
        // not working, it is stopped until a human confirms the Vault -> Hot transfer inside the
        // phone wallet. Said on every pass because this is a loop and the announcement belongs
        // where the waiting happens; the display ignores a repeat of what it already says, so the
        // seconds keep counting up rather than resetting each poll. On a command with no status
        // display this is a no-op, and the printed notice above remains the only word.
        // Which of the two it is comes from what the arrangement actually did, not from the fact
        // that a wait is running.
        let (waiting_on, needs_you) = notice.wait_step();
        if needs_you {
            crate::cli::progress::step_needs_you(waiting_on);
        } else {
            crate::cli::progress::step(waiting_on);
        }
        tokio::time::sleep(bounds.poll).await;
    }

    // Reached two ways, and both are the operator's window running out: the budget check above, and
    // a read that was still running when the window closed. One exit means one verdict -- before
    // the second way produced a transient chain error instead, which named the wrong culprit
    // for the same event.

    // `last_observed` is `Some` here: the first pass reads unbounded and records its answer, and the
    // bounded arm only breaks once a reading exists. The fallback keeps that reasoning honest
    // rather than resting on it - if it were ever wrong, the message says so instead of panicking.
    let Some(observed) = last_observed else {
        return Err(carrying_funding_state(
            anyhow!(
                "{operation}: timed out after {}s waiting for Hot {hot_address}, and this run never \
                 completed a single balance read, so it cannot say what is still missing. Nothing \
                 was cancelled: any Vault transfer you have already confirmed stays on chain, the \
                 wallet binding, its keys and every recovery file are untouched, and the funding \
                 journal keeps what is pending.",
                bounds.timeout.as_secs()
            ),
            notice_is_this_run_s.then_some(&notice),
        ));
    };
    let shortfall = requirements.shortfall(&observed);
    let native_shortfall = requirements.native_shortfall(&observed);
    // `arranged` is true on every path that reaches here: a met requirement returns above,
    // and an unmet one arranges before the budget is ever checked. So the timeout always
    // carries the funding state, which is the whole point of naming it here.
    // Two layers, one error, as everywhere else: the operator's two lines on top, and the timeout's
    // own words -- the whole address, the raw figures, what was left untouched -- in the chain under
    // them, where `{error:#}`, the funding state and a later reconstruction all still read them.
    Err(carrying_funding_state(
        funding_timeout_refusal(
            operation,
            &hot_address,
            bounds.timeout.as_secs(),
            native_shortfall,
            &shortfall,
            notice_is_this_run_s.then_some(&notice),
        )
        .into_error(),
        notice_is_this_run_s.then_some(&notice),
    ))
}

/// What the status display says while the wait is on the operator, not on the client.

/// Deliberately in the second person and deliberately naming the application: the operator who has
/// walked away from a `note deploy` needs to read one line and know the client is not stuck but
/// waiting for them. Measured before this existed: 147 seconds under the label `preparing`.
const AWAITING_CONFIRMATION: &str =
    "waiting for you to confirm the Vault -> Hot transfer in Acki Nacki Wallet";

/// The lines that replace it where nothing was submitted. Each names what the client is
/// waiting ON, because "waiting" alone is what let the wrong one stand for all of them.
const AWAITING_EXECUTED_CREDIT: &str =
    "waiting for the Hot to show a Vault transfer that already executed -- nothing was submitted, \
     so there is nothing to confirm";
const AWAITING_UNRESOLVED_SUBMIT: &str =
    "waiting on the Hot balance -- an earlier submit's result is unresolved, so nothing was \
     submitted";
const AWAITING_MANUAL_TOP_UP: &str = "waiting for you to top the Hot up yourself";
const AWAITING_HOT_BALANCE: &str = "waiting for the Hot balance to reach what this command needs";

/// The wait for a Vault -> Hot transfer, run out.

/// The old text was six lines and 128 hex characters of address, in raw ECC[2], with a paragraph
/// listing what had NOT been cancelled -- to say "the transfer was never confirmed; confirm it and
/// run this again".

/// The "nothing was cancelled" thought stays, as one clause. It is there because an operator read a
/// timeout as a cancellation and sent a second transfer; between wordiness and a double spend, the
/// choice is wordiness. What it does NOT need is the inventory of which files survived: that is in
/// the detail, which the log keeps.
fn funding_timeout_refusal(
    operation: &str,
    hot_address: &str,
    seconds: u64,
    native_shortfall: u128,
    shortfall: &BTreeMap<u32, u128>,
    notice: Option<&FundingNotice>,
) -> crate::cli::refusal::Refusal {
    use crate::cli::refusal::{address, how_long, shell, Refusal};

    let ecc_shell = shortfall
        .get(&dexdo_core::params::SHELL_CURRENCY_ID)
        .copied()
        .unwrap_or_default();
    // Which shortfall to say out loud: a Hot can be short of the token it trades in, of the gas its
    // own messages need, or of both, and an operator sends different things for each.
    let missing = match (ecc_shell, native_shortfall) {
        (0, 0) => "the balance it needs".to_string(),
        (ecc, 0) => format!("{} SHELL", shell(ecc)),
        (0, native) => format!("{} vmshell of gas", shell(native)),
        (ecc, native) => format!("{} SHELL and {} vmshell of gas", shell(ecc), shell(native)),
    };
    // Why the budget ran out, when the answer is not "the transfer was never confirmed".
    // The shortfall alone is true and useless: an operator who was never asked for anything reads
    // it as their own fault and goes looking in a wallet that has nothing in it.
    let (cause, do_next) = match notice {
        Some(FundingNotice::RequestExecuted { evidence }) => {
            let cause = if evidence.delivery_message_id.is_some() {
                " An earlier Vault transfer had already executed and the Hot has not shown its \
                 credit yet, so this run submitted nothing."
            } else {
                " An earlier Vault transfer had already executed but could not be bound to this \
                 request, so this run submitted nothing rather than risk a second transfer."
            };
            (
                cause,
                "There is nothing in the wallet to confirm: nothing was submitted, on purpose. \
                 Re-run the same command -- it re-reads the balance and carries on. If the Hot is \
                 still short after that, the Vault has to hold enough to cover it first.",
            )
        }
        Some(FundingNotice::RequestIndeterminate { .. }) => (
            " An earlier submit's result could not be established, so this run submitted nothing.",
            "There is nothing in the wallet to confirm: nothing was submitted while that earlier \
             submit is unresolved. Re-run the same command -- it reconciles that submit first, \
             then carries on.",
        ),
        Some(FundingNotice::ManualTopUpRequested) => (
            " This provider creates no request; the Hot is topped up by you.",
            "Top the Hot up yourself, then run the same command again -- it re-reads the balance \
             and carries on.",
        ),
        _ => (
            "",
            // Named plainly here. The clickable form lives in the interactive branch;
            // putting it in this one too would tangle two reviews over one sentence.
            "Confirm the Vault -> Hot transfer in Acki Nacki Wallet, then run the same command \
             again -- it re-reads the balance and carries on.",
        ),
    };
    Refusal::new(
        format!(
            "Hot {} is still short {missing} after {}, and nothing was cancelled.{cause}",
            address(hot_address),
            how_long(seconds)
        ),
        do_next.to_string(),
        format!(
            "{operation}: timed out after {seconds}s waiting for Hot {hot_address} to reach the \
             required balance (still missing {}). Any Vault transfer already confirmed stays on \
             chain; the wallet binding, its keys and every recovery file are untouched, and the \
             funding journal keeps what is pending.",
            render_native_and_ecc_amounts(native_shortfall, shortfall)
        ),
    )
}

/// neither the wait nor the refusal may send the operator to a wallet with nothing in it.

/// Measured on the chain on 2026-08-19. Finalized history showed an executed Vault -> Hot transfer
/// for this Hot that no recorded queue id bound to a generation, so the client deliberately
/// submitted nothing -- the conservative branch that stops a double transfer, and it stays. What it
/// TOLD the operator was another matter: the display said "waiting for you to confirm the Vault ->
/// Hot transfer in Acki Nacki Wallet" on every pass, and the timeout that ended the run said to
/// confirm it and re-run. Both name an action on a request that does not exist. The operator opened
/// the wallet, found nothing, and waited out the whole budget.
#[cfg(test)]
mod issue_1621_nothing_pending_is_not_waiting_for_you {
    use super::*;

    /// The shape of the id-less history fallback: execution established, the delivery message that
    /// would bind it to a generation not.
    fn unbindable_execution() -> FundingNotice {
        FundingNotice::RequestExecuted {
            evidence: FundingEvidence {
                verdict: "executed".to_string(),
                source: "finalized history".to_string(),
                observed_at_unix: Some(1_787_173_349),
                detail: "the Vault emitted TransactionSent for queue transaction \
                         3380883781668717591"
                    .to_string(),
                delivery_message_id: None,
            },
        }
    }

    /// The boundary is the DEMAND, not the word: a line may well mention that there is nothing to
    /// confirm -- what it may not do is address the operator as the one holding this up. Pinned as
    /// "waiting for you" plus `needs_you`, so a re-wording around the demand still holds it, and
    /// the first draft of this module does not come back: it banned the substring `confirm`
    /// outright and so failed the honest line "nothing was submitted, so there is nothing to
    /// confirm".
    #[test]
    fn the_wait_does_not_ask_the_operator_for_an_action_that_does_not_exist() {
        let (label, needs_you) = unbindable_execution().wait_step();
        assert!(
            !label.contains("waiting for you"),
            "the wait addresses the operator about a request that was never created: {label}"
        );
        assert!(
            !needs_you,
            "the display says the client is stopped on the operator, and it is not: {label}"
        );
        assert!(
            label.contains("nothing was submitted"),
            "the wait does not say why there is nothing in the wallet: {label}"
        );
    }

    /// The path where the operator IS holding it up keeps saying so. Narrowing a claim must not
    /// switch it off where it was right.
    #[test]
    fn a_request_actually_in_the_queue_still_asks_the_operator_to_confirm_it() {
        for notice in [
            FundingNotice::RequestSubmitted,
            FundingNotice::RequestAlreadyPending,
        ] {
            let (label, needs_you) = notice.wait_step();
            assert!(
                label.contains("confirm"),
                "a pending request must still be confirmed by the operator: {label}"
            );
            assert!(needs_you, "{label}");
        }
    }

    /// The verdict the run ends on. It named the shortfall -- true, and not the reason the operator
    /// had just spent the whole budget waiting.
    /// The refusal is handed the notice the run ended on, and not a flag about something else.

    /// `funding_timeout_refusal` was already right, and its own test above proves it: given
    /// `RequestExecuted` it says "nothing was submitted, on purpose". The hole was one line up, in
    /// the CALLER, and it is why this whole issue survived a passing test suite.

    /// `arranged` answers "may this pass size a residual from a balance it read?" -- loop control.
    /// It was also used to decide WHAT TO TELL THE OPERATOR, and on the one path this issue is
    /// about those two answers are opposite: a generation proved EXECUTED whose credit has not
    /// landed sets `arranged = false`, so `arranged.then_some(&notice)` handed the refusal `None`,
    /// `None` fell to the `_` arm, and the operator was told to go and confirm a transfer this run
    /// deliberately never sent. Exactly the sentence exists against.

    /// Read out of the source because the alternative is standing up a chain, a journal and a
    /// wallet binding to observe one argument. What is pinned is the argument itself: the notice
    /// goes through, and a flag about residual sizing does not stand between it and the operator.
    #[test]
    fn the_timeout_refusal_is_given_the_notice_and_not_the_residual_flag() {
        let source = include_str!("wallet_funding.rs");
        let call = source
            .split_once("Err(carrying_funding_state(")
            .expect("the wait ends in a carried funding refusal")
            .1;
        let refusal_args = call
            .split_once("        .into_error(),")
            .expect("the refusal is built before it is carried")
            .0;

        assert!(
            !refusal_args.contains("arranged.then_some(&notice)"),
            "the timeout refusal is gated on `arranged`, which answers a different question -- \
             residual sizing -- and on the EXECUTED-without-credit path it answers `false`, so the \
             operator is sent to confirm a transfer this run never submitted:\n{refusal_args}"
        );
        assert!(
            refusal_args.contains("notice_is_this_run_s.then_some(&notice)"),
            "the refusal is not given the notice this run ended on, so it cannot say which of the \
             four endings happened:\n{refusal_args}"
        );
    }

    #[test]
    fn the_timeout_names_the_state_it_ended_in_and_an_action_that_exists() {
        let hot = "f830b3800ef37e69b66ae4efd524506defd5e39491bcbb1287695559fb9f6e20";
        let mut shortfall = BTreeMap::new();
        shortfall.insert(
            dexdo_core::params::SHELL_CURRENCY_ID,
            350 * dexdo_core::params::SHELL_UNIT,
        );
        let refusal = funding_timeout_refusal(
            "note deploy",
            hot,
            600,
            0,
            &shortfall,
            Some(&unbindable_execution()),
        );

        let action = refusal.do_next();
        assert!(
            !action.contains("Confirm the Vault -> Hot transfer"),
            "the action sends the operator to a wallet with nothing in it: {action}"
        );
        assert!(
            action.contains("nothing was submitted"),
            "the action does not say why there is nothing to confirm: {action}"
        );

        let rendered = refusal.render();
        assert!(
            rendered.contains("could not be bound"),
            "the verdict still blames the balance and not the state it ended in: {rendered}"
        );
        assert!(
            rendered.contains("nothing was cancelled"),
            "the clause that stops a second transfer must survive: {rendered}"
        );
    }
}

/// the funding wait's refusal names what to do, in units the operator holds.
#[cfg(test)]
mod funding_refusal_1432_tests {
    use super::*;

    fn shortfall_of(ecc_shell: u128) -> BTreeMap<u32, u128> {
        let mut map = BTreeMap::new();
        if ecc_shell > 0 {
            map.insert(dexdo_core::params::SHELL_CURRENCY_ID, ecc_shell);
        }
        map
    }

    /// The action is the whole point: an operator who reads a timeout as "it failed" sends a
    /// second transfer. It has to say confirm-then-rerun, and it has to say nothing was cancelled.
    #[test]
    fn the_timeout_says_confirm_and_rerun_and_that_nothing_was_cancelled() {
        let refusal = funding_timeout_refusal(
            "note deploy",
            "f5d7cf2acdb781ec106701e7f02835e6625f15708e819b5822f65364f17acc2b",
            600,
            0,
            &shortfall_of(100 * dexdo_core::params::SHELL_UNIT),
            None,
        );

        let action = refusal.do_next();
        assert!(action.contains("Confirm"), "{action}");
        assert!(action.contains("run the same command again"), "{action}");

        let rendered = refusal.render_with(crate::cli::style::Palette::None);
        assert!(
            rendered.contains("nothing was cancelled"),
            "the clause that stops a second transfer: {rendered}"
        );
    }

    /// Units and lengths the operator can hold: SHELL rather than raw ECC[2], minutes rather than
    /// seconds, an address they can tell from another rather than 128 hex characters.
    #[test]
    fn the_first_line_carries_no_raw_units_and_no_whole_address() {
        let hot = "f5d7cf2acdb781ec106701e7f02835e6625f15708e819b5822f65364f17acc2b";
        let refusal = funding_timeout_refusal(
            "note deploy",
            hot,
            600,
            0,
            &shortfall_of(100 * dexdo_core::params::SHELL_UNIT),
            None,
        );
        let first = refusal
            .render_with(crate::cli::style::Palette::None)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();

        assert!(first.contains("100 SHELL"), "{first}");
        assert!(
            !first.contains("100000000000"),
            "raw ECC[2] reached the operator: {first}"
        );
        assert!(first.contains("10 minutes"), "{first}");
        assert!(!first.contains("600s"), "{first}");
        assert!(
            !first.contains(hot),
            "the whole address reached the operator: {first}"
        );
        assert!(first.contains("\u{2026}7acc2b"), "{first}");
    }

    /// A Hot short of gas and a Hot short of SHELL are different errands. Both said, and neither
    /// invented: with nothing missing the sentence does not name an amount at all.
    #[test]
    fn each_kind_of_shortfall_is_named_for_what_it_is() {
        let hot = "f5d7cf2acdb781ec106701e7f02835e6625f15708e819b5822f65364f17acc2b";
        let unit = dexdo_core::params::SHELL_UNIT;

        let gas_only =
            funding_timeout_refusal("note deploy", hot, 60, 5 * unit, &shortfall_of(0), None);
        assert!(
            gas_only
                .render_with(crate::cli::style::Palette::None)
                .contains("vmshell of gas"),
            "{}",
            gas_only.render_with(crate::cli::style::Palette::None)
        );

        let both =
            funding_timeout_refusal("note deploy", hot, 60, 5 * unit, &shortfall_of(unit), None);
        let text = both.render_with(crate::cli::style::Palette::None);
        assert!(text.contains("SHELL and"), "{text}");
        assert!(text.contains("vmshell of gas"), "{text}");

        let neither = funding_timeout_refusal("note deploy", hot, 60, 0, &shortfall_of(0), None);
        assert!(
            neither
                .render_with(crate::cli::style::Palette::None)
                .contains("the balance it needs"),
            "no amount may be invented: {}",
            neither.render_with(crate::cli::style::Palette::None)
        );
    }

    /// The detail keeps every figure the first line dropped, for whoever reconstructs the run.
    #[test]
    fn the_detail_still_carries_the_raw_figures() {
        let hot = "f5d7cf2acdb781ec106701e7f02835e6625f15708e819b5822f65364f17acc2b";
        // The record keeps every figure; the operator's two lines keep none of them. The amount is
        // stated in SHELL, as everywhere else this client says a SHELL figure -- the same number,
        // named in the unit it is spent in.
        let rendered = funding_timeout_refusal(
            "note deploy",
            hot,
            600,
            0,
            &shortfall_of(100 * dexdo_core::params::SHELL_UNIT),
            None,
        )
        .detail()
        .to_string();
        assert!(
            rendered.contains(hot),
            "the whole address is still recorded"
        );
        assert!(
            rendered.contains("600s"),
            "the exact wait is still recorded"
        );
        assert!(
            rendered.contains("100 SHELL"),
            "the amount is still recorded: {rendered}"
        );
    }
}

/// Close an open record, and only ever with the balances that were actually read.
fn close_journal_if_open(
    data_dir: &Path,
    network: &str,
    hot_address: &str,
    generation: u32,
    observed: &HotBalances,
) -> Result<()> {
    let Some(mut record) = load_funding_journal_records(data_dir, network, hot_address)?
        .unwrap_or_default()
        .into_iter()
        .find(|record| record.generation == generation)
    else {
        return Ok(());
    };
    if !record.is_open() {
        return Ok(());
    }
    if record.needs_reconciliation_before_close() {
        if let Some(pending) = record.pending_transaction_id.as_deref() {
            bail!(
                "refusing to retire funding generation {} while Vault queue transaction {pending} \
                 may still execute: the Hot balance now meets the requirement, but finalized \
                 history has not proved that the pending transfer executed. Settle it from the \
                 Vault pending list, then re-run the same command",
                record.generation
            );
        }
        // A receipt-less submit may have reached the Vault without leaving this client a queue id.
        // Keep that generation visible so a later shortfall probes it instead of signing another
        // transfer. Unlike a known live queue entry there is nothing concrete to settle from the
        // pending list, so a sufficient balance may still let the command continue; it just cannot
        // erase the unresolved generation.
        return Ok(());
    }
    record.state = FundingState::Satisfied;
    record.last_checked_at_unix = Some(unix_now_secs());
    record.satisfied_balances = Some(observed.balances.clone());
    record.satisfied_native_balance = Some(observed.native);
    store_funding_journal(data_dir, &record)
}

#[allow(clippy::too_many_arguments)]
async fn arrange_funding<R, P>(
    binding: &WalletBinding,
    requirements: &FundingRequirements,
    observed: &mut HotBalances,
    operation: &str,
    creator_pubkey: &str,
    hot: &CanonicalAddress,
    vault_address: Option<String>,
    data_dir: &Path,
    active_generation: &mut Option<u32>,
    balance_read_deadline: Option<tokio::time::Instant>,
    funding_deadline: tokio::time::Instant,
    reader: &R,
    provider: &P,
) -> Result<FundingNotice>
where
    R: HotBalanceReader,
    P: HotFundingProvider,
{
    let hot_address = hot.to_string();
    let shortfall = requirements.shortfall(observed);
    let native_shortfall = requirements.native_shortfall(observed);
    let mut today = FundingRequest {
        provider: binding.provider,
        network: binding.network.clone(),
        vault_address,
        hot_address: hot_address.clone(),
        // Finding (2): the Hot's own DApp, never the dexdo constant. See `FundingRequest`.
        hot_dapp_id: hot.dapp_id().to_string(),
        creator_pubkey: creator_pubkey.to_string(),
        required: requirements.required.clone(),
        required_native: requirements.required_native,
        shortfall: shortfall.clone(),
        native_shortfall,
    };

    let now = unix_now_secs();
    let records =
        load_funding_journal_records(data_dir, &binding.network, &hot_address)?.unwrap_or_default();
    if let Some(open) = records
        .iter()
        .find(|record| record.blocks_duplicate_at(now) && record.provider != binding.provider)
    {
        bail!(
            "{operation}: the funding journal for Hot {hot_address} has an open {} request but \
                 the wallet binding now names {}. A request created by the first provider may still \
                 be pending; refusing to continue down a different provider's flow. Resolve the \
                 pending request, or re-bind after it is settled.",
            open.provider.as_str(),
            binding.provider.as_str()
        );
    }

    if !binding.provider.creates_vault_request() {
        let open = records
            .into_iter()
            .filter(FundingJournalRecord::is_open)
            .max_by_key(|record| record.generation);
        provider
            .refresh_recorded_request(open.as_ref().map(FundingJournalRecord::recorded_request));
        // Nothing on chain to duplicate. Record the open need so the wait and a later run share the
        // same shortfall and the same reconciliation timestamp, then point the operator at the
        // provider's own top-up.
        let record = open.unwrap_or_else(|| FundingJournalRecord::open(&today, unix_now_secs()));
        store_funding_journal(data_dir, &record)?;
        *active_generation = Some(record.generation);
        eprintln!("{}", provider.manual_instruction(&today));
        // The same code the deploy prints, for the same reason: the address is 130 characters, the
        // wallet that must send is a phone, and copying it out of a terminal by hand is where a
        // line break gets swallowed. A top-up used to print the address and nothing else -- so the
        // one moment the operator had a camera in their hand was the moment we stopped offering it.

        // ONLY for an ECC[2] SHELL shortfall, and for exactly that amount. Two mistakes are being
        // avoided here, and the first version of this block made both:

        // * `native_shortfall` is a different balance from ECC[2] SHELL. A wallet transfer to an
        // already-deployed account credits ECC[2]; native vmshell is gas, and on an Active
        // account it is not what an incoming SHELL transfer touches. Asking for native and
        // labelling it `token=2` would have the operator send a currency the wait never looks
        // at, and the command would time out however much they sent. (The deploy case differs
        // because its code carries `flag=16`, which is what converts the arriving SHELL into
        // native vmshell -- measured in `ledger.md`: at flag 1 the same ECC[2] arrives as
        // ECC[2]. Being uninit is not what converts it, and this comment used to say it was.)
        // * `native_shortfall` is capped by FUNDING_WALLET_NATIVE_FLOOR_RAW, ~0.507 vmshell, so
        // `div_ceil(SHELL_UNIT).max(1)` is 1 for every possible value. The line above says
        // "short 100 SHELL" and the code beneath it would open the send screen pre-filled with
        // 1 -- and the operator, having scanned rather than read, sends 1.

        // Rounded UP to whole SHELL: the link carries a display-unit decimal, and asking for less
        // than the shortfall leaves the command waiting on money that will never be enough.
        if let Some(code) = top_up_payment_code(binding.provider, &today) {
            crate::cli::wallet_manual::write_payment_qr(
                &mut std::io::stderr(),
                &code.address,
                code.whole_shell,
                &code.network,
            );
        }
        return Ok(FundingNotice::ManualTopUpRequested);
    }

    let selected = if requirements.met_by(observed) {
        records
            .into_iter()
            .find(|record| Some(record.generation) == *active_generation)
            .map(FundingJournalSelection::Existing)
    } else {
        Some(select_or_prepare_funding_request(data_dir, &today, now)?)
    };
    let Some(selected) = selected else {
        return Ok(FundingNotice::AlreadyFunded);
    };
    *active_generation = Some(match &selected {
        FundingJournalSelection::Existing(record) | FundingJournalSelection::Prepared(record) => {
            record.generation
        }
    });
    let open = match selected {
        FundingJournalSelection::Prepared(record) => {
            return submit_prepared_request(
                data_dir,
                &today,
                operation,
                &hot_address,
                record,
                provider,
            )
            .await;
        }
        FundingJournalSelection::Existing(record) => record,
    };
    provider.refresh_recorded_request(Some(open.recorded_request()));

    // Reconcile only the live request whose native value and complete ECC map exactly match today's
    // shortfall. Other live amounts remain independently confirmable in the Vault.
    let probe_for = open.recorded_funding_request(hot.dapp_id().to_string());
    match provider.probe_existing_request(&probe_for).await? {
        RequestPresence::Present {
            transaction_hash,
            pending_transaction_id,
            chain_created_at_unix,
        } => {
            let mut record = open;
            record.state = FundingState::Submitted;
            // `Present` is the live-queue verdict. Discard any conservative no-id `Executed`
            // fallback from an earlier read; it was allowed to forbid a submit, never to describe
            // this now-identified queue entry as finalized.
            record.evidence = None;
            record.transaction_hash = transaction_hash.or(record.transaction_hash);
            record.pending_transaction_id =
                pending_transaction_id.or(record.pending_transaction_id);
            if let Some(created_at_unix) = chain_created_at_unix {
                record.created_at_unix = created_at_unix;
                record.expires_at_unix = funding_request_deadline(created_at_unix);
            }
            record.last_checked_at_unix = Some(unix_now_secs());
            store_funding_journal(data_dir, &record)?;
            print_ackinacki_funding_notice(&format!(
                "Vault -> Hot funding request was already pending; confirm it in {}.",
                crate::cli::link::wallet_app()
            ));
            Ok(FundingNotice::RequestAlreadyPending)
        }
        RequestPresence::Executed { evidence } => {
            // Finalized execution bound to this generation's recorded queue id proves that request
            // can never execute again. If its delivered amount is insufficient for today's need,
            // retire it before opening the next generation for today's exact shortfall. An id-less
            // or malformed-id history fallback remains conservative: it may forbid a submit, but
            // cannot prove which generation executed.
            let retired_generation = open.retirable_generation();
            let mut record = open;
            record.state = FundingState::Executed;
            record.evidence = Some(evidence.clone());
            record.last_checked_at_unix = Some(unix_now_secs());
            store_funding_journal(data_dir, &record)?;
            if retired_generation.is_some() && evidence.delivery_message_id.is_some() {
                // The finalized destination receipt may become visible after this turn's first Hot
                // read. Re-read under the same held Hot turn before deciding that the old
                // generation is credited, closing it, or sizing a replacement from the balance.
                // Otherwise the replacement can repeat the whole old shortfall even though its
                // transfer landed between the first read and this receipt lookup.
                let refreshed = match balance_read_deadline {
                    Some(deadline) => {
                        match tokio::time::timeout_at(deadline, reader.hot_balances(hot)).await {
                            Ok(answer) => answer,
                            Err(_elapsed) => {
                                return Err(FundingBalanceReadTimedOut {
                                    evidence: evidence.clone(),
                                }
                                .into())
                            }
                        }
                    }
                    None => reader.hot_balances(hot).await,
                };
                *observed = match refreshed {
                    Ok(observed) => observed,
                    Err(error)
                        if read_got_no_answer(&error)
                            && tokio::time::Instant::now() < funding_deadline =>
                    {
                        return Err(FundingBalanceReadNeedsRetry {
                            evidence: evidence.clone(),
                        }
                        .into())
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "{operation}: read Hot {hot_address} after finalized Vault -> Hot \
                                 delivery"
                            )
                        })
                    }
                };
                today.shortfall = requirements.shortfall(observed);
                today.native_shortfall = requirements.native_shortfall(observed);
            }
            if requirements.met_by(observed) {
                tracing::info!(
                    "{operation}: the earlier Vault -> Hot funding request for {hot_address} \
                     EXECUTED ({}), and the Hot balance already reflects enough funding. No second \
                     request was created.",
                    evidence.detail
                );
            } else {
                if retired_generation.is_some() {
                    // Execution proves the message left the VAULT, not that the Hot has been
                    // credited. Between the two the Hot still holds what this generation was sized
                    // against, so a residual computed here would be the whole old shortfall over
                    // again - and the Hot would end up holding both transfers. The Hot's balance is
                    // the destination receipt: wait for it to show the credit, and let a later pass
                    // size whatever is still missing from a reading taken after it.
                    if !record.executed_delivery_is_credited(observed) {
                        // Two different facts can be the missing one, and they read very differently
                        // to whoever is watching a Hot whose balance HAS moved. Naming the one that
                        // is actually missing is the difference between a wait an operator can
                        // follow and a refusal that looks like it is ignoring the chain.
                        let missing = if record
                            .evidence
                            .as_ref()
                            .is_some_and(|evidence| evidence.delivery_message_id.is_some())
                        {
                            "the Hot has not shown that credit yet, so the only balance this run \
                             has read is the one that generation was already sized against"
                        } else {
                            "the internal message that carried it to the Hot cannot be named from \
                             chain fact yet, so nothing says the Hot's balance holds THIS transfer \
                             rather than an unrelated one of the same size"
                        };
                        // Bookkeeping about a previous run, not this run's result: at `info` it is
                        // there for a reconstruction without being a paragraph in front of the
                        // operator.
                        tracing::info!(
                            "{operation}: the earlier Vault -> Hot funding request for \
                             {hot_address} EXECUTED ({}), but {missing}. Sizing a second request \
                             from it would ask the Vault for the same shortfall twice. Nothing was \
                             submitted; the wait continues against the Hot's own balance.",
                            evidence.detail
                        );
                        return Ok(FundingNotice::RequestExecuted { evidence });
                    }
                    tracing::info!(
                        "{operation}: the earlier Vault -> Hot funding request for {hot_address} \
                         EXECUTED ({}), the Hot has been credited with it, and the current \
                         requirement is still unmet. That finalized generation cannot execute \
                         again, so a fresh request is being created.",
                        evidence.detail
                    );
                    close_journal_if_open(
                        data_dir,
                        &binding.network,
                        &hot_address,
                        record.generation,
                        observed,
                    )?;
                    return submit_new_request(
                        data_dir,
                        &today,
                        operation,
                        &hot_address,
                        active_generation,
                        provider,
                    )
                    .await;
                }
                eprintln!(
                    "{operation}: finalized history shows an earlier Vault -> Hot transfer for \
                     {hot_address} executed ({}), but no parseable recorded queue id binds that \
                     fallback to this generation. No second request was created.",
                    evidence.detail
                );
            }
            Ok(FundingNotice::RequestExecuted { evidence })
        }
        RequestPresence::Unknown { reason } => {
            // Not proven absent, so not submittable. The record keeps whatever state the chain last
            // proved; nothing here advances it.
            eprintln!(
                "{operation}: cannot establish whether an earlier Vault -> Hot funding request for \
                 {hot_address} exists ({reason}). Not submitting another one. Re-run the same \
                 command to reconcile once the chain is readable."
            );
            Ok(FundingNotice::RequestIndeterminate { reason })
        }
        RequestPresence::Absent => {
            // Proven absent, and nothing of ours was ever queued. A generation that could still
            // execute must never be overwritten from here: `Absent` for a record already carrying a
            // request would be the provider contradicting itself, and the safe reading of a
            // contradiction is "unknown".
            if open.generation_may_still_execute() && open.pending_transaction_id.is_some() {
                let reason = format!(
                    "the queue reports no request while the journal holds pending transaction \
                         {} for generation {}",
                    open.pending_transaction_id
                        .as_deref()
                        .unwrap_or("<unknown>"),
                    open.generation
                );
                eprintln!(
                    "{operation}: refusing to create a second Vault -> Hot funding request for \
                         {hot_address}: {reason}. A live request that has been in the queue and is \
                         no longer there must be proven executed, never read as absence. Re-run the \
                         same command to reconcile."
                );
                return Ok(FundingNotice::RequestIndeterminate { reason });
            }
            if requirements.met_by(observed) {
                return Ok(FundingNotice::AlreadyFunded);
            }
            submit_new_request(
                data_dir,
                &today,
                operation,
                &hot_address,
                active_generation,
                provider,
            )
            .await
        }
    }
}

/// Write `prepared` for `generation`, flush it, and only then submit.

/// From the moment the record is on disk, a submit whose result is never observed still leaves a
/// trace - which is what the next run reconciles against. Nothing about this ordering is negotiable:
/// it is the only thing standing between "the client crashed mid-submit" and "the client has no idea
/// a transfer is queued".
async fn submit_new_request<P>(
    data_dir: &Path,
    request: &FundingRequest,
    operation: &str,
    hot_address: &str,
    active_generation: &mut Option<u32>,
    provider: &P,
) -> Result<FundingNotice>
where
    P: HotFundingProvider,
{
    match select_or_prepare_funding_request(data_dir, request, unix_now_secs())? {
        FundingJournalSelection::Prepared(record) => {
            *active_generation = Some(record.generation);
            submit_prepared_request(data_dir, request, operation, hot_address, record, provider)
                .await
        }
        FundingJournalSelection::Existing(record) => {
            *active_generation = Some(record.generation);
            print_ackinacki_funding_notice(&format!(
                "Vault -> Hot funding request was already pending; confirm it in {}.",
                crate::cli::link::wallet_app()
            ));
            Ok(FundingNotice::RequestAlreadyPending)
        }
    }
}

async fn submit_prepared_request<P>(
    data_dir: &Path,
    request: &FundingRequest,
    operation: &str,
    hot_address: &str,
    record: FundingJournalRecord,
    provider: &P,
) -> Result<FundingNotice>
where
    P: HotFundingProvider,
{
    // This generation is durably Prepared, but this process has not submitted it. Clear any older
    // generation the provider was constructed with, then inspect the exact transfer in the live
    // queue. Another host or a lost local journal may already have created it with the same
    // custodian key; only a proven absence authorizes this process to submit.
    provider.refresh_recorded_request(None);
    match provider.probe_existing_request(request).await? {
        RequestPresence::Present {
            transaction_hash,
            pending_transaction_id,
            chain_created_at_unix,
        } => {
            let Some(created_at_unix) = chain_created_at_unix else {
                let reason =
                    "the provider found the exact request in the Vault queue but supplied \
                              no finalized chain admission time for this unbound local generation"
                        .to_string();
                eprintln!(
                    "{operation}: cannot safely adopt the Vault -> Hot funding request for \
                     {hot_address} ({reason}). Not submitting another one. It remains recorded as \
                     prepared; re-run the same command to reconcile once finalized history is \
                     readable."
                );
                return Ok(FundingNotice::RequestIndeterminate { reason });
            };
            let mut record = record;
            record.state = FundingState::Submitted;
            record.transaction_hash = transaction_hash;
            record.pending_transaction_id = pending_transaction_id;
            record.created_at_unix = created_at_unix;
            record.expires_at_unix = funding_request_deadline(created_at_unix);
            record.last_checked_at_unix = Some(unix_now_secs());
            store_funding_journal(data_dir, &record)?;
            print_ackinacki_funding_notice(&format!(
                "Vault -> Hot funding request was already pending; confirm it in {}.",
                crate::cli::link::wallet_app()
            ));
            return Ok(FundingNotice::RequestAlreadyPending);
        }
        RequestPresence::Executed { evidence } => {
            let mut record = record;
            record.state = FundingState::Executed;
            record.evidence = Some(evidence.clone());
            record.last_checked_at_unix = Some(unix_now_secs());
            store_funding_journal(data_dir, &record)?;
            return Ok(FundingNotice::RequestExecuted { evidence });
        }
        RequestPresence::Unknown { reason } => {
            eprintln!(
                "{operation}: cannot establish whether a Vault -> Hot funding request for \
                 {hot_address} already exists ({reason}). Not submitting one. It remains recorded \
                 as prepared; re-run the same command to reconcile once the chain is readable."
            );
            return Ok(FundingNotice::RequestIndeterminate { reason });
        }
        RequestPresence::Absent => {}
    }

    let mut record = record;
    // Persist the boundary before entering the network call. If the process dies anywhere after
    // this write, the next run must treat the submit as possibly having reached the Vault.
    record.submit_attempted = true;
    store_funding_journal(data_dir, &record)?;

    match provider.create_request(request).await? {
        SubmitOutcome::Accepted {
            transaction_hash,
            pending_transaction_id,
        } => {
            record.state = FundingState::Submitted;
            record.transaction_hash = transaction_hash;
            record.pending_transaction_id = pending_transaction_id;
            record.last_checked_at_unix = Some(unix_now_secs());
            store_funding_journal(data_dir, &record)?;
            // Past tense where it becomes a tick, because that line stays on the screen as a
            // record of what happened; the instruction it carries is what the live line under it
            // then repeats for as long as the wait lasts. The wallet's name is a link where the
            // terminal can make one.
            print_ackinacki_funding_notice(&format!(
                "Vault -> Hot funding request sent; confirm it in {}.",
                crate::cli::link::wallet_app()
            ));
            Ok(FundingNotice::RequestSubmitted)
        }
        SubmitOutcome::Indeterminate { reason } => {
            // Left at `prepared` on purpose. The next run probes before it submits, which is
            // exactly what an unresolved submit needs.
            eprintln!(
                "{operation}: the Vault -> Hot funding request for {hot_address} was sent but its \
                 result could not be established ({reason}). It is recorded as prepared and no \
                 second request will be created. Re-run the same command to reconcile."
            );
            Ok(FundingNotice::RequestIndeterminate { reason })
        }
    }
}

/// A native vmshell amount and an ECC currency map, rendered with each half named as its own unit.

/// Both halves, always, because they are disjoint balances and every gate that reads them blocks on
/// BOTH ([`FundingRequirements::met_by`]). Native is never folded into an `ECC[N]` label: an
/// invented currency id would be a second way to say the same thing wrongly.

/// # Why there is exactly one of these

/// There were two. This module's wait-loop timeout and the provider instructions in [`providers`]
/// each rendered the same pair of values through their own implementation, and the copies drifted
/// until they disagreed about money: a live mainnet `note deploy` printed "Hot wallet... is short
/// nothing" from the provider copy, which read only the currency map, and then failed with "still
/// missing 492980000 native vmshell" from this one, which had known the figure the whole time
/// . A wallet rich in ECC[2] and low on gas is exactly the state in which that map is empty,
/// so the operator was told the sum of a missing balance was "nothing" by one renderer and told the
/// truth by the other, in one incident.

/// # Why it is not named for shortfalls

/// [`providers::describe_recorded_request`] renders a queued transfer's own amount through it,
/// which is not a shortfall at all. One name covering two meanings is how the last divergence
/// started.
fn render_native_and_ecc_amounts(native: u128, amounts: &BTreeMap<u32, u128>) -> String {
    let mut parts = Vec::new();
    if native > 0 {
        parts.push(format!("{native} raw native vmshell"));
    }
    parts.extend(amounts.iter().map(|(currency, amount)| {
        if *currency == dexdo_core::params::SHELL_CURRENCY_ID {
            format!("{} SHELL", dexdo_core::shell_amount(*amount))
        } else {
            format!("{amount} raw ECC[{currency}]")
        }
    }));
    if parts.is_empty() {
        // Unreachable from every caller. `met_by` is exactly "no native shortfall AND an empty
        // currency map", so neither a provider instruction nor the timeout below - both of which
        // run only on its negation - can hold a request with nothing missing, and a recorded
        // transfer that moves nothing was never queued. It is named as the client bug it would be
        // rather than as "nothing", which is the one reading an operator can act on and be wrong.
        // Refusing here instead would turn a rendering fault into a failed money command, and a
        // panic is not allowed on a runtime path - so this stays a total function with no new
        // branch in any caller.
        return "an amount this client failed to record (client bug: the request carries no amount)"
            .to_string();
    }
    parts.join(" and ")
}

// ---------------------------------------------------------------------------------------------
// The real chain reader
// ---------------------------------------------------------------------------------------------

#[async_trait::async_trait(?Send)]
impl HotBalanceReader for dexdo_core::ChainClient {
    async fn hot_balances(&self, hot: &CanonicalAddress) -> Result<HotBalances> {
        let address = dexdo_core::Address::parse(&hot.legacy())
            .map_err(|e| anyhow!("Hot {hot} is not a chain address: {e:?}"))?;
        let account = dexdo_core::chain::retry_transient_read(|| self.get_account(&address))
            .await
            .map_err(|e| anyhow!("read Hot {hot} balances: {e}"))?
            .ok_or_else(|| anyhow!("Hot {hot} not found on chain"))?;
        if !account.is_active() {
            bail!("Hot {hot} is not Active (acc_type={})", account.status);
        }
        Ok(HotBalances::new(account.balance, account.ecc))
    }
}

/// The production providers: one per `WalletProvider`.
pub(crate) mod providers;

// ---------------------------------------------------------------------------------------------
// The entry point the money commands call
// ---------------------------------------------------------------------------------------------

/// Ensure the bound Hot can pay for `operation`, arranging a top-up through its provider if not.

/// This is what `note deploy` and `note topup` call. Three things about its shape are deliberate.

/// **An explicit Hot is manual for this command.** `--multisig-address` wins over the binding and
/// is used exactly as resolved (`resolve_funding_wallet`). It does not create a durable binding and
/// no provider is inferred from its address: the explicit BYO path itself selects the agreed manual
/// instruction plus bounded on-chain balance wait.

/// **The Hot's turn is already held.** Both callers take the funding-wallet lock they have shared
/// since before they read anything, so the check and the spend are already serialized under
/// one key. Taking a second lock here would serialize nothing the first does not.

/// **It does not do the final check.** Step 7 of the specification - re-read the balance immediately
/// before sending - is the caller's own preflight, which runs next and still refuses on its own
/// terms. This returns once the balance HAS been observed to meet the requirement; the caller then
/// proves it again against the figure it is about to spend.
pub(crate) struct MoneyCommandFunding<'a> {
    pub(crate) client: &'a dexdo_core::ChainClient,
    pub(crate) endpoint: &'a str,
    pub(crate) binding: Option<&'a crate::cli::wallet::WalletBinding>,
    pub(crate) resolved_hot_address: &'a str,
    pub(crate) network: &'a str,
    pub(crate) requirements: FundingRequirements,
    pub(crate) operation: &'a str,
    pub(crate) funding_timeout: Option<Duration>,
}

pub(crate) async fn fund_hot_for_money_command(
    funding: MoneyCommandFunding<'_>,
) -> Result<FundingNotice> {
    use providers::{AckinackiVaultProvider, DirectTopUpProvider, RealVaultChain};

    let MoneyCommandFunding {
        client,
        endpoint,
        binding,
        resolved_hot_address,
        network,
        requirements,
        operation,
        funding_timeout,
    } = funding;

    let view = binding.map_or_else(
        || HotFundingBinding {
            provider: WalletProvider::Manual,
            network: network.to_string(),
            hot_address: dexdo_core::address::display_self_dapp(resolved_hot_address),
            vault_address: None,
        },
        HotFundingBinding::from_active,
    );
    let hot_address = view.hot()?.to_string();
    let data_dir = crate::cli::data_dir::effective()?;
    let bounds = FundingWaitBounds {
        timeout: funding_timeout.unwrap_or(dexdo_core::params::HOT_FUNDING_TIMEOUT),
        ..FundingWaitBounds::default()
    };

    // What an earlier run recorded, read under the turn this runs under, so the record the provider
    // reconciles against and the record the mechanism writes cannot be two different readings.
    let recorded = load_funding_journal(&data_dir, &view.network, &hot_address)?
        .filter(FundingJournalRecord::is_open)
        .map(|record| record.recorded_request());

    if !view.provider.creates_vault_request() {
        // No Vault, so no request and no creator: the operator tops the Hot up and the wait
        // observes it. The journal still records the open need, which is what keeps one shortfall
        // and one reconciliation timestamp across a repeat of the command.
        let provider = DirectTopUpProvider::new(view.provider)?;
        let funded = ensure_hot_funded_with_turn(
            &HotFundingContext {
                binding: &view,
                requirements: &requirements,
                operation,
                creator_pubkey: "",
                data_dir: &data_dir,
                bounds,
            },
            HotTurn::AlreadyHeldByCaller,
            client,
            &provider,
        )
        .await?;
        return Ok(funded.notice);
    }

    let active = binding.ok_or_else(|| {
        anyhow!("internal: a Vault funding view cannot come from an explicit manual Hot")
    })?;
    let vault = view.vault()?.ok_or_else(|| {
        anyhow!(
            "wallet binding {} names provider `{}` but records no Vault address, so there is \
             nothing to ask for a Hot top-up. Re-bind with `dexdo wallet rebind ackinacki-wallet`.",
            active.id,
            active.provider
        )
    })?;
    // The custodian key the Vault request is signed with. A separately generated `--vault-key` is
    // preferred when the binding retained one; otherwise it is the Hot key, which is exactly what
    // onboarding validated the Vault's custodian set against when no separate Vault key was given.
    let vault_key = active
        .vault_key_file
        .clone()
        .or_else(|| active.hot_key_file.clone());
    let vault_seed = if active.vault_key_file.is_some() {
        None
    } else {
        active.hot_seed_file.clone()
    };
    if vault_key.is_none() && vault_seed.is_none() {
        bail!(
            "wallet binding {} (provider `{}`) records no local key for Vault {vault}, so this \
             instance cannot sign the Vault -> Hot top-up request. Re-bind with a provider flow \
             that stores one.",
            active.id,
            active.provider
        );
    }
    let (source, secret_hex) = crate::cli::commands::multisig_secret_hex(&vault_key, &vault_seed)?;
    let keys = dexdo_core::KeyPair::from_secret_hex(secret_hex.trim())
        .map_err(|e| anyhow!("{source} (SDK secret hex): {e:?}"))?;
    let creator_pubkey = keys.public_hex().to_string();
    let chain = RealVaultChain::new(client, endpoint, vault, keys)?;
    let provider = AckinackiVaultProvider::new(chain, recorded);
    let funded = ensure_hot_funded_with_turn(
        &HotFundingContext {
            binding: &view,
            requirements: &requirements,
            operation,
            creator_pubkey: &creator_pubkey,
            data_dir: &data_dir,
            bounds,
        },
        HotTurn::AlreadyHeldByCaller,
        client,
        &provider,
    )
    .await?;
    Ok(funded.notice)
}

#[cfg(test)]
mod tests;

/// the states a request leaves the Vault queue through, and what may follow each.
#[cfg(test)]
mod reconciliation_tests;

/// re-audit item 8: terminal colour policy and the secret-free machine notice mapping.
#[cfg(test)]
#[path = "wallet_funding/item8_output_tests.rs"]
mod item8_output_tests;

/// re-audit items 2 and 3: the window an executed transfer has not been credited in, and the
/// native floor of a money path that attaches it more than once.
#[cfg(test)]
mod issue_334_reaudit_regressions;

/// re-audit item 2, second reading: which transfer credited the Hot is an identity the
/// aggregated balance cannot carry.
#[cfg(test)]
mod issue_334_delivery_identity;

/// multiple live amounts, exact de-duplication, local UTC expiry and journal migration.
#[cfg(test)]
mod issue_1904_multiple_requests;
