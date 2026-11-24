//! Failed contract calls by type: the one record the buyer's and the seller's
//! `contract.call.errors` counters are built from.

//! A contract call is a getter run or a signed external message sent to a contract. A call is
//! reported once, when it fails, after the client's own repeats of it: a read the client retried
//! and then got an answer to reports nothing, and a read that ran out of attempts reports once, by
//! its last failure. A message is reported once per send, by what that send ended with.

//! Nothing is reported until a role installs its observer with [`observe`], so every command but
//! `dexdo buyer` and `dexdo seller` reports nothing, and decoding which contract a failed message
//! was meant for costs nothing on the success path.
use crate::market::ChainError;
use crate::onchain_diagnostics::{OnchainSubmitError, SubmitRefusal};
use gosh_ackinacki::sdk::Address;
use std::sync::OnceLock;

/// How a contract call failed. A closed list: every failure is exactly one of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CallErrorType {
    /// The node was not reached or the connection broke: DNS, TCP, TLS, HTTP 5xx.
    Transport,
    /// The node did not answer within the time the client gives it.
    Timeout,
    /// The node answered with an error: a GraphQL error, an HTTP refusal, or a message it refused
    /// before executing it.
    NodeRejected,
    /// The transaction aborted or its compute phase exited non-zero; a getter failed in the TVM.
    TvmCompute,
    /// The action phase returned a non-zero result code, for example no funds for an outbound send.
    TvmAction,
    /// The message may have reached the chain, and no result for it was observed in time.
    Unconfirmed,
    /// The answer did not decode: not the JSON or the ABI shape the call expects.
    Decode,
    /// The client refused the call before sending it: the message did not encode by its ABI, or
    /// the machine clock disagrees with the chain beyond the signed-message window.
    Client,
}

impl CallErrorType {
    pub const ALL: [Self; 8] = [
        Self::Transport,
        Self::Timeout,
        Self::NodeRejected,
        Self::TvmCompute,
        Self::TvmAction,
        Self::Unconfirmed,
        Self::Decode,
        Self::Client,
    ];

    /// The `error.type` label value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Timeout => "timeout",
            Self::NodeRejected => "node_rejected",
            Self::TvmCompute => "tvm_compute",
            Self::TvmAction => "tvm_action",
            Self::Unconfirmed => "unconfirmed",
            Self::Decode => "decode",
            Self::Client => "client",
        }
    }
}

/// One failed contract call. `account` is the called account's bare hex id, handed to the observer
/// so a role can relate the call to its own deals; it is never a label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailedCall<'a> {
    pub contract: &'static str,
    pub method: &'a str,
    pub account: &'a str,
    pub error_type: CallErrorType,
}

/// `contract` when neither the ABI nor the message names a contract this client knows.
pub const UNKNOWN_CONTRACT: &str = "unknown";
/// `method` when a failed message carries no function this client can name.
pub const UNKNOWN_METHOD: &str = "unknown";

