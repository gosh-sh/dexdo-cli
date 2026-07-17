//! Real shellnet backend -- an on-chain adapter on top of the **`gosh.ackinacki`
//! SDK**(`gosh_ackinacki::sdk`). The wallet/keys/chain interaction are not rewritten -- they are
//! taken from the SDK.
//! This module covers **step 1 of the START signal**:
//! connecting to the manifest-selected Block Manager endpoint and reading the deployed contracts
//! (`contracts/deployed.shellnet.json`). The `ChainBackend` trait implementation
//! (offer/match in `InferenceOrderBook`, probe/`advance`/`stop`/burn in `TokenContract`, notes in
//! `PrivateNote`) is layered on top of this `ChainClient` in the next step -- its money choreography
//! is verified against the real on-chain(funded keys required), so no trait
//! stubs are introduced here.

mod backends;
mod book_events;
mod client;
mod contracts_provision;
#[cfg(all(test, feature = "test-giver"))]
#[path = "legacy_giver.rs"]
mod live_tests;
mod note_events;
mod order_events;
mod stream_locks;

pub use backends::{
    real_market_deal_view, DealContext, RealBuyerBackend, RealDealBackend, RealNote,
    RealSellerBackend, MODEL_TICK_SIZE,
};
pub use book_events::{
    fold_book_event_pages, BookEventFold, BookEventMessage, BookEventPage, LiveBookOrder,
};
pub use client::{
    endpoint_urls, normalize_endpoint, resolve_endpoint, Deployed, MoneySubmitError,
    NoteStreamLockStatus, RealChainBackend, ShellnetDoctorCheck, ShellnetDoctorReport,
    ShellnetDoctorStatus, DEFAULT_SHELLNET_ENDPOINT, PRIVATE_NOTE_STREAM_LOCK_MAX_SECS,
};
pub use contracts_provision::keypair_ed_pubkey;
pub use stream_locks::{NoteStreamLockEntry, NoteStreamLockKind, NoteStreamLockSnapshot};
