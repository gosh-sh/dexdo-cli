//! "no wallet is bound" is its own machine-readable code, never `INTERNAL`.

//! # The defect this holds shut

//! A note deploy in `--json` mode with no binding and no `--multisig-address` emitted

//! ```text
//! {"schema":"dexdo.error.v1","operation":"note_deploy","code":"INTERNAL",
//! "message":"internal invariant failed","cause":"error[E_WALLET_NOT_CONFIGURED]..."}
//! ```

//! The one instruction the operator needed was buried in `cause`, while `code` and `message` said
//! the client had a bug. To a human those are different instructions; to a machine `INTERNAL` is an
//! unhandled branch -- a code that means "escalate", not "run one setup command". The refusal is
//! raised before anything reaches the chain, so it is also the cheapest possible outcome to report
//! correctly.

//! # Why the error here is produced, not written

//! Every case builds its error by calling [`resolve_funding_wallet`] on an empty store -- the exact
//! call `run_note_deploy` and `run_note_topup` make, in the exact state the live run was in. An
//! `anyhow!("...")` with the same words would prove only that the words classify, which is what the
//! shipped classifier already did with them: it read them and returned `INTERNAL`.

use super::{
    classify_error, error_cause, forbidden_machine_fragment, ErrorCode, MachineError, ERROR_SCHEMA,
    OP_NOTE_DEPLOY, WALLET_NOT_CONFIGURED_CODE,
};
use crate::cli::wallet::{resolve_funding_wallet, WalletStore};

/// `note topup` shares the fail-fast but has no `--json` surface of its own, so it has no operation
/// constant. Classification must not depend on which command asked.
const OP_NOTE_TOPUP: &str = "note_topup";

/// The refusal exactly as a money command raises it: no `--multisig-address`, no key, no binding.
fn wallet_fail_fast() -> (tempfile::TempDir, anyhow::Error) {
    let dir = tempfile::tempdir().expect("temp dir");
    let store = WalletStore::at(dir.path().join("wallet"));
    let error = resolve_funding_wallet(
        &store,
        &crate::cli::wallet::test_network_a(),
        None,
        &None,
        &None,
    )
    .expect_err("a command that spends the Hot cannot proceed without one");
    (dir, error)
}

/// The classification, for both commands that spend the Hot. Reverting the mapping puts this back
/// on `INTERNAL` and fails here.
#[test]
fn the_wallet_fail_fast_classifies_as_its_own_code_and_never_as_internal() {
    for operation in [OP_NOTE_DEPLOY, OP_NOTE_TOPUP] {
        let (_dir, error) = wallet_fail_fast();
        let code = classify_error(operation, &error);
        assert_eq!(
            code,
            ErrorCode::WalletNotConfigured,
            "{operation}: an unbound wallet is a configuration state, not a client fault: {error:#}"
        );
        assert_eq!(code.as_str(), WALLET_NOT_CONFIGURED_CODE);
        assert_eq!(code.as_str(), "wallet_not_configured");
        assert_ne!(code, ErrorCode::Internal, "{operation}");
        assert_ne!(code.as_str(), "INTERNAL", "{operation}");
    }
}

/// The emitted envelope, field by field, as `--json` prints it.

/// `message` has to carry the remediation itself: it is the field an orchestrator shows a human,
/// and `internal invariant failed` told them to file a bug. The full hint stays on `cause`.
#[test]
fn the_emitted_envelope_names_the_code_and_the_remediation() {
    for operation in [OP_NOTE_DEPLOY, OP_NOTE_TOPUP] {
        let (_dir, error) = wallet_fail_fast();
        let code = classify_error(operation, &error);
        let rendered = serde_json::to_string(
            &MachineError::new(OP_NOTE_DEPLOY, code).with_cause(error_cause(&error)),
        )
        .expect("serialize the machine error");
        let value: serde_json::Value =
            serde_json::from_str(&rendered).expect("the envelope is json");

        assert_eq!(value["schema"], ERROR_SCHEMA, "{operation}");
        assert_eq!(value["code"], "wallet_not_configured", "{operation}");
        assert_ne!(value["code"], "INTERNAL", "{operation}");
        assert_eq!(
            value["retryable"], false,
            "{operation}: repeating the command binds no wallet"
        );

        let message = value["message"].as_str().expect("a message");
        assert_ne!(message, "internal invariant failed", "{operation}");
        assert_eq!(
            message, "wallet is not configured; run `dexdo wallet onboard gosh-ai` first",
            "{operation}: the remediation must be directly executable"
        );

        let cause = value["cause"].as_str().expect("a cause");
        assert!(
            cause.contains("E_WALLET_NOT_CONFIGURED"),
            "{operation}: the stable code from the error table must survive: {cause}"
        );
        assert!(
            cause.contains("hint:") && cause.contains("dexdo wallet onboard"),
            "{operation}: the hint must reach the envelope intact: {cause}"
        );
        assert!(
            forbidden_machine_fragment(&rendered).is_none(),
            "{operation}: {rendered}"
        );
    }
}

/// The fix is a TYPED match on one code, so it must not have moved anything else onto it.

/// The two neighbours in the same downcast block, a chain error and the fall-through that legitimately
/// stays `INTERNAL` are all checked here: if the new arm had been placed so that it swallowed them,
/// or written as another text rule, one of these would come back `wallet_not_configured`.
#[test]
fn no_other_error_was_moved_onto_the_wallet_code() {
    let unreachable = anyhow::Error::new(
        dexdo_core::DexdoError::new(
            dexdo_core::error_codes::E_GATEWAY_UNREACHABLE,
            "the seller gateway did not answer",
        )
        .with_hint(dexdo_core::error_codes::E_GATEWAY_UNREACHABLE.fix()),
    );
    let wrong_endpoint = anyhow::Error::new(dexdo_core::DexdoError::new(
        dexdo_core::error_codes::E_GATEWAY_WRONG_ENDPOINT,
        "the certificate did not match the pin",
    ));
    let transport = anyhow::Error::new(dexdo_core::ChainError::Transport("rpc gone".to_string()));
    // Wording taken from the wallet refusal so a text rule, rather than a typed match, would be
    // caught: this carries no code at all and must stay INTERNAL.
    let untyped = anyhow::anyhow!("no active wallet binding, and nothing structured said so");

    for (error, expected) in [
        (unreachable, ErrorCode::GatewayConnectFailed),
        (wrong_endpoint, ErrorCode::GatewayAuthFailed),
        (transport, ErrorCode::ChainTransport),
        (untyped, ErrorCode::Internal),
    ] {
        assert_eq!(
            classify_error(OP_NOTE_DEPLOY, &error),
            expected,
            "{error:#}"
        );
    }
}
