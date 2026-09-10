//! Real the chain backend -- an on-chain adapter on top of the **`gosh.ackinacki`
//! SDK** (`gosh_ackinacki::sdk`). The wallet/keys/chain interaction are not rewritten -- they are
//! taken from the SDK.

//! This module covers **step 1 of the START signal**:
//! connecting to the manifest-selected Block Manager endpoint and reading the deployed contracts
//! (the committed manifest). The `ChainBackend` trait implementation
//! (offer/match in `InferenceOrderBook`, probe/`advance`/`stop`/burn in `TokenContract`, notes in
//! `PrivateNote`) is layered on top of this `ChainClient` in the next step -- its money choreography
//! is verified against the real on-chain (funded keys required), so no trait
//! stubs are introduced here.

mod backends;
mod book_events;
mod client;
mod contracts_provision;
#[cfg(test)]
#[path = "legacy_giver.rs"]
mod live_tests;
mod note_events;
mod note_withdraw_gate_boc;
mod order_events;

pub use backends::{
    real_market_deal_view, DealContext, RealBuyerBackend, RealDealBackend, RealNote,
    RealSellerBackend,
};
pub use book_events::{
    fold_book_event_pages, BookEventFold, BookEventMessage, BookEventPage, BookFillCandidate,
    LiveBookOrder,
};
pub use client::NoteDealCreditReceipt;
pub use client::{
    chain_clock_skew_preflight, chain_http_client, endpoint_urls, normalize_endpoint,
    note_transfer_amount_refusal, note_transfer_deposit_identifier_hash,
    note_transfer_dest_refusal, note_transfer_sender_refusal, note_transfer_submit_hint,
    observe_note_deploy_rootpn_action, observe_note_deploy_wallet_action, resolve_endpoint,
    BookFillCandidateRefusal, BookFillCandidateReport, ChainDoctorCheck, ChainDoctorReport,
    ChainDoctorStatus, Deployed, MoneySubmitError, NoteDeployRootPnActionObservation,
    NoteDeployWalletActionObservation, NoteHistoryCoverage, NoteTransferRefusal,
    OutstandingDealLead, OutstandingDealLeadRefusal, PrivateNoteOutstandingReport,
    RealChainBackend, RecoveredRestingOrder, TokenContractCurrentFacts,
    TokenContractReceiptChainData, TokenContractSettlementEvent, TokenContractSettlementReceipt,
    TokenContractSettlementReceipts, CHAIN_DOCTOR_CHECK_COUNT,
};
pub use client::{
    chain_time_secs, decode_multisig_queue_event, parse_message_destination,
    parse_source_transaction_out_messages, prove_multisig_delivery_message,
    read_multisig_queue_history, sole_delivery_sibling, MultisigQueueEvent, MultisigQueueRecord,
};
pub use client::{decode_tokens_withdrawn_event, TokensWithdrawnEvent};
pub use client::{
    is_transient_read_failure, retry_transient_read, ChainRequestCeiling, LimitedChainClient,
    RetryingReads,
};
pub use client::{PlaceInferenceBuyReceipt, TokenContractInboundCall};
pub use contracts_provision::keypair_ed_pubkey;
pub use contracts_provision::{
    compiled_contract_hash, compiled_contract_hashes, private_note_pin_of_generation,
};
pub use note_withdraw_gate_boc::note_withdraw_gate_from_account_boc;

// Rebuild the PMP account fixture against the contracts this tree compiles. Test-only and
// `#[ignore]`d: it writes into the working tree.
#[cfg(test)]
mod pmp_fixture_regen;
