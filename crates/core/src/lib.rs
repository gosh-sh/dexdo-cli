//! `dexdo-core` -- shared types, protocol parameters, the stream state machine, the crypto note,
//! and an on-chain abstraction with a mock implementation. Pure logic without networking (state
//! machine/formulas), plus real local note cryptography and `MockChainBackend`.
//! Canon: `dexdo-cli.md`-, `private-inference-market-design.md`-,, Appx. A.

pub mod chain;
pub mod handover;
pub mod machine;
pub mod note;
pub mod onchain_diagnostics;
pub mod params;
pub mod settle;
// issue: market-provisioning output manifest(pure data; consumed by seller/buyer).
pub mod manifest;
// issue: oracle/PMP prediction-market provisioning manifest(pure data).
pub mod oracle_manifest;
// wallet-address parse/normalize(`half1::half2` -> `0:<half2>`), fail-loud. Non-gated so
// the format logic is offline-tested; consumed by the real money path(`shellnet`) and the seed-wallet CLI.
pub mod wallet;
// real shellnet backend on top of the gosh.ackinacki SDK(behind the `shellnet` feature).
#[cfg(feature = "shellnet")]
pub mod shellnet;

/// SDK shellnet types -- re-exported behind `shellnet` for the live harness and the production CLI note-deploy
/// path. Custody stays external: dexdo reads the wallet/note secrets from explicit operator files and never
/// owns key generation.
#[cfg(feature = "shellnet")]
pub use gosh_ackinacki::{
    private_note,
    sdk::{Address, ChainClient, KeyPair},
};
#[cfg(feature = "shellnet")]
pub use shellnet::{
    keypair_ed_pubkey, real_market_deal_view, DealContext, Deployed, RealBuyerBackend,
    RealChainBackend, RealDealBackend, RealNote, RealSellerBackend, ShellnetDoctorCheck,
    ShellnetDoctorReport, ShellnetDoctorStatus, MODEL_TICK_SIZE,
};

pub use chain::{
    aggregate_tree, check_buy_deposit_headroom, check_disputable,
    check_matched_token_contract_state, check_no_duplicate_resting_asks, check_reclaimable,
    check_recoverable, check_release_disputable, check_seller_pubkey, check_withdrawable_shell,
    deal_anomalies, duplicate_resting_ask_token_contracts, executable_quote, per_model_breakdown,
    required_escrow_for_buy, ChainBackend, ChainError, CounterpartyTally, DealAnomaly,
    DealChainState, DealRole, DealView, ExecutableQuote, Match, MatchWatchCursor,
    MatchedTokenContractStatus, MockChainBackend, ModelBreakdown, NoteSnapshot, OfferListing,
    OrderBookOrder, OrderBookSnapshot, OrderBookStats, OrderBookSubscription, QuoteFill, SellOffer,
    StreamSnapshot, TokenContract, TreeSnapshot, MATCH_OPEN_TIMEOUT_SECS, UNKNOWN_MODEL,
};
pub use handover::Handover;
pub use machine::{InvariantError, Settlement, StreamMachine, StreamState, Tick};
pub use manifest::{
    model_hash_for, resolve_model_name, validate_canonical_model_id, MarketManifest,
};
pub use note::{verify, LocalNote, Note, NoteError, NotePubkey, NoteTree, Signature};
pub use onchain_diagnostics::{
    contract_error_names, sanitize_onchain_submit_payload, validate_onchain_submit_response,
    OnchainSubmitError,
};
pub use oracle_manifest::OracleMarketManifest;
pub use params::{DobParams, ProtocolConsts, Shell};
pub use settle::{fee, net_burn, probe_burn, rebate, rebate_rate_bps, ProbeBurn};
pub use wallet::normalize_wallet_address;
