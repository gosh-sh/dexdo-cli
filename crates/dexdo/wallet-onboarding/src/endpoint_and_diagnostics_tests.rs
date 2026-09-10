//! Regressions for the two failures that stopped a real `dexdo wallet onboard --network mainnet`
//! after the wallet had already answered: a scheme-less endpoint reaching the `AuthProfile` write,
//! and a failure report that discarded everything except the words "Send message".

use ackinacki_kit::tvm_client::net::construct_rest_api_endpoint;
use ackinacki_kit::tvm_client::processing::ResultOfSendMessage;
use bee_connect::errors::AppError;

use super::{describe_bee_failure, send_message_failure, CanonicalBeeSessionIo};

/// The scheme the SDK will use for `/v2/messages` given an endpoint, decided the way
/// `ServerLink::new` decides it: `endpoints.first().starts_with("https://")`, then
/// `construct_rest_api_endpoint`. Reproduced rather than mocked, so the pin below fails if the
/// vendored SDK ever changes the rule this guard exists to satisfy.
fn sdk_write_base(endpoint: &str) -> String {
    construct_rest_api_endpoint(endpoint, endpoint.starts_with("https://"))
        .expect("the SDK can always build a REST base from a host")
        .to_string()
}

#[test]
fn a_scheme_less_endpoint_cannot_reach_the_authprofile_write() {
    // Why the guard has to exist: the same bare host that reads fine writes over plain http.
    assert_eq!(
        sdk_write_base("net-b.example"),
        "http://net-b.example/v2/",
        "a bare host puts the AuthProfile write on plain http, which the edge answers 405"
    );
    assert_eq!(
        sdk_write_base("https://net-b.example"),
        "https://net-b.example/v2/"
    );

    // So the constructor refuses one instead of carrying it to the write.
    for bare in [
        "net-b.example",
        "net-a.example",
        "  net-b.example  ",
        "net-b.example/graphql",
    ] {
        let error = CanonicalBeeSessionIo::new(bare)
            .err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| {
                panic!("`{bare}` has no scheme and must never reach the AuthProfile write path")
            });
        assert!(
            error.contains("has no scheme"),
            "the refusal must name the cause, got: {error}"
        );
    }

    assert!(CanonicalBeeSessionIo::new("https://net-b.example").is_ok());
    assert!(CanonicalBeeSessionIo::new("http://127.0.0.1:8033").is_ok());
    assert!(CanonicalBeeSessionIo::new("   ").is_err());
}

fn failing_publish() -> AppError {
    let mut error = AppError::new("Send message");
    error.kind = Some("tvm_exit".to_string());
    error.module = Some("authservice".to_string());
    error.error_code = Some("12".to_string());
    error.details = Some("tvm_code=414, exit_code=Some(12), http_status=405".to_string());
    error.tvm_error = Some(serde_json::json!({
        "code": 414,
        "message": "Can not send message: 405 Method Not Allowed",
    }));
    error
}

#[test]
fn a_failed_bee_operation_reports_its_structured_cause() {
    let described = describe_bee_failure("publish agent_onboard_request", &failing_publish(), &[]);
    for expected in [
        "publish agent_onboard_request",
        "Send message",
        "kind=tvm_exit",
        "module=authservice",
        "error_code=12",
        "http_status=405",
        "405 Method Not Allowed",
    ] {
        assert!(
            described.contains(expected),
            "the operator lost `{expected}` from: {described}"
        );
    }

    // An error carrying nothing but a message must still read as one clean line, not as a row of
    // empty fields.
    let bare = describe_bee_failure(
        "query bee wallet onboarding context",
        &AppError::new("no"),
        &[],
    );
    assert_eq!(bare, "query bee wallet onboarding context: no");
}

#[test]
fn a_failure_report_never_carries_a_secret_or_the_encrypted_envelope() {
    let signing_secret = "5555555555555555555555555555555555555555555555555555555555555555";
    let dh_secret = "2222222222222222222222222222222222222222222222222222222222222222";
    let envelope = r#"{"v":"bee_connect.msg/1","body":"AAAAAAAAAAAAAAAAAAAAAAAAAA"}"#;

    // A transport failure is free to quote the request it could not send, in any field.
    let mut error = AppError::new(format!("Send message: rejected {envelope}"));
    error.details = Some(format!("signer=Keys({signing_secret})"));
    error.tvm_error = Some(serde_json::json!({ "params": { "my_dh_secret": dh_secret } }));

    let described = describe_bee_failure(
        "publish agent_onboard_request",
        &error,
        &[signing_secret, dh_secret, envelope],
    );
    for secret in [signing_secret, dh_secret, envelope] {
        assert!(
            !described.contains(secret),
            "a secret survived redaction in: {described}"
        );
    }
    assert!(described.contains("[redacted]"), "{described}");
    assert!(described.contains("Send message"), "{described}");
}

#[test]
fn a_refused_transaction_reports_the_values_the_node_returned() {
    assert_eq!(send_message_failure(&ResultOfSendMessage::default()), None);
    assert_eq!(
        send_message_failure(&ResultOfSendMessage {
            aborted: Some(false),
            exit_code: Some(0),
            ..Default::default()
        }),
        None
    );

    let aborted = send_message_failure(&ResultOfSendMessage {
        aborted: Some(true),
        exit_code: Some(12),
        message_hash: Some("0xabc".to_string()),
        ..Default::default()
    })
    .expect("an aborted transaction is a failure");
    assert!(aborted.contains("aborted=true"), "{aborted}");
    assert!(aborted.contains("exit_code=12"), "{aborted}");
    assert!(aborted.contains("message_hash=0xabc"), "{aborted}");

    // A non-zero exit code with no `aborted` flag is still a failure, and "the node said nothing"
    // must not be reported as "the node said false".
    let exited = send_message_failure(&ResultOfSendMessage {
        exit_code: Some(101),
        ..Default::default()
    })
    .expect("a non-zero exit code is a failure");
    assert!(exited.contains("exit_code=101"), "{exited}");
    assert!(exited.contains("aborted=unknown"), "{exited}");
    assert!(exited.contains("tx_hash=unknown"), "{exited}");
}
