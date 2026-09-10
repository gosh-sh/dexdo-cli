//! The production [`HotFundingProvider`] implementations.

//! One per `WalletProvider`, because the specification is explicit that the funding flow is selected
//! from the binding and never inferred: `gosh-ai` and `manual` have no request to create and are
//! served by [`DirectTopUpProvider`], while `ackinacki-wallet` has a Vault and is served by
//! [`AckinackiVaultProvider`].

//! # Why the Vault provider is built around a seam

//! [`VaultChain`] is the whole of what the Acki Nacki flow needs from the chain: the live queue, the
//! finalized queue history, and the submit. It is a trait for the reason every money path in this
//! client uses one - the decisions taken from those three facts are the part that
//! costs money to get wrong, and they have to be provable offline,
//! against scripted answers, rather than only against a testnet that happens to be in the right
//! state.

use std::{cell::RefCell, collections::BTreeMap};

use anyhow::{bail, Result};

use super::{
    funding_request_deadline, payload_hash, render_native_and_ecc_amounts,
    vault_to_hot_native_value, FundingEvidence, FundingFingerprint, FundingRequest,
    HotFundingProvider, RecordedRequest, RequestPresence, SubmitOutcome, WalletProvider,
    VAULT_TO_HOT_BOUNCE, VAULT_TO_HOT_PAYLOAD, VAULT_TO_HOT_SEND_FLAGS,
};

/// The base64 of an empty BOC cell, which is how a multisig queue reports the empty payload every
/// plain currency transfer carries.
const EMPTY_PAYLOAD_CELL: &str = "te6ccgEBAQEAAgAAAA==";

// ---------------------------------------------------------------------------------------------
// gosh-ai and manual: no request to create
// ---------------------------------------------------------------------------------------------

/// The providers that have no funding request to create.

/// Gosh.ai offers no server-side top-up request and no response to wait for, and `manual` is a Hot
/// the operator already controls. Both therefore do exactly one thing when the Hot is short: say
/// what is missing, say where to send it, and let the shared mechanism wait for the balance. The
/// specification is explicit that completion is determined by the on-chain balance alone - not by an
/// HTTP response and not by the operator returning to a page.
pub(crate) struct DirectTopUpProvider {
    provider: WalletProvider,
}

impl DirectTopUpProvider {
    pub(crate) fn new(provider: WalletProvider) -> Result<Self> {
        if provider.creates_vault_request() {
            bail!(
                "provider `{}` creates an on-chain funding request and must not be served by the \
                 direct top-up flow",
                provider.as_str()
            );
        }
        Ok(Self { provider })
    }
}

#[async_trait::async_trait(?Send)]
impl HotFundingProvider for DirectTopUpProvider {
    fn provider(&self) -> WalletProvider {
        self.provider
    }

    async fn probe_existing_request(&self, _request: &FundingRequest) -> Result<RequestPresence> {
        // The shared mechanism never probes for a provider that creates no request. Answering
        // "absent" here would be a claim about a queue this provider does not have, and `Absent` is
        // the one answer that authorizes a submit.
        Ok(RequestPresence::Unknown {
            reason: format!(
                "provider `{}` has no funding request queue to probe",
                self.provider.as_str()
            ),
        })
    }

    async fn create_request(&self, _request: &FundingRequest) -> Result<SubmitOutcome> {
        bail!(
            "provider `{}` cannot create a funding request: it has no Vault. The operator tops the \
             Hot up directly and the command waits for the balance.",
            self.provider.as_str()
        )
    }

