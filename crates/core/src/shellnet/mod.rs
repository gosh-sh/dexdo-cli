//! Real shellnet backend -- an on-chain adapter on top of the **`gosh.ackinacki`
//! SDK**(`gosh_ackinacki::sdk`). The wallet/keys/chain interaction are not rewritten -- they are
//! taken from the SDK.
//! This module covers **step 1 of the START signal**:
//! connecting to shellnet via `ChainClient::shellnet()` and reading the manifest of deployed
//! contracts(`contracts/deployed.shellnet.json`). The `ChainBackend` trait implementation
//! (offer/match in `InferenceOrderBook`, probe/`advance`/`stop`/burn in `TokenContract`, notes in
//! `PrivateNote`) is layered on top of this `ChainClient` in the next step -- its money choreography
//! is verified against the real on-chain(funded keys required), so no trait
//! stubs are introduced here.

mod backends;
mod client;
mod contracts_provision;
#[cfg(all(test, feature = "test-giver"))]
#[path = "legacy_giver.rs"]
mod live_tests;
mod note_events;

pub use backends::{
    real_market_deal_view, DealContext, RealBuyerBackend, RealDealBackend, RealNote,
    RealSellerBackend, MODEL_TICK_SIZE,
};
pub use client::{
    Deployed, RealChainBackend, ShellnetDoctorCheck, ShellnetDoctorReport, ShellnetDoctorStatus,
};
pub use contracts_provision::keypair_ed_pubkey;
