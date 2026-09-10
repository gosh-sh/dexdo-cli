//! Completing an onboarding must not drop the identity it proved.

//! The recorded answer on (2026-08-12) separates the two Bee addresses: `hello.profile_address`
//! is the Connect `AuthProfile` address and "we save it separately in the completed onboarding
//! state". Completion used to discard it -- `ResponseReceived` carried it, `Complete` did not -- so
//! a session that finished no longer knew which profile it had been onboarded through, and the
//! value cannot be recovered afterwards: the invitation is spent and cannot be scanned twice.

//! Driven through the real durable file format rather than a constructed phase, because that is
//! what is actually at risk: the CLI writes the session after `mark_complete` and reads it back on
//! the next run. Every value here is synthetic -- a live session's signing and DH secrets must
//! never enter the repository.

use super::{
    parse_scoped_address, AgentWalletsResponse, OnboardingSession, SessionPhase,
    AGENT_WALLETS_BODY_VERSION, SESSION_FILE_VERSION,
};

fn filler(byte: char) -> String {
    byte.to_string().repeat(64)
}

/// The Connect `AuthProfile` address.
fn profile() -> String {
    format!("0:{}", filler('f'))
}

/// The multifactor wallet address -- a DIFFERENT value, per the recorded answer, and different
/// here too so that a test cannot pass by confusing the two.
fn wallet() -> String {
    format!("0:{}", filler('e'))
}

fn response() -> AgentWalletsResponse {
    AgentWalletsResponse {
        version: AGENT_WALLETS_BODY_VERSION,
        network: "net-a".to_string(),
        vault: parse_scoped_address(&format!("{0}::{0}", filler('c'))).unwrap(),
        hot: parse_scoped_address(&format!("{0}::{0}", filler('d'))).unwrap(),
    }
}

fn session_state() -> serde_json::Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    serde_json::json!({
        "encryption_root": filler('1'),
        "my_dh_secret": filler('2'),
        "peer_dh_public": filler('3'),
        "signing_public": filler('4'),
        "signing_secret": filler('5'),
        "created_at": now - 120,
        "expires_at": now + 3600,
        "last_seen_seq": 1_000_000_000_003u64,
        "last_sent_seq": 1_000_000_000_002u64,
    })
}

/// A state file in the shape the CLI writes once the wallet's response has been authenticated.
fn response_received_state() -> String {
    state_with_phase(serde_json::json!({
        "name": "response_received",
        "profile_address": profile(),
        "wallet_address": wallet(),
        "response_event_id": filler('7'),
        "session_state": session_state(),
        "response": response(),
    }))
}

fn state_with_phase(phase: serde_json::Value) -> String {
    serde_json::json!({
        "file_version": SESSION_FILE_VERSION,
        "agent_name": "fixture-agent",
        "network": "net-a",
        "endpoint": "https://net-a.example",
        "hot_pubkey": filler('a'),
        "phase": phase,
    })
    .to_string()
}

/// A prepared request on disk, with or without the optional wallet address.
fn request_json(wallet_address: Option<String>) -> serde_json::Value {
    let mut request = serde_json::json!({
        "profile_address": profile(),
        "session_id": "fixture-session-id",
        "hello_event_id": filler('7'),
        "context_created_at_from": 1_000_000_000u64,
        "envelope_json": "{}",
        "session_state": session_state(),
    });
    if let Some(address) = wallet_address {
        request["wallet_address"] = serde_json::json!(address);
    }
    request
}

fn load(state: &str) -> OnboardingSession {
    serde_json::from_str(state).expect("the fixture is a valid state file")
}

#[test]
fn completing_keeps_the_authenticated_profile_address() {
    let session = load(&response_received_state());
    assert_eq!(session.phase_name(), "response_received");
    assert_eq!(session.profile_address(), Some(profile().as_str()));

    let complete = session
        .mark_complete()
        .expect("a validated response completes");
    assert_eq!(complete.phase_name(), "complete");
    assert_eq!(
        complete.profile_address(),
        Some(profile().as_str()),
        "completion must not drop the AuthProfile address the hello proved"
    );

    // Durability is the point: the CLI persists the session immediately after `mark_complete`,
    // and a later run reads this file rather than the value in memory.
    let written = serde_json::to_string(&complete).expect("a complete session serializes");
    assert!(
        written.contains(&profile()),
        "the completed state file does not carry the profile address: {written}"
    );
    let reloaded = load(&written);
    assert_eq!(reloaded.phase_name(), "complete");
    assert_eq!(reloaded.profile_address(), Some(profile().as_str()));
    reloaded
        .validate_file()
        .expect("a completed state file stays loadable");
}

