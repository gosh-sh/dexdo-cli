//! every failure of a contract call has exactly one type, and the contract and the function
//! of a failed message are read from the message itself.
use super::*;
use crate::onchain_diagnostics::validate_onchain_submit_response;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// Every call this test binary reports after [`install`], for a test to find by its own account.
static REPORTED: Mutex<Vec<(String, &'static str, String, CallErrorType)>> = Mutex::new(Vec::new());

fn record(call: &FailedCall<'_>) {
    REPORTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push((
            call.account.to_ascii_lowercase(),
            call.contract,
            call.method.to_string(),
            call.error_type,
        ));
}

/// Install the recorder of this test binary; nothing else in it installs an observer. Other tests'
/// failures are recorded too, under their own accounts, so a test reads only the account it called.
pub(crate) fn install() {
    observe(record);
}

/// What was reported for `account`, in order.
pub(crate) fn reported_for(account: &str) -> Vec<(&'static str, String, CallErrorType)> {
    let account = account.rsplit(':').next().unwrap().to_ascii_lowercase();
    REPORTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .filter(|(reported, ..)| *reported == account)
        .map(|(_, contract, method, error_type)| (*contract, method.clone(), *error_type))
        .collect()
}

fn refused(response: serde_json::Value) -> anyhow::Error {
    anyhow::Error::new(validate_onchain_submit_response(response).unwrap_err())
}

fn status(code: u16) -> anyhow::Error {
    let response: reqwest::Response = http::Response::builder()
        .status(code)
        .body(Vec::<u8>::new())
        .unwrap()
        .into();
    anyhow::Error::new(response.error_for_status().unwrap_err())
}

/// A listener that accepts and never answers, and the address it listens on.
async fn silent() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            held.push(socket);
        }
    });
    (address, task)
}

#[test]
fn the_list_of_types_is_closed_and_each_label_is_its_own() {
    let labels: Vec<&str> = CallErrorType::ALL.iter().map(|t| t.as_str()).collect();
    assert_eq!(
        labels,
        [
            "transport",
            "timeout",
            "node_rejected",
            "tvm_compute",
            "tvm_action",
            "unconfirmed",
            "decode",
            "client"
        ]
    );
}

#[tokio::test]
async fn an_unreachable_node_is_transport_and_a_5xx_is_transport() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let refused = reqwest::Client::new()
        .post(format!("http://{address}/graphql"))
        .send()
        .await
        .unwrap_err();
    assert!(refused.is_connect(), "{refused}");
    assert_eq!(
        classify(&anyhow::Error::new(refused).context("POST graphql")),
        CallErrorType::Transport
    );
    assert_eq!(classify(&status(502)), CallErrorType::Transport);
    assert_eq!(classify(&status(503)), CallErrorType::Transport);
}

#[tokio::test]
async fn a_node_that_does_not_answer_in_time_is_timeout() {
    let (address, server) = silent().await;
    let error = reqwest::Client::builder()
        .timeout(Duration::from_millis(50))
        .build()
        .unwrap()
        .post(format!("http://{address}/graphql"))
        .send()
        .await
        .unwrap_err();
    assert!(error.is_timeout(), "{error}");
    assert_eq!(classify(&anyhow::Error::new(error)), CallErrorType::Timeout);
    server.abort();

    // The read retry's own ceiling: the attempt is dropped and `Elapsed` stands for it.
    let elapsed = tokio::time::timeout(Duration::ZERO, std::future::pending::<()>())
        .await
        .unwrap_err();
    let exhausted = anyhow::Error::new(elapsed)
        .context("chain read exceeded 20s on attempt 3")
        .context("chain read gave up after 3 attempt(s)");
    assert_eq!(classify(&exhausted), CallErrorType::Timeout);
}

#[test]
fn an_answered_refusal_is_node_rejected() {
    assert_eq!(
        classify(&refused(json!({"error": {
            "code": "INVALID_ARGUMENT", "message": "message expired", "data": {"exit_code": null}
        }}))),
        CallErrorType::NodeRejected
    );
    for code in [400, 403, 404, 429] {
        assert_eq!(
            classify(&status(code)),
            CallErrorType::NodeRejected,
            "{code}"
        );
    }
    assert_eq!(
        classify(&anyhow::anyhow!(
            r#"GraphQL errors: [{{"message":"account query failed"}}]"#
        )),
        CallErrorType::NodeRejected
    );
}

