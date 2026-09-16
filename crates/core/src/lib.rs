//! `dexdo-core` -- shared types, protocol parameters, the stream state machine, the crypto note,
//! and an on-chain abstraction with a mock implementation. Pure logic without networking (state
//! machine/formulas), plus real local note cryptography and `MockChainBackend`.

//! Canon: `dexdo-cli.md`-, `private-inference-market-design.md`-,, Appx. A.

// The default limit of 128 is not enough to prove `Send` for the chain backend's async trait
// methods now that there are no cargo features. With the features, `place_buy` and the
// nine calls it transitively awaits were split across two compilations and neither had to be
// proved whole; in one build rustc walks the entire chain and gives up at the limit with
// "overflow evaluating the requirement". It is a warning on this toolchain -- the bound is
// assumed rather than proved -- and a warning is the wrong place for a `Send` bound to be
// decided, because the same code is an error on any toolchain that counts a step differently.
#![recursion_limit = "256"]

// issue: canonical `<dapp_id>::<account_id>` addresses -- the one parse/format for every address a
// user reads, pastes, or has persisted. Non-gated: the format logic is offline-tested.
pub mod address;
// issue: Shell Accumulator SHELL <-> eccUSDC money arithmetic and getter decoders. Non-gated
// on purpose - the planning logic is what decides whether money moves, so it is offline-tested and
// runs under the default-feature CI gate rather than only under the chain build.
pub mod accumulator;
pub mod market;
// issue: the structured user-facing error (stable code + kind + message + preserved source
// chain). It lives in `core` -- not in the CLI crate -- because `dexdo` already depends on `core`,
// so there is no dependency inversion, and both crates can construct the same codes.
pub mod error;
pub mod handover;
pub mod machine;
pub mod note;
pub mod onchain_diagnostics;
pub mod params;
pub mod settle;
// issue: market-provisioning output manifest (pure data; consumed by seller/buyer).
pub mod manifest;
// issue: oracle/PMP prediction-market provisioning manifest (pure data).
pub mod oracle_manifest;
// wallet-address parse/normalize (`half1::half2` -> `0:<half2>`), fail-loud. Non-gated so
// the format logic is offline-tested; consumed by the real money path (the chain build) and the seed-wallet CLI.
mod canonical_multisig_allowlist;
pub mod wallet;
// real chain backend on top of the gosh.ackinacki SDK (behind a cargo feature that no longer exists).
pub mod canonical_multisig;
/// Stable classification prefix emitted when the chain read policy exhausts its retry budget.
/// It remains feature-independent because the seller must recognize that result in default builds.
pub const CHAIN_READ_EXHAUSTED_MESSAGE_PREFIX: &str = "chain read got no answer after ";
pub mod chain;

/// which of `withdrawTokens`'s eleven gates holds a note's money. Feature-independent: the
/// per-gate regression the money directive asks for must run in the default build.
pub mod note_withdraw_gate;
pub use note_withdraw_gate::{
    check_withdrawal_arrival, note_withdraw_gate_from_storage,
    refusal_carries_a_withdraw_gate_code, withdraw_gate_line, NoteWithdrawGate,
    NoteWithdrawalArrival, WithdrawGate, WITHDRAW_GATE_EXIT_CODES, WITHDRAW_GATE_FIELDS,
};

/// SDK chain types -- re-exported behind the chain build for the live harness and the production CLI note-deploy
/// path. Wallet custody stays external. `note deploy` generates the PrivateNote owner key and persists it in
/// operator-owned recovery/pool files; subsequent commands read wallet/note secrets from explicit files.
pub use gosh_ackinacki::{
    airegistry, private_note,
    sdk::{Address, ChainClient, KeyPair},
};
pub mod ackinacki_wallet {
    pub use gosh_ackinacki::wallet::query;
}
pub use chain::{
    chain_clock_skew_preflight, chain_http_client, endpoint_urls, keypair_ed_pubkey,
    normalize_endpoint, note_transfer_amount_refusal, note_transfer_deposit_identifier_hash,
    note_transfer_dest_refusal, note_transfer_sender_refusal, note_transfer_submit_hint,
    note_withdraw_gate_from_account_boc, real_market_deal_view, resolve_endpoint, ChainDoctorCheck,
    ChainDoctorReport, ChainDoctorStatus, DealContext, Deployed, MoneySubmitError,
    NoteDealCreditReceipt, NoteTransferRefusal, RealBuyerBackend, RealChainBackend,
    RealDealBackend, RealNote, RealSellerBackend, TokenContractCurrentFacts,
    TokenContractReceiptChainData, TokenContractSettlementEvent, TokenContractSettlementReceipt,
    TokenContractSettlementReceipts, CHAIN_DOCTOR_CHECK_COUNT,
};
pub use chain::{PlaceInferenceBuyReceipt, TokenContractInboundCall};