    fn manual_instruction(&self, request: &FundingRequest) -> String {
        let shortfall = render_native_and_ecc_amounts(request.native_shortfall, &request.shortfall);
        match self.provider {
            // No link here. The one that used to be printed was the onboarding
            // placeholder, and this operator reached this line BY onboarding through Gosh.ai --
            // they have the page. Where Gosh.ai lives is now the manifest's to say, and repeating
            // a copy of it in a second sentence is how the two come to disagree.
            WalletProvider::GoshAi => format!(
                "Hot wallet {} on {} is short {shortfall}. Top it up in Gosh.ai, then leave this \
                 command running: it continues by itself as soon as the balance arrives on chain.",
                request.hot_address, request.network,
            ),
            // The network is named because a QR is printed directly under this line and the
            // canonical address is the SAME STRING on both chains. Without it the operator
            // reads an address they cannot tell apart, scans, and their own wallet supplies
            // whichever network its switch happens to be set to.
            _ => format!(
                "Hot wallet {} is short {shortfall}. Send it to that address on {}; this command \
                 continues by itself as soon as the balance arrives on chain.",
                request.hot_address, request.network
            ),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The Vault seam
// ---------------------------------------------------------------------------------------------

/// One transaction resting in a Vault's `getTransactions` queue.

/// Deliberately plain owned values rather than the SDK's types: everything the reconciliation
/// decides is decided from these fields, and a seam that only exists in a chain build cannot be
/// driven by a test that proves the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QueuedTransfer {
    pub(crate) id: u64,
    pub(crate) creator_pubkey: Option<String>,
    pub(crate) dest: String,
    pub(crate) value: u128,
    pub(crate) cc: BTreeMap<u32, u128>,
    pub(crate) send_flags: u16,
    pub(crate) bounce: bool,
    pub(crate) dapp_id: String,
    /// The payload cell exactly as the queue reported it.
    pub(crate) payload: Option<String>,
}

/// One finalized queue event from the Vault's own history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VaultQueueEvent {
    pub(crate) kind: VaultQueueEventKind,
    pub(crate) transaction_id: u64,
    pub(crate) dest: String,
    pub(crate) value: u128,
    pub(crate) dapp_id: String,
    pub(crate) message_id: String,
    /// Chain time, from the finalized message that carried the event.
    pub(crate) created_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VaultQueueEventKind {
    /// The request entered the queue.
    Submitted,
    /// The request left the queue by EXECUTING.
    Sent,
}

/// Everything the Acki Nacki funding flow needs from the chain.
#[async_trait::async_trait(?Send)]
pub(crate) trait VaultChain {
    /// The Vault's live pending-transaction queue.
    async fn queue(&self) -> Result<Vec<QueuedTransfer>>;

    /// Every queue event in the Vault's finalized ext-out history, in chain order.
    async fn history(&self) -> Result<Vec<VaultQueueEvent>>;

    /// The INTERNAL message that carried an executed queue transfer to `destination`, proven by the
    /// destination's own finalized receipt.

    /// `sent_event_message_id` is a `TransactionSent` message id, and that message is an EVENT: the
    /// wallet emits it on a hardcoded event channel, so its own destination is that channel and
    /// never the transfer's. What binds them is the queued path executing `txn.dest.transfer(...)`
    /// and then `emit TransactionSent(...)` in ONE Vault transaction - so the event anchors that
    /// transaction and the delivery is its sibling out-message addressed to `destination`.

    /// `Ok(None)` is "cannot be established from chain fact", which every caller must read as
    /// unknown: no sibling, more than one, or no finalized receipt for it. It is never "no
    /// delivery", and it never authorizes anything.
    async fn delivery_message_id(
        &self,
        sent_event_message_id: &str,
        destination: &str,
        destination_dapp_id: &str,
    ) -> Result<Option<String>>;

    /// Put the transfer `fingerprint` describes into the Vault's queue.
    async fn submit(&self, fingerprint: &FundingFingerprint) -> Result<SubmitOutcome>;
}

// ---------------------------------------------------------------------------------------------
// The Acki Nacki Vault provider
// ---------------------------------------------------------------------------------------------

/// The `ackinacki-wallet` funding flow: a `submitTransaction` resting in the Vault's queue with the
/// agent's single signature IS the funding request, and the human supplies the second signature in
/// the wallet application.

/// # What this refuses to conclude

/// The request leaving the queue is NOT a fact about money. It leaves both when the human confirms
/// it - the transfer executes - and when it expires unconfirmed - the transfer never happens - and
/// the two are opposite: after the first, a second request would transfer twice; after the second, a
/// fresh request is the only way the Hot is ever funded. This provider therefore never answers
/// `Absent` for a request that has been in the queue. It answers from finalized history:

/// - a `TransactionSent` for the request's id is a POSITIVE proof of execution;
/// - `TransactionSubmitted` gives the id and the CHAIN time the request was queued at; that time is
/// mandatory when adopting a queue request for which this process has no local record;
/// - request lifetime and expiry use the journal timestamp and the one canonical client lifetime;
/// - anything else - an unreadable queue, an unreadable history, or no finalized verdict - is
/// `Unknown`.
pub(crate) struct AckinackiVaultProvider<C: VaultChain> {
    chain: C,
    /// What an earlier run recorded, read from the journal under the same held turn this runs
    /// under. `None` means no local open record; an exact request from another checkout can still be
    /// present and is adopted only after its finalized admission time is found.
    recorded: RefCell<Option<RecordedRequest>>,
}

impl<C: VaultChain> AckinackiVaultProvider<C> {
    pub(crate) fn new(chain: C, recorded: Option<RecordedRequest>) -> Self {
        Self {
            chain,
            recorded: RefCell::new(recorded),
        }
    }

    /// The frozen fingerprint of the generation being reconciled.

    /// From the record when there is one - a later run whose shortfall has moved must still look
    /// for the transfer the earlier run described - and derived from the request only when this is
    /// the first attempt.
    fn expected_fingerprint(&self, request: &FundingRequest) -> FundingFingerprint {
        self.recorded
            .borrow()
            .as_ref()
            .map(|recorded| recorded.fingerprint.clone())
            .unwrap_or_else(|| FundingFingerprint::of(request, vault_to_hot_native_value()))
    }

    fn known_id(&self) -> Option<u64> {
        self.recorded
            .borrow()
            .as_ref()
            .and_then(|recorded| recorded.pending_transaction_id.as_deref())
            .and_then(|id| id.parse::<u64>().ok())
    }
}

fn describe_recorded_request(expected: &FundingFingerprint, recorded: &RecordedRequest) -> String {
    let queue_id = recorded.pending_transaction_id.as_deref().map_or_else(
        || "without a known queue id".to_string(),
        |id| format!("queue transaction {id}"),
    );
    format!(
        "generation {} ({queue_id}), {} to {} in DApp {}, created by custodian key {}",
        recorded.generation,
        render_native_and_ecc_amounts(expected.value, &expected.cc),
        expected.dest,
        expected.dapp_id,
        expected.creator,
    )
}

/// Whether a queued transaction is the transfer `expected` describes.

/// Every field of the fingerprint, and the creator key among them: a transaction to the same
/// destination for the same amount created by a DIFFERENT custodian is not ours to de-duplicate
/// against, and treating it as ours would leave our own request never made.
pub(crate) fn queue_entry_matches(entry: &QueuedTransfer, expected: &FundingFingerprint) -> bool {
    entry
        .creator_pubkey
        .as_deref()
        .map(normalized_uint256)
        .is_some_and(|creator| creator == normalized_uint256(&expected.creator))
        && addresses_equal(&entry.dest, &expected.dest)
        && dapp_ids_equal(&entry.dapp_id, &expected.dapp_id)
        && entry.value == expected.value
        && entry.cc == expected.cc
        && entry.send_flags == expected.send_flags
        && entry.bounce == expected.bounce
        && queue_payload_hash(entry.payload.as_deref()) == expected.payload_hash
}

/// The payload hash of a queued transaction, in the fingerprint's own terms.

/// Every transfer this client creates carries the empty payload, which the queue reports either as
/// an absent cell or as the empty cell. Both are hashed as the empty payload so that the recorded
/// fingerprint and the queue agree; anything else is hashed as itself and will therefore not match,
/// which is the intended answer for a transaction that carried a body we did not send.
pub(crate) fn queue_payload_hash(payload: Option<&str>) -> String {
    match payload.map(str::trim) {
        None => payload_hash(VAULT_TO_HOT_PAYLOAD),
        Some(cell) if cell.is_empty() || cell == EMPTY_PAYLOAD_CELL => {
            payload_hash(VAULT_TO_HOT_PAYLOAD)
        }
        Some(cell) => payload_hash(cell),
    }
}

/// Address equality that does not depend on which spelling the chain answered with.
fn addresses_equal(left: &str, right: &str) -> bool {
    normalized_address(left) == normalized_address(right)
}

fn normalized_address(address: &str) -> String {
    let tail = address
        .rsplit_once("::")
        .map_or(address, |(_, account)| account);
    let tail = tail.rsplit_once(':').map_or(tail, |(_, account)| account);
    tail.trim_start_matches("0x").to_ascii_lowercase()
}

fn dapp_ids_equal(left: &str, right: &str) -> bool {
    normalized_uint256(left) == normalized_uint256(right)
}

/// A `uint256` in the one spelling this client compares them in.

/// The same normalization `note_cmd`'s `normalize_multisig_pubkey` already applies to the custodian
/// keys read out of this very contract - strip `0x`, lowercase, left-pad to 64 - because that is the
/// form the SDK renders a `uint256` getter output in, and two spellings of one DApp id comparing
/// unequal would read as "not our request".
pub(crate) fn normalized_uint256(value: &str) -> String {
    let trimmed = value.trim().trim_start_matches("0x").to_ascii_lowercase();
    format!("{trimmed:0>64}")
}

#[async_trait::async_trait(?Send)]
impl<C: VaultChain> HotFundingProvider for AckinackiVaultProvider<C> {
    fn provider(&self) -> WalletProvider {
        WalletProvider::AckinackiWallet
    }

    fn refresh_recorded_request(&self, recorded: Option<RecordedRequest>) {
        *self.recorded.borrow_mut() = recorded;
    }

    async fn probe_existing_request(&self, request: &FundingRequest) -> Result<RequestPresence> {
        let expected = self.expected_fingerprint(request);
        let known_id = self.known_id();

        let queue = match self.chain.queue().await {
            Ok(queue) => queue,
            Err(error) => {
                return Ok(RequestPresence::Unknown {
                    reason: format!("the Vault queue could not be read: {error}"),
                })
            }
        };

        // The id is the primary key once the chain has ever reported one; the fingerprint is what
        // recognises the request before that, and corroborates the id afterwards.
        let found = match known_id {
            Some(id) => queue.iter().find(|entry| entry.id == id),
            None => queue
                .iter()
                .find(|entry| queue_entry_matches(entry, &expected)),
        };
        if let Some(found) = found {
            if !queue_entry_matches(found, &expected) {
                return Ok(RequestPresence::Unknown {
                    reason: format!(
                        "the Vault queue holds transaction {} but it does not describe the \
                         transfer this journal recorded; refusing to treat it as ours",
                        found.id
                    ),
                });
            }
            let recorded = self.recorded.borrow().clone();
            let chain_created_at_unix = if known_id.is_some()
                || recorded
                    .as_ref()
                    .is_some_and(|recorded| recorded.submit_attempted)
            {
                None
            } else {
                let history = match self.chain.history().await {
                    Ok(history) => history,
                    Err(error) => {
                        return Ok(RequestPresence::Unknown {
                            reason: format!(
                                "the Vault queue holds exact transaction {}, but its finalized \
                                 history could not be read to date that request: {error}",
                                found.id
                            ),
                        })
                    }
                };
                let Some(submitted) = history.iter().rev().find(|event| {
                    event.kind == VaultQueueEventKind::Submitted
                        && event.transaction_id == found.id
                        && addresses_equal(&event.dest, &expected.dest)
                        && dapp_ids_equal(&event.dapp_id, &expected.dapp_id)
                        && event.value == expected.value
                }) else {
                    return Ok(RequestPresence::Unknown {
                        reason: format!(
                            "the Vault queue holds exact transaction {}, but finalized history has \
                             no matching TransactionSubmitted message whose chain time can date it",
                            found.id
                        ),
                    });
                };
                Some(submitted.created_at)
            };
            return Ok(RequestPresence::Present {
                transaction_hash: recorded.and_then(|recorded| recorded.transaction_hash),
                pending_transaction_id: Some(found.id.to_string()),
                chain_created_at_unix,
            });
        }

        // Nothing of ours is in the live queue. With no open record there is nothing that could
        // have been queued, so this is a first request and absence is a fact about a request that
        // was never made.
        let Some(recorded) = self.recorded.borrow().clone() else {
            return Ok(RequestPresence::Absent);
        };

        // An open record and an empty queue is exactly the ambiguous observation the specification
        // forbids acting on. Only finalized history separates the two readings.
        let history = match self.chain.history().await {
            Ok(history) => history,
            Err(error) => {
                return Ok(RequestPresence::Unknown {
                    reason: format!("the Vault's finalized history could not be read: {error}"),
                })
            }
        };

        // Recover the id when our own submit's result was never observed: the wallet's own
        // `TransactionSubmitted` names it, and carries the chain time it was queued at.
        let submitted = history
            .iter()
            .filter(|event| event.kind == VaultQueueEventKind::Submitted)
            .rfind(|event| match known_id {
                Some(id) => event.transaction_id == id,
                None => {
                    addresses_equal(&event.dest, &expected.dest)
                        && dapp_ids_equal(&event.dapp_id, &expected.dapp_id)
                        && event.value == expected.value
                }
            });

        let id = known_id.or_else(|| submitted.map(|event| event.transaction_id));

        // Executed? A `TransactionSent` for our id is a positive proof that the money left the
        // Vault, and it forbids a second request outright.
        if let Some(id) = id {
            if let Some(sent) = history
                .iter()
                .find(|event| event.kind == VaultQueueEventKind::Sent && event.transaction_id == id)
            {
                // The event proves the money left the VAULT. Which internal message carried it to
                // the Hot is a separate question, and it is the one that later decides whether a
                // replacement generation may be sized: a chain read failure there must not be
                // allowed to look like "no delivery", so it fails closed as `Unknown`.
                let delivery_message_id = match self
                    .chain
                    .delivery_message_id(&sent.message_id, &expected.dest, &expected.dapp_id)
                    .await
                {
                    Ok(delivery_message_id) => delivery_message_id,
                    Err(error) => {
                        return Ok(RequestPresence::Unknown {
                            reason: format!(
                                "the Vault emitted TransactionSent for queue transaction {id}, so \
                                 the money left the Vault, but the internal message that carried it \
                                 to {} could not be read: {error}",
                                expected.dest
                            ),
                        })
                    }
                };
                let delivery = match delivery_message_id.as_deref() {
                    Some(delivery) => format!(
                        ", delivered to the Hot by internal message {delivery}, whose destination \
                         receipt is finalized"
                    ),
                    None => String::new(),
                };
                return Ok(RequestPresence::Executed {
                    evidence: FundingEvidence {
                        verdict: "executed".to_string(),
                        source: format!("finalized ext-out message {}", sent.message_id),
                        observed_at_unix: Some(sent.created_at),
                        detail: format!(
                            "the Vault emitted TransactionSent for queue transaction {id} to {} at \
                             chain time {}{delivery}",
                            sent.dest, sent.created_at
                        ),
                        delivery_message_id,
                    },
                });
            }
        }

        // Selection reaches the provider only for a journal record that is still live by its local
        // UTC deadline. With neither a queue entry nor finalized execution, there is no additional
        // chain verdict to make here.
        Ok(RequestPresence::Unknown {
            reason: format!(
                "{} is not in the Vault queue and finalized history shows no TransactionSent for \
                 it; it remains live in the local funding journal until {}",
                describe_recorded_request(&expected, &recorded),
                funding_request_deadline(recorded.created_at_unix)
            ),
        })
    }

    async fn create_request(&self, request: &FundingRequest) -> Result<SubmitOutcome> {
        let fingerprint = FundingFingerprint::of(request, vault_to_hot_native_value());
        self.chain.submit(&fingerprint).await
    }

    fn manual_instruction(&self, request: &FundingRequest) -> String {
        format!(
            "Hot wallet {} is short {}. Confirm the pending Vault -> Hot transaction in {}.",
            request.hot_address,
            render_native_and_ecc_amounts(request.native_shortfall, &request.shortfall),
            crate::cli::link::wallet_app()
        )
    }
}

/// The `sendFlags` value the queue reports, as the `flag` argument `submitTransaction` takes.
pub(crate) fn send_flag_argument() -> Result<u8> {
    u8::try_from(VAULT_TO_HOT_SEND_FLAGS).map_err(|_| {
        anyhow::anyhow!(
            "the Vault -> Hot send flags ({VAULT_TO_HOT_SEND_FLAGS}) do not fit the \
             submitTransaction flag argument"
        )
    })
}

/// The bounce value every Vault -> Hot transfer carries, re-exported for the production chain.
pub(crate) const fn bounce_argument() -> bool {
    VAULT_TO_HOT_BOUNCE
}

mod chain;

pub(crate) use chain::RealVaultChain;

#[cfg(test)]
mod tests;

/// the native half of a shortfall is part of the instruction, and "nothing" is never it.
#[cfg(test)]
mod issue_1387_regressions;