#[test]
fn an_aborted_or_non_zero_compute_phase_is_tvm_compute() {
    for response in [
        json!({"result": {"exit_code": 151}}),
        json!({"result": {"aborted": true}}),
        json!({"error": {"code": "TVM_ERROR", "message": "compute phase failed", "data": {"exit_code": 321}}}),
    ] {
        assert_eq!(
            classify(&refused(response.clone())),
            CallErrorType::TvmCompute,
            "{response}"
        );
    }
    assert_eq!(
        classify(&anyhow::anyhow!(
            "run_tvm getter getState: Contract execution was terminated with error: 151"
        )),
        CallErrorType::TvmCompute
    );
}

#[test]
fn a_non_zero_action_phase_is_tvm_action() {
    assert_eq!(
        classify(&refused(json!({"result": {
            "aborted": true, "compute": {"exit_code": 0}, "action": {"result_code": 38, "no_funds": true}
        }}))),
        CallErrorType::TvmAction
    );
}

#[test]
fn a_send_that_may_have_landed_is_unconfirmed_whatever_it_carries() {
    let forwarding = refused(json!({"error": {
        "code": "INTERNAL_ERROR",
        "message": "The message redirection to the Block Producer has failed: error sending request for url (http://producer/v2/messages) (source: operation timed out)"
    }}));
    assert_eq!(classify(&forwarding), CallErrorType::Unconfirmed);
    let ambiguous = anyhow::Error::new(super::super::client::MoneySubmitError::Ambiguous {
        source: refused(json!({"result": {"exit_code": 151}})),
    });
    assert_eq!(classify(&ambiguous), CallErrorType::Unconfirmed);
    let rejected = anyhow::Error::new(super::super::client::MoneySubmitError::Rejected {
        source: refused(json!({"result": {"exit_code": 151}})),
    });
    assert_eq!(classify(&rejected), CallErrorType::TvmCompute);
    assert_eq!(
        classify(&anyhow::Error::new(ChainError::OpenUnconfirmed(
            "0:2245".into()
        ))),
        CallErrorType::Unconfirmed
    );
}

#[tokio::test]
async fn an_answer_that_does_not_decode_is_decode() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let _ = socket.read(&mut request).await.unwrap();
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot-json")
            .await
            .unwrap();
    });
    let error = reqwest::Client::new()
        .post(format!("http://{address}/graphql"))
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap_err();
    assert!(error.is_decode(), "{error}");
    assert_eq!(
        classify(&anyhow::Error::new(error).context("parse account response")),
        CallErrorType::Decode
    );
    server.await.unwrap();
    assert_eq!(
        classify(&anyhow::anyhow!("getter getState: no decoded output")),
        CallErrorType::Decode
    );
}

#[test]
fn a_refusal_of_the_client_itself_is_client() {
    assert_eq!(
        classify(&anyhow::Error::new(super::super::client::ClockSkewRefusal(
            "local clock is 120s ahead of the chain".into()
        ))),
        CallErrorType::Client
    );
    assert_eq!(
        classify(&anyhow::anyhow!(
            "encode getter message getState: Wrong parameter"
        )),
        CallErrorType::Client
    );
}

/// Every function the client sends a contract of [`contracts`] as a signed message, by the ABI
/// it encodes with in `client.rs`.
const SENT: &[(&str, &[&str])] = &[
    (
        "TokenContract",
        &[
            "acceptProbe",
            "claimTokens",
            "close",
            "destroy",
            "finalize",
            "open",
            "releaseDispute",
            "resolveDisputeTimeout",
            "sellerStop",
            "settleWeek",
            "withdrawShell",
        ],
    ),
    (
        "PrivateNote",
        &[
            "cancelAllInferenceOrders",
            "cancelInferenceOrder",
            "cancelStake",
            "claim",
            "deleteStake",
            "deployDeal",
            "deployInferenceOrderBook",
            "deployPMP",
            "fundDeal",
            "fundDeployShell",
            "initTransfer",
            "placeInferenceBuy",
            "postSellOffer",
            "streamCleanup",
            "streamDispute",
            "streamStop",
            "sweepShell",
            "touchDeal",
            "withdrawTokens",
        ],
    ),
    ("InferenceOrderBook", &["expireOrder"]),
    ("SuperRoot", &["deployRootModel"]),
];