pub use address::{CanonicalAddress, DEXDO_DAPP_ID};
pub use error::{codes as error_codes, BoxError, DexdoError, ErrorCode, ErrorKind};
pub use handover::Handover;
pub use machine::{InvariantError, Settlement, StreamMachine, StreamState, Tick};
pub use manifest::{
    model_hash_for, parse_canonical_model_id, resolve_model_name, validate_canonical_model_id,
    AttestedModelPrecision, CanonicalModelFlags, CanonicalModelId, MarketManifest,
};
pub use market::flags as order_flags;
pub use market::{
    aggregate_tree, buyer_net_result, buyer_total_debit, check_buy_deposit_headroom,
    check_declared_payout_against_credits, check_disputable, check_matched_token_contract_state,
    check_reclaimable, check_recoverable, check_release_disputable, check_seller_pubkey,
    check_subscription_buy_reserve, check_withdrawable_shell, deal_anomalies, executable_quote,
    implied_write_off, order_deadline_is_live, ordinary_buy_reserve, per_model_breakdown,
    required_escrow_for_buy, submit_safe_single_ask_quote, subscription_buy_clearing_refund,
    subscription_buy_reserve, subscription_claim_cap_at, subscription_current_week_headroom,
    validate_seller_resume_state, BuyerOrderFact, BuyerOrderFactKind, BuyerStopTerminalFact,
    BuyerStopTerminalReceipt, ChainBackend, ChainError, ClaimBounds, ConservationBreach,
    CounterpartyTally, DealAnomaly, DealBuyerBond, DealChainSnapshot, DealChainState, DealLedger,
    DealLedgerStep, DealMoneyFlow, DealOfferLatch, DealRole, DealSellerBond, DealSettlement,
    DealSubscription, DealTerminalSettlement, DealView, ExecutableQuote,
    InferenceSubscriptionPlacement, Match, MatchWatchCursor, MatchedFill,
    MatchedTokenContractStatus, MockChainBackend, MockSubscriptionExit, MockSubscriptionTerminal,
    ModelBreakdown, NoteSnapshot, OfferListing, OrderBookOrder, OrderBookSnapshot, OrderBookStats,
    QuoteFill, RawUint128, SellOffer, SellOfferOutcome, SettlementAction,
    SettlementActionBondState, SettlementActionEvent, SettlementActionPostState,
    SettlementActionReceipt, StreamSnapshot, SubscriptionBuyReserve, TokenContract, TreeSnapshot,
    UNKNOWN_MODEL,
};
pub use note::{verify, LocalNote, Note, NoteError, NotePubkey, NoteTree, Signature};
pub use onchain_diagnostics::{
    contract_error_label, contract_error_names, sanitize_onchain_submit_payload,
    unvendored_contract_error_label, validate_onchain_submit_response, OnchainSubmitError,
};
pub use oracle_manifest::OracleMarketManifest;
pub use params::{
    cli_buy_deadline_is_valid, default_buy_deadline, price_raw_from_shell, probe_seed_owed,
    shell_amount, shell_amount_of_text, shell_amount_raw, DobParams, ProtocolConsts, Shell,
    BUYER_HANDOVER_WAIT_SECS, BUYER_ON_DEMAND_PURCHASE_SECS, DEAL_SNAPSHOT_MAX_ATTEMPTS,
    DEFAULT_BUY_TTL, MATCH_OPEN_TIMEOUT, MATCH_OPEN_TIMEOUT_SECS, MAX_SELL_TTL,
    MIN_STREAM_BUY_TICKS, PLATFORM_FEE_BPS, PRICE_STEP, PROBE_SEED_TOKENS,
    SELLER_TERMINAL_RECEIPT_POLL_INTERVAL, SELLER_TERMINAL_RECEIPT_TIMEOUT, SHELL_ECC_ID,
    SUBSCRIPTION_BUYER_BOND_TICKS, SUBSCRIPTION_MAX_TICKS, SUBSCRIPTION_ORDER_RECONCILE_POLL,
    SUBSCRIPTION_WEEKS, SUB_TICKS_PER_WEEK, SUB_WEEK_LEN, TICK_SIZE,
};
pub use settle::{
    contested_burn, fee, net_burn, probe_burn, rebate, rebate_rate_bps, ContestedBurn,
};
pub use wallet::{normalize_multisig_pubkey, normalize_wallet_address};