static OBSERVER: OnceLock<fn(&FailedCall<'_>)> = OnceLock::new();

/// Install the one observer of this process. A second call is ignored and returns `false`.
pub fn observe(observer: fn(&FailedCall<'_>)) -> bool {
    OBSERVER.set(observer).is_ok()
}

/// The contracts the client calls, under their ABI file names. A message is matched to its
/// contract by the function id it carries. Two contracts share an id when they declare the same
/// function -- a getter (`getVersion`, for one), the constructor, `updateCode` -- and such a
/// message names no contract. `every_function_the_client_sends_names_one_contract` proves no
/// function the client sends is one of them.
fn contracts() -> [(&'static str, &'static str); 9] {
    use super::contracts_provision::{
        INFERENCE_ORDERBOOK_ABI, PRIVATENOTE_ABI, ROOTMODEL_ABI, ROOTPN_ABI, SUPERROOT_ABI,
        TOKENCONTRACT_ABI,
    };
    [
        ("TokenContract", TOKENCONTRACT_ABI),
        ("PrivateNote", PRIVATENOTE_ABI),
        (
            "PrivateNote",
            crate::private_note::artifacts::PRIVATE_NOTE_ABI_JSON,
        ),
        ("InferenceOrderBook", INFERENCE_ORDERBOOK_ABI),
        ("RootPN", ROOTPN_ABI),
        ("RootPN", crate::private_note::artifacts::ROOT_PN_ABI_JSON),
        ("RootModel", ROOTMODEL_ABI),
        ("SuperRoot", SUPERROOT_ABI),
        (
            "UpdateCustodianMultisigWallet_v2",
            crate::canonical_multisig::MULTISIG_ABI_JSON,
        ),
    ]
}

fn contract_of_abi(abi: &str) -> &'static str {
    contracts()
        .into_iter()
        .find(|(_, known)| *known == abi)
        .map_or(UNKNOWN_CONTRACT, |(name, _)| name)
}

/// The contract and the function a signed external message calls, by its function id. A function
/// two contracts declare alike names its function and no contract.
fn message_call(boc: &str) -> Option<(&'static str, String)> {
    use base64::Engine as _;
    use tvm_block::Deserializable as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(boc.trim())
        .ok()?;
    let cell = tvm_types::read_single_root_boc(&bytes).ok()?;
    let body = tvm_block::Message::construct_from_cell(cell).ok()?.body()?;
    let mut matched: Option<(&'static str, String)> = None;
    for (name, abi) in contracts() {
        let Some(decoded) = tvm_abi::Contract::load(abi.as_bytes())
            .ok()
            .and_then(|contract| contract.decode_input(body.clone(), false, true).ok())
        else {
            continue;
        };
        match &matched {
            Some((first, _)) if *first != name => {
                return Some((UNKNOWN_CONTRACT, decoded.function_name));
            }
            Some(_) => {}
            None => matched = Some((name, decoded.function_name)),
        }
    }
    matched
}

fn report(contract: &'static str, method: &str, account: &str, error_type: CallErrorType) {
    if let Some(observer) = OBSERVER.get() {
        observer(&FailedCall {
            contract,
            method,
            account,
            error_type,
        });
    }
}

/// A message to `account` by `abi` was refused before it was sent, by the client itself.
pub(crate) fn refused_before_send(account: &Address, abi: &str, method: &str) {
    if OBSERVER.get().is_some() {
        report(
            contract_of_abi(abi),
            method,
            account.bare(),
            CallErrorType::Client,
        );
    }
}

/// A getter by `abi` on `account` failed, or a message to it failed before it was sent.
pub(crate) fn call_failed(account: &Address, abi: &str, method: &str, error: &anyhow::Error) {
    if OBSERVER.get().is_some() {
        report(
            contract_of_abi(abi),
            method,
            account.bare(),
            classify(error),
        );
    }
}

/// The send of the signed message `boc` failed with `error`.
pub(crate) fn message_failed(boc: &str, error: &anyhow::Error) {
    if OBSERVER.get().is_none() {
        return;
    }
    let account = gosh_ackinacki::wallet::query::dest_account_id_hex(boc).unwrap_or_default();
    let (contract, method) =
        message_call(boc).unwrap_or((UNKNOWN_CONTRACT, UNKNOWN_METHOD.to_string()));
    report(contract, &method, &account, classify(error));
}

/// A sent message to `account` by `abi` got no result within the confirmation budget.
pub(crate) fn unconfirmed(account: &Address, abi: &str, method: &str) {
    if OBSERVER.get().is_some() {
        report(
            contract_of_abi(abi),
            method,
            account.bare(),
            CallErrorType::Unconfirmed,
        );
    }
}

/// The type of a failed call, from the first cause in `error` that says how it failed.
pub fn classify(error: &anyhow::Error) -> CallErrorType {
    // A send whose outcome is unknown stays unknown whatever refusal text it carries: the money
    // path wraps a forwarding timeout or a lost response as ambiguous because it may have landed.
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<super::client::MoneySubmitError>()
            .is_some_and(super::client::MoneySubmitError::is_ambiguous)
            || matches!(
                cause.downcast_ref::<ChainError>(),
                Some(ChainError::AmbiguousSubmit(_) | ChainError::OpenUnconfirmed(_))
            )
    }) {
        return CallErrorType::Unconfirmed;
    }
    for cause in error.chain() {
        if let Some(refused) = cause.downcast_ref::<OnchainSubmitError>() {
            if super::client::forwarding_timed_out(refused) {
                return CallErrorType::Unconfirmed;
            }
            return match refused.refusal() {
                SubmitRefusal::Node => CallErrorType::NodeRejected,
                SubmitRefusal::Compute => CallErrorType::TvmCompute,
                SubmitRefusal::Action => CallErrorType::TvmAction,
            };
        }
        if cause.is::<tokio::time::error::Elapsed>() {
            return CallErrorType::Timeout;
        }
        if cause.is::<super::client::ClockSkewRefusal>() {
            return CallErrorType::Client;
        }
        if cause.is::<super::client::MessagePostResponseDecodeError>() {
            return CallErrorType::Decode;
        }
        if cause.is::<super::client::ChainHttpResponseError>()
            || cause.is::<super::client::GraphQlBodyError>()
        {
            return CallErrorType::NodeRejected;
        }
        if let Some(error) = cause.downcast_ref::<reqwest::Error>() {
            return if error.is_timeout() {
                CallErrorType::Timeout
            } else if error.is_decode() {
                CallErrorType::Decode
            } else if error
                .status()
                .is_some_and(|status| !status.is_server_error())
            {
                CallErrorType::NodeRejected
            } else {
                CallErrorType::Transport
            };
        }
        // The pinned SDK flattens three getter failures into text. Only these exact prefixes it
        // writes are read, never arbitrary prose.
        let text = cause.to_string();
        if text.starts_with("GraphQL errors: ") {
            return CallErrorType::NodeRejected;
        }
        if text.starts_with("run_tvm getter ") {
            return CallErrorType::TvmCompute;
        }
        if text.starts_with("encode getter message ") {
            return CallErrorType::Client;
        }
    }
    // What is left is an answer that arrived and could not be used: a field missing from it, an
    // account without its BOC, a getter that returned no output.
    CallErrorType::Decode
}

#[cfg(test)]
#[path = "call_errors_2245_tests.rs"]
pub(crate) mod tests;