#[test]
fn every_function_the_client_sends_names_one_contract() {
    let mut owners: HashMap<u32, Vec<&str>> = HashMap::new();
    for (name, abi) in contracts() {
        let contract = tvm_abi::Contract::load(abi.as_bytes()).unwrap();
        for function in contract.functions().values() {
            let names = owners.entry(function.get_input_id()).or_default();
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    for (name, methods) in SENT {
        let abi = contracts()
            .into_iter()
            .find(|(contract, _)| contract == name)
            .unwrap()
            .1;
        let contract = tvm_abi::Contract::load(abi.as_bytes()).unwrap();
        for method in *methods {
            let id = contract
                .function(method)
                .unwrap_or_else(|_| panic!("{name} declares no {method}"))
                .get_input_id();
            assert_eq!(
                owners[&id],
                [*name],
                "{name}.{method} shares function id {id:#010x}"
            );
        }
    }
    // The shared ids exist, and they are what the next test sends.
    assert!(owners.values().any(|names| names.len() > 1));
}

#[tokio::test]
async fn a_message_two_contracts_could_take_names_no_contract() {
    use super::super::contracts_provision::TOKENCONTRACT_ABI;
    let account = format!("0:{}", "2245e".repeat(12) + "2245");
    let boc = signed(TOKENCONTRACT_ABI, &account, "getVersion", json!({})).await;
    assert_eq!(
        message_call(&boc),
        Some((UNKNOWN_CONTRACT, "getVersion".to_string()))
    );
}

async fn signed(abi: &str, account: &str, method: &str, args: serde_json::Value) -> String {
    let keys = gosh_ackinacki::sdk::KeyPair::from_secret_hex(&"22".repeat(32)).unwrap();
    gosh_ackinacki::airegistry::calls::encode_external_call(
        &gosh_ackinacki::airegistry::deploy::local_context().unwrap(),
        abi,
        account,
        method,
        args,
        keys.public_hex(),
        keys.secret_hex(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_failed_message_names_its_contract_and_function() {
    use super::super::contracts_provision::{PRIVATENOTE_ABI, TOKENCONTRACT_ABI};
    install();
    let note = format!("0:{}", "2245a".repeat(12) + "2245");
    let tc = format!("0:{}", "2245b".repeat(12) + "2245");
    let funding = signed(
        PRIVATENOTE_ABI,
        &note,
        "fundDeployShell",
        super::super::client::note_fund_deploy_shell_params(7, 11),
    )
    .await;
    let stop = signed(TOKENCONTRACT_ABI, &tc, "sellerStop", json!({})).await;
    message_failed(&funding, &refused(json!({"result": {"exit_code": 151}})));
    message_failed(
        &stop,
        &refused(json!({"result": {"action": {"result_code": 38}}})),
    );
    assert_eq!(
        reported_for(&note),
        [(
            "PrivateNote",
            "fundDeployShell".to_string(),
            CallErrorType::TvmCompute
        )]
    );
    assert_eq!(
        reported_for(&tc),
        [(
            "TokenContract",
            "sellerStop".to_string(),
            CallErrorType::TvmAction
        )]
    );
    // A message this client cannot read is still counted, under an unknown contract.
    let before = REPORTED.lock().unwrap().len();
    message_failed("not-a-boc", &status(503));
    assert!(REPORTED.lock().unwrap()[before..]
        .iter()
        .any(|(account, contract, method, kind)| {
            account.is_empty()
                && *contract == UNKNOWN_CONTRACT
                && method == UNKNOWN_METHOD
                && *kind == CallErrorType::Transport
        }));
}

#[test]
fn a_getter_names_its_contract_by_its_abi() {
    use super::super::contracts_provision::{INFERENCE_ORDERBOOK_ABI, TOKENCONTRACT_ABI};
    assert_eq!(contract_of_abi(TOKENCONTRACT_ABI), "TokenContract");
    assert_eq!(
        contract_of_abi(INFERENCE_ORDERBOOK_ABI),
        "InferenceOrderBook"
    );
    assert_eq!(
        contract_of_abi(crate::canonical_multisig::MULTISIG_ABI_JSON),
        "UpdateCustodianMultisigWallet_v2"
    );
    assert_eq!(contract_of_abi("{}"), UNKNOWN_CONTRACT);
}