#[test]
fn a_session_completed_before_the_address_was_retained_still_loads() {
    // Written by a binary that dropped the address. It must keep loading -- forcing a fresh QR
    // scan on an onboarding that already finished would be a worse failure than a missing
    // reserved field -- and it must report absence rather than an empty address.
    let legacy = state_with_phase(serde_json::json!({"name": "complete", "response": response()}));

    let session = load(&legacy);
    assert_eq!(session.phase_name(), "complete");
    session
        .validate_file()
        .expect("an older completed state file is still a valid state file");
    assert_eq!(session.profile_address(), None);
    assert_eq!(session.wallet_address(), None);
}

#[test]
fn an_incomplete_phase_reports_no_profile_address() {
    let session = load(&response_received_state());
    let before = load(&state_with_phase(serde_json::json!({
        "name": "awaiting_wallets_response",
        "request": request_json(Some(wallet())),
    })));

    // The accessor reports the address of a PROVED onboarding, so a phase that has not consumed
    // the wallet's response yet answers nothing rather than the request's own copy.
    assert_eq!(before.profile_address(), None);
    assert_eq!(before.wallet_address(), None);
    assert_eq!(session.profile_address(), Some(profile().as_str()));
}

#[test]
fn completing_keeps_the_multifactor_wallet_address_and_keeps_it_distinct() {
    // `push_profile_address` in the binding is this value, not the `AuthProfile` one, so a test
    // that used the same string for both would pass while the wrong address was recorded.
    let session = load(&response_received_state());
    assert_eq!(session.wallet_address(), Some(wallet().as_str()));
    assert_ne!(
        session.wallet_address(),
        session.profile_address(),
        "the two Bee addresses are different values"
    );

    let complete = session
        .mark_complete()
        .expect("a validated response completes");
    assert_eq!(
        complete.wallet_address(),
        Some(wallet().as_str()),
        "completion must not drop the multifactor wallet address"
    );
    assert_eq!(complete.profile_address(), Some(profile().as_str()));

    let written = serde_json::to_string(&complete).expect("a complete session serializes");
    let reloaded = load(&written);
    assert_eq!(reloaded.wallet_address(), Some(wallet().as_str()));
    assert_eq!(reloaded.profile_address(), Some(profile().as_str()));
    reloaded
        .validate_file()
        .expect("a completed state file stays loadable");
}

#[test]
fn a_hello_that_carried_no_wallet_address_still_completes() {
    // The recorded answer makes the address optional: its absence must not by itself fail an
    // onboarding that is otherwise proved.
    let session = load(&state_with_phase(serde_json::json!({
        "name": "response_received",
        "profile_address": profile(),
        "response_event_id": filler('7'),
        "session_state": session_state(),
        "response": response(),
    })));
    assert_eq!(session.wallet_address(), None);

    let complete = session
        .mark_complete()
        .expect("an absent optional address is not an onboarding failure");
    assert_eq!(complete.wallet_address(), None);
    assert_eq!(complete.profile_address(), Some(profile().as_str()));
    // Absent stays absent on disk rather than becoming an empty string.
    let written = serde_json::to_string(&complete).expect("a complete session serializes");
    assert!(!written.contains("wallet_address"), "{written}");
}

#[test]
fn the_prepared_request_carries_the_wallet_address_across_a_restart() {
    // The invitation is spent and cannot be scanned twice, so the state file is the only thing
    // that survives a restart -- if the request does not carry the address, an onboarding resumed
    // after a restart finishes without it.
    let session = load(&state_with_phase(serde_json::json!({
        "name": "request_prepared",
        "request": request_json(Some(wallet())),
    })));
    let SessionPhase::RequestPrepared { request } = &session.phase else {
        panic!(
            "the fixture is a prepared request, not `{}`",
            session.phase_name()
        );
    };
    assert_eq!(request.wallet_address, wallet());
    session
        .validate_file()
        .expect("a prepared request carrying the address is a valid state file");

    // A state file written before this field existed still loads, and stays absent.
    let older = load(&state_with_phase(serde_json::json!({
        "name": "request_prepared",
        "request": request_json(None),
    })));
    let SessionPhase::RequestPrepared { request } = &older.phase else {
        panic!("the fixture is a prepared request");
    };
    assert!(request.wallet_address.is_empty());
    older
        .validate_file()
        .expect("an older prepared request is still a valid state file");
}
