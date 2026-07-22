use serde_json::Value;
use std::collections::HashSet;
use std::fmt;

const REDACTED: &str = "<redacted>";

#[derive(Debug, Clone, PartialEq)]
pub struct OnchainSubmitError {
    message: String,
    sanitized_payload: Value,
}

impl OnchainSubmitError {
    pub fn sanitized_payload(&self) -> &Value {
        &self.sanitized_payload
    }
}

impl fmt::Display for OnchainSubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OnchainSubmitError {}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExitCode {
    code: i64,
    stage: String,
}

pub fn validate_onchain_submit_response(resp: Value) -> Result<Value, OnchainSubmitError> {
    if resp.get("error").is_some_and(|v| !v.is_null()) {
        let sanitized = sanitize_onchain_submit_payload(&resp);
        let sanitized_err = sanitized
            .get("error")
            .expect("sanitizing a JSON object preserves its keys");
        return Err(OnchainSubmitError {
            message: block_manager_error_message(sanitized_err),
            sanitized_payload: sanitized,
        });
    }

    if let Some(exit) = first_nonzero_exit_code(&resp) {
        return Err(submit_error(exit_code_error_message(&exit), &resp));
    }

    if let Some(action) = first_nonzero_action_result_code(&resp) {
        return Err(submit_error(
            action_result_code_error_message(&action),
            &resp,
        ));
    }

    if value_at_path(&resp, &["result", "aborted"]).and_then(Value::as_bool) == Some(true) {
        return Err(submit_error(aborted_error_message(&resp), &resp));
    }

    Ok(resp)
}

pub fn sanitize_onchain_submit_payload(value: &Value) -> Value {
    let mut secrets = HashSet::new();
    collect_echo_secrets(value, &mut secrets);
    sanitize_value(value, &secrets)
}

pub fn contract_error_names(code: i64) -> &'static [&'static str] {
    match code {
        101 => &["dex::ERR_INVALID_SENDER"],
        102 => &["dex::ERR_LOW_VALUE"],
        103 => &["dex::ERR_ALREADY_RESOLVED"],
        107 => &["dex::ERR_ALREADY_INITIALIZED"],
        108 => &["dex::ERR_ALREADY_CLAIMED"],
        114 => &["dex::ERR_NOT_INITIALIZED"],
        115 => &["dex::ERR_NOT_WINNER"],
        116 => &["dex::ERR_NOT_APPROVED"],
        117 => &["dex::ERR_ALREADY_APPROVED"],
        118 => &["dex::ERR_INSUFFICIENT_NETWORK_FEE"],
        120 => &["dex::ERR_STAKE_PERIOD_ENDED"],
        121 => &["dex::ERR_NOTE_BUSY"],
        122 => &["dex::ERR_STAKE_NOT_APPROVED"],
        123 => &["dex::ERR_WRONG_DEADLINE"],
        124 => &["dex::ERR_STAKE_NOT_STARTED"],
        125 => &["dex::ERR_RESULT_NOT_STARTED"],
        126 => &["dex::ERR_RESULT_ENDED"],
        127 => &["dex::ERR_INVALID_CURRENCY_COUNT"],
        128 => &["dex::ERR_ZERO_TOKEN_AMOUNT"],
        129 => &["dex::ERR_INVALID_PARAMS"],
        130 => &["dex::ERR_INVALID_OUTCOME_ID"],
        131 => &["dex::ERR_OUTCOMES_NOT_SET"],
        132 => &["dex::ERR_ALREADY_CANCELLED"],
        133 => &["dex::ERR_NOT_CANCELLED"],
        134 => &["dex::ERR_LONG_ARRAY"],
        135 => &["dex::ERR_ALREADY_VOTED"],
        136 => &["dex::ERR_WRONG_HASH"],
        137 => &["dex::ERR_INVALID_ZKPROOF"],
        138 => &["dex::ERR_INVALID_TOKEN_TYPE"],
        139 => &["dex::ERR_NOT_APPROVED_BY_ORACLE"],
        140 => &["dex::ERR_PROPOSAL_NOT_EXISTS"],
        141 => &["dex::ERR_NOT_ALLOWED"],
        142 => &["dex::ERR_STAKE_NOT_EXISTS"],
        143 => &["dex::ERR_HAS_DEBT"],
        144 => &["dex::ERR_NON_ZERO_BALANCE"],
        145 => &["dex::ERR_COUPON_POOL_LIMIT_EXCEEDED"],
        146 => &["dex::ERR_NO_COUPON_AVAILABLE"],
        147 => &["dex::ERR_INVALID_BET_TYPE"],
        148 => &["dex::ERR_COUPON_ALREADY_EXISTS"],
        149 => &["dex::ERR_COUPON_ACTIVE"],
        150 => &["dex::ERR_DEBT_NON_ZERO"],
        151 => &["dex::ERR_INVALID_STATE"],
        152 => &["dex::ERR_DEPLOYER_NOT_COVERED"],
        153 => &["dex::ERR_NOT_FROZEN"],
        154 => &["dex::ERR_ALREADY_FROZEN"],
        155 => &["dex::ERR_MERGE_SOLVENCY"],
        156 => &["dex::ERR_NOT_STAKEEND"],
        157 => &["dex::ERR_INVALID_EPOCH"],
        158 => &["dex::ERR_ORDER_NOT_FOUND"],
        159 => &["dex::ERR_EPOCH_NOT_ENDED"],
        160 => &["dex::ERR_ORDER_TOO_SMALL"],
        161 => &["dex::ERR_BATCH_TOO_LARGE"],
        162 => &["dex::ERR_EMPTY_BATCH"],
        163 => &["dex::ERR_AMOUNT_NOT_LOT_MULTIPLE"],
        164 => &["dex::ERR_PRICE_NOT_TICK_MULTIPLE"],
        165 => &["dex::ERR_ORDERBOOK_NOT_SHUTDOWN"],
        166 => &["dex::ERR_INSOLVENT"],
        167 => &["dex::ERR_OPEN_ORDERS_EXIST"],
        168 => &["dex::ERR_NOTIONAL_OVERFLOW"],
        301 => &["airegistry::ERR_NOT_OWNER"],
        302 => &["airegistry::ERR_INVALID_SENDER"],
        303 => &["airegistry::ERR_ZERO_AMOUNT"],
        304 => &["airegistry::ERR_ALREADY_REGISTERED"],
        305 => &["airegistry::ERR_NOT_INITIALIZED"],
        306 => &["airegistry::ERR_INSUFFICIENT_TOKENS"],
        307 => &["airegistry::ERR_CONTRACT_LOCKED"],
        308 => &["airegistry::ERR_NOT_RESERVED"],
        309 => &["airegistry::ERR_RESERVATION_OVERFLOW"],
        310 => &["airegistry::ERR_NOT_EMPTY"],
        311 => &["airegistry::ERR_NO_SHELL"],
        312 => &["airegistry::ERR_BAD_FEE_BPS"],
        313 => &["airegistry::ERR_BAD_PARAM"],
        314 => &["airegistry::ERR_OVERFLOW"],
        315 => &["airegistry::ERR_FIRST_BATCH_LIMIT"],
        316 => &["airegistry::ERR_BAD_CODE_HASH"],
        317 => &["airegistry::ERR_SINGLE_SESSION_REQUIRED"],
        318 => &["airegistry::ERR_NOT_FUNDED"],
        319 => &["airegistry::ERR_ALREADY_FUNDED"],
        320 => &["airegistry::ERR_NOT_OPEN"],
        321 => &["airegistry::ERR_ALREADY_OPEN"],
        322 => &["airegistry::ERR_NOT_BUYER"],
        323 => &["airegistry::ERR_SETTLE_WINDOW_OPEN"],
        324 => &["airegistry::ERR_DISPUTED"],
        325 => &["airegistry::ERR_NOT_DISPUTED"],
        326 => &["airegistry::ERR_DISPUTE_WINDOW_OPEN"],
        327 => &["airegistry::ERR_STREAM_TIMEOUT_OPEN"],
        328 => &["airegistry::ERR_INSUFFICIENT_DEPOSIT"],
        329 => &["airegistry::ERR_STILL_OPEN"],
        332 => &["airegistry::ERR_PROBE_NOT_FUNDED"],
        333 => &[
            "airegistry::ERR_PROBE_ALREADY_FUNDED",
            "iob::ERR_NOT_DEPLOYER_NOTE",
        ],
        334 => &["airegistry::ERR_NOT_PROBE", "iob::ERR_NO_LIQUIDITY"],
        335 => &["airegistry::ERR_ALREADY_STREAMING", "iob::ERR_BAD_FLAGS"],
        336 => &["iob::ERR_EXPIRED"],
        337 => &["iob::ERR_FOK_UNFILLED"],
        338 => &["iob::ERR_NOT_SUB"],
        339 => &["iob::ERR_NOTHING_TO_CLAIM"],
        340 => &["iob::ERR_QUEUE_FULL"],
        341 => &["iob::ERR_NOT_SELF"],
        342 => &["iob::ERR_BAD_TOKEN_CONTRACT"],
        343 => &["iob::ERR_NAME_TOO_LONG"],
        344 => &["iob::ERR_BAD_MODEL_NAME"],
        400 => &["dex::ERR_MESSAGE_IS_EXIST"],
        401 => &["dex::ERR_MESSAGE_WITH_HUGE_EXPIREAT"],
        402 => &["dex::ERR_MESSAGE_EXPIRED"],
        403 => &["dex::ERR_INVALID_HISTORY_PROOF"],
        404 => &["dex::ERR_NORM_REFUND_PENDING"],
        405 => &["dex::ERR_STREAM_LOCKED"],
        406 => &["dex::ERR_BAD_TOKEN_CONTRACT"],
        _ => &[],
    }
}

fn submit_error(message: String, payload: &Value) -> OnchainSubmitError {
    OnchainSubmitError {
        message,
        sanitized_payload: sanitize_onchain_submit_payload(payload),
    }
}

fn block_manager_error_message(err: &Value) -> String {
    let code = value_to_string(err.get("code")).unwrap_or_else(|| "UNKNOWN".to_string());
    let message = value_to_string(err.get("message")).unwrap_or_else(|| "(no message)".to_string());
    let mut parts = vec![format!(
        "block manager rejected message code={code} message={message:?}"
    )];
    if let Some(exit) = first_nonzero_exit_code(err).or_else(|| first_exit_code(err)) {
        parts.push(exit_code_fragment(&exit));
    }
    if let Some(action) = first_nonzero_action_result_code(err).or_else(|| action_result_code(err))
    {
        parts.push(action_result_code_fragment(&action));
    }
    parts.extend(bm_detail_fragments(err));
    parts.push(format!(
        "tvm_sdk_error={}",
        sanitize_onchain_submit_payload(err)
    ));
    parts.join("; ")
}

fn exit_code_error_message(exit: &ExitCode) -> String {
    format!("on-chain submit failed: {}", exit_code_fragment(exit))
}

fn action_result_code_error_message(action: &ExitCode) -> String {
    format!(
        "on-chain submit failed: {}",
        action_result_code_fragment(action)
    )
}

fn aborted_error_message(resp: &Value) -> String {
    let mut parts = vec!["on-chain submit failed: aborted=true".to_string()];
    if let Some(exit) = first_nonzero_exit_code(resp).or_else(|| first_exit_code(resp)) {
        parts.push(exit_code_fragment(&exit));
    }
    if let Some(action) = action_result_code(resp) {
        parts.push(action_result_code_fragment(&action));
    }
    parts.join("; ")
}

fn exit_code_fragment(exit: &ExitCode) -> String {
    let label = contract_error_label(exit.code)
        .map(|l| format!(" ({l})"))
        .unwrap_or_default();
    format!("exit_code={}{} stage={}", exit.code, label, exit.stage)
}

fn action_result_code_fragment(action: &ExitCode) -> String {
    let label = action_result_code_label(action.code)
        .map(|l| format!(" ({l})"))
        .unwrap_or_default();
    format!(
        "action_result_code={}{} stage={}",
        action.code, label, action.stage
    )
}

fn action_result_code_label(code: i64) -> Option<String> {
    if code == 38 {
        return Some("insufficient extra currency / no_funds".to_string());
    }
    contract_error_label(code)
}

fn contract_error_label(code: i64) -> Option<String> {
    let names = contract_error_names(code);
    (!names.is_empty()).then(|| names.join("|"))
}

fn value_to_string(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn bm_detail_fragments(err: &Value) -> Vec<String> {
    let Some(data) = err.get("data") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for key in [
        "category",
        "raw_category",
        "phase",
        "stage",
        "vm_step",
        "vmStep",
        "vm_step_name",
    ] {
        if let Some(value) = find_named_value(data, key).and_then(|v| value_to_string(Some(v))) {
            out.push(format!("{key}={value}"));
        }
    }
    out
}

fn first_exit_code(value: &Value) -> Option<ExitCode> {
    all_exit_codes(value).into_iter().next()
}

fn first_nonzero_exit_code(value: &Value) -> Option<ExitCode> {
    all_exit_codes(value)
        .into_iter()
        .find(|exit| exit.code != 0)
}

fn all_exit_codes(value: &Value) -> Vec<ExitCode> {
    let mut out = Vec::new();
    collect_exit_codes_recursive(value, "", &mut out);
    out
}

fn action_result_code(value: &Value) -> Option<ExitCode> {
    all_action_result_codes(value).into_iter().next()
}

fn first_nonzero_action_result_code(value: &Value) -> Option<ExitCode> {
    all_action_result_codes(value)
        .into_iter()
        .find(|action| action.code != 0)
}

fn all_action_result_codes(value: &Value) -> Vec<ExitCode> {
    let mut out = Vec::new();
    collect_action_result_codes_recursive(value, "", &mut out);
    out
}

fn collect_exit_codes_recursive(value: &Value, path: &str, out: &mut Vec<ExitCode>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let stage = if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{path}.{key}")
                };
                if is_exit_code_key(key) {
                    if let Some(code) = value_i64(child) {
                        out.push(ExitCode {
                            code,
                            stage: path.to_string(),
                        });
                    }
                }
                collect_exit_codes_recursive(child, &stage, out);
            }
        }
        Value::Array(items) => items.iter().enumerate().for_each(|(i, child)| {
            collect_exit_codes_recursive(child, &format!("{path}[{i}]"), out)
        }),
        _ => {}
    }
}

fn is_exit_code_key(key: &str) -> bool {
    matches!(
        key,
        "exit_code" | "exitCode" | "vm_exit_code" | "vmExitCode"
    )
}

fn collect_action_result_codes_recursive(value: &Value, path: &str, out: &mut Vec<ExitCode>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let stage = if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{path}.{key}")
                };
                if is_action_result_code_key(key) {
                    if let Some(code) = value_i64(child) {
                        out.push(ExitCode {
                            code,
                            stage: path.to_string(),
                        });
                    }
                }
                collect_action_result_codes_recursive(child, &stage, out);
            }
        }
        Value::Array(items) => items.iter().enumerate().for_each(|(i, child)| {
            collect_action_result_codes_recursive(child, &format!("{path}[{i}]"), out)
        }),
        _ => {}
    }
}

fn is_action_result_code_key(key: &str) -> bool {
    matches!(
        key,
        "result_code" | "resultCode" | "action_result_code" | "actionResultCode"
    )
}

fn find_named_value<'a>(value: &'a Value, wanted: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == wanted {
                    return Some(child);
                }
                if let Some(found) = find_named_value(child, wanted) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(items) => items
            .iter()
            .find_map(|child| find_named_value(child, wanted)),
        _ => None,
    }
}

fn value_at_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = value;
    for segment in path {
        cur = cur.get(*segment)?;
    }
    Some(cur)
}

fn value_i64(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    if let Some(n) = value.as_u64() {
        return i64::try_from(n).ok();
    }
    let s = value.as_str()?.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<i64>().ok()
    }
}

fn collect_echo_secrets(value: &Value, secrets: &mut HashSet<String>) {
    match value {
        Value::Object(map) => map.iter().for_each(|(key, child)| {
            if is_credential_key(key) && !is_ext_message_token_key(key) {
                collect_strings(child, secrets);
            } else {
                collect_echo_secrets(child, secrets);
            }
        }),
        Value::Array(items) => items
            .iter()
            .for_each(|child| collect_echo_secrets(child, secrets)),
        _ => {}
    }
}

fn is_ext_message_token_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "extmessagetoken" | "ext_message_token"
    )
}

fn collect_strings(value: &Value, secrets: &mut HashSet<String>) {
    match value {
        Value::String(secret) if !secret.is_empty() => {
            secrets.insert(secret.clone());
        }
        Value::Object(map) => map
            .values()
            .for_each(|child| collect_strings(child, secrets)),
        Value::Array(items) => items
            .iter()
            .for_each(|child| collect_strings(child, secrets)),
        _ => {}
    }
}

fn sanitize_value(value: &Value, secrets: &HashSet<String>) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, child)| {
                    let child = if is_credential_key(key) {
                        Value::String(REDACTED.to_string())
                    } else {
                        sanitize_value(child, secrets)
                    };
                    (key.clone(), child)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|child| sanitize_value(child, secrets))
                .collect(),
        ),
        Value::String(text) => Value::String(mask_exact_secrets(text, secrets)),
        _ => value.clone(),
    }
}

fn is_credential_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "extmessagetoken"
            | "ext_message_token"
            | "authorization"
            | "accesstoken"
            | "access_token"
            | "refreshtoken"
            | "refresh_token"
            | "api_key"
            | "apikey"
            | "provider_api_key"
            | "providerapikey"
            | "secret"
            | "secretkey"
            | "secret_key"
            | "secret_hash"
            | "seed"
            | "seedphrase"
            | "seed_phrase"
            | "mnemonic"
            | "password"
            | "password_hash"
            | "passwd"
            | "privatekey"
            | "private_key"
            | "signature"
            | "unsigned"
            | "signedmessagebody"
            | "signed_message_body"
            | "messageboc"
            | "message_boc"
            | "signedboc"
            | "signed_boc"
    )
}

fn mask_exact_secrets(text: &str, secrets: &HashSet<String>) -> String {
    let mut masked = text.to_string();
    let mut secrets = secrets
        .iter()
        .filter(|secret| secret.len() >= 8)
        .collect::<Vec<_>>();
    secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
    for secret in secrets {
        masked = masked.replace(secret, REDACTED);
    }
    masked
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn zero_exit_code_stays_successful() {
        let resp = json!({"result": {"exit_code": 0, "aborted": false, "tx_hash": "abc"}});
        assert_eq!(
            validate_onchain_submit_response(resp.clone()).unwrap(),
            resp
        );
    }

    #[test]
    fn all_zero_nested_codes_stay_successful() {
        let resp = json!({
            "result": {
                "exit_code": 0,
                "compute": {"exit_code": 0},
                "vm": {"exit_code": 0},
                "action": {"result_code": 0},
                "aborted": false,
                "tx_hash": "abc"
            }
        });
        assert_eq!(
            validate_onchain_submit_response(resp.clone()).unwrap(),
            resp
        );
    }

    #[test]
    fn nonzero_result_exit_code_fails_with_number() {
        let err = validate_onchain_submit_response(json!({"result": {"exit_code": 321}}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("exit_code=321"), "{err}");
        assert!(err.contains("stage=result"), "{err}");
    }

    #[test]
    fn wrapper_zero_nested_compute_nonzero_fails() {
        let err = validate_onchain_submit_response(json!({
            "result": {
                "exit_code": 0,
                "compute": {"exit_code": 321},
                "aborted": false
            }
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("exit_code=321"), "{err}");
        assert!(err.contains("airegistry::ERR_ALREADY_OPEN"), "{err}");
        assert!(err.contains("stage=result.compute"), "{err}");
    }

    #[test]
    fn wrapper_zero_nested_camelcase_compute_nonzero_fails() {
        let err = validate_onchain_submit_response(json!({
            "result": {
                "exitCode": 0,
                "compute": {"exitCode": 321},
                "vm": {"exitCode": 0},
                "aborted": false
            }
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("exit_code=321"), "{err}");
        assert!(err.contains("airegistry::ERR_ALREADY_OPEN"), "{err}");
        assert!(err.contains("stage=result.compute"), "{err}");
    }

    #[test]
    fn wrapper_zero_nested_camelcase_vm_nonzero_fails() {
        let err = validate_onchain_submit_response(json!({
            "result": {
                "exitCode": 0,
                "compute": {"exitCode": 0},
                "vm": {"exitCode": 322},
                "aborted": false
            }
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("exit_code=322"), "{err}");
        assert!(err.contains("airegistry::ERR_NOT_BUYER"), "{err}");
        assert!(err.contains("stage=result.vm"), "{err}");
    }

    #[test]
    fn wrapper_zero_action_result_nonzero_fails() {
        let err = validate_onchain_submit_response(json!({
            "result": {
                "exit_code": 0,
                "compute": {"exit_code": 0},
                "action": {"result_code": 38},
                "aborted": false
            }
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("action_result_code=38"), "{err}");
        assert!(
            err.contains("insufficient extra currency / no_funds"),
            "{err}"
        );
        assert!(!err.contains("ECC[2]"), "{err}");
        assert!(err.contains("no_funds"), "{err}");
        assert!(err.contains("stage=result.action"), "{err}");
    }

    #[test]
    fn block_manager_action_no_funds_keeps_sanitized_diagnostics() {
        let err = validate_onchain_submit_response(json!({
            "error": {
                "code": "TVM_ERROR",
                "message": "transaction aborted",
                "data": {
                    "transaction": {
                        "aborted": true,
                        "compute": {"exit_code": 0},
                        "action": {"success": false, "result_code": 38, "no_funds": true}
                    },
                    "transaction_hash": "tx-public-480",
                    "signature": "secret-signature-480"
                }
            }
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("action_result_code=38"), "{err}");
        assert!(
            err.contains("insufficient extra currency / no_funds"),
            "{err}"
        );
        assert!(!err.contains("ECC[2]"), "{err}");
        assert!(err.contains("no_funds"), "{err}");
        assert!(err.contains("stage=data.transaction.action"), "{err}");
        assert!(err.contains("transaction_hash"), "{err}");
        assert!(err.contains("tx-public-480"), "{err}");
        assert!(err.contains(REDACTED), "{err}");
        assert!(!err.contains("secret-signature-480"), "{err}");
    }

    #[test]
    fn wrapper_zero_camelcase_action_result_nonzero_fails() {
        let err = validate_onchain_submit_response(json!({
            "result": {
                "exitCode": 0,
                "compute": {"exitCode": 0},
                "action": {"resultCode": 38},
                "aborted": false
            }
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("action_result_code=38"), "{err}");
        assert!(err.contains("stage=result.action"), "{err}");
    }

    #[test]
    fn known_exit_code_maps_to_contract_error_name() {
        let err = validate_onchain_submit_response(json!({"result": {"exit_code": 321}}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("airegistry::ERR_ALREADY_OPEN"), "{err}");
    }

    #[test]
    fn unknown_exit_code_keeps_number_and_stage() {
        let err = validate_onchain_submit_response(json!({"result": {"exit_code": 777}}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("exit_code=777"), "{err}");
        assert!(err.contains("stage=result"), "{err}");
    }

    #[test]
    fn bm_tvm_error_keeps_structured_detail() {
        let err = validate_onchain_submit_response(json!({
            "error": {
                "code": "TVM_ERROR",
                "message": "compute phase failed",
                "data": {
                    "category": "tvm",
                    "phase": "compute",
                    "vm_step": "execute",
                    "compute": {"exit_code": 321}
                }
            }
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("code=TVM_ERROR"), "{err}");
        assert!(err.contains("message=\"compute phase failed\""), "{err}");
        assert!(err.contains("exit_code=321"), "{err}");
        assert!(err.contains("airegistry::ERR_ALREADY_OPEN"), "{err}");
        assert!(err.contains("phase=compute"), "{err}");
        assert!(err.contains("vm_step=execute"), "{err}");
    }

    #[test]
    fn production_submit_tvm_error_is_diagnostic_and_credential_safe() {
        for (exit_code, contract_label) in [
            (137, "dex::ERR_INVALID_ZKPROOF"),
            (403, "dex::ERR_INVALID_HISTORY_PROOF"),
        ] {
            let continuation = format!("continuation-token-{exit_code}-long");
            let signature = format!("message-signature-{exit_code}-long");
            let message = format!("submission rejected; prefix{continuation}suffix");
            let submit_error = validate_onchain_submit_response(json!({
                "result": null,
                "error": {
                    "code": "TVM_ERROR",
                    "message": message,
                    "data": {
                        "exit_code": exit_code,
                        "phase": "compute",
                        "vm_error": "contract execution failed",
                        "signature": signature,
                        "message_hash": "msg-public-deadbeef",
                        "current_time": "1752345678",
                        "thread_id": "thread-public-cafebabe"
                    }
                },
                "ext_message_token": {
                    "unsigned": continuation,
                    "signature": signature,
                    "issuer": {"bm": "public-ish-issuer"}
                }
            }))
            .unwrap_err();
            let direct = submit_error.to_string();
            let chained = format!("{:#}", anyhow::Error::new(submit_error));

            for displayed in [&direct, &chained] {
                for expected in [
                    "code=TVM_ERROR",
                    "phase=compute",
                    "contract execution failed",
                    "msg-public-deadbeef",
                    "current_time",
                    "1752345678",
                    "thread-public-cafebabe",
                    REDACTED,
                    contract_label,
                ] {
                    assert!(
                        displayed.contains(expected),
                        "missing {expected}: {displayed}"
                    );
                }
                assert!(
                    displayed.contains(&format!("exit_code={exit_code}")),
                    "{displayed}"
                );
                assert!(!displayed.contains(&continuation), "{displayed}");
                assert!(!displayed.contains(&signature), "{displayed}");
            }
        }
    }

    #[test]
    fn tvm_sdk_621_wrapper_remains_diagnostic_and_credential_safe() {
        const GENERIC_MESSAGE: &str = "Message failed during the compute phase";
        for (exit_code, contract_label) in [
            (137, "dex::ERR_INVALID_ZKPROOF"),
            (403, "dex::ERR_INVALID_HISTORY_PROOF"),
        ] {
            let continuation = format!("synthetic-continuation-{exit_code}");
            let signature = format!("synthetic-signature-{exit_code}");
            let unsigned = format!("synthetic-unsigned-{exit_code}");
            let signed_boc = format!("synthetic-signed-boc-{exit_code}");
            // Shape produced by pinned tvm-sdk's
            // Error::try_extract_send_messages_error/send_message_server_error.
            let submit_error = validate_onchain_submit_response(json!({
                "error": {
                    "code": 621,
                    "message": GENERIC_MESSAGE,
                    "data": {
                        "node_error": {
                            "extensions": {
                                "code": "TVM_ERROR",
                                "message": GENERIC_MESSAGE,
                                "details": {
                                    "phase": "compute",
                                    "vm_error": "contract execution failed",
                                    "exit_code": exit_code,
                                    "transaction_hash": "tx-deadbeef",
                                    "message_hash": "msg-cafebabe",
                                    "signed_boc": signed_boc
                                }
                            }
                        },
                        "ext_message_token": {
                            "unsigned": unsigned,
                            "signature": signature,
                            "issuer": {"bm": continuation}
                        }
                    }
                }
            }))
            .unwrap_err();
            let direct = submit_error.to_string();
            let chained = format!("{:#}", anyhow::Error::new(submit_error));

            for displayed in [&direct, &chained] {
                assert!(
                    displayed.contains(&format!("exit_code={exit_code}")),
                    "{displayed}"
                );
                assert!(
                    displayed.contains("stage=data.node_error.extensions.details"),
                    "{displayed}"
                );
                assert!(displayed.contains(contract_label), "{displayed}");
                assert!(displayed.contains(GENERIC_MESSAGE), "{displayed}");
                assert!(
                    displayed.contains("contract execution failed"),
                    "{displayed}"
                );
                assert!(displayed.contains("phase=compute"), "{displayed}");
                assert!(displayed.contains("tx-deadbeef"), "{displayed}");
                assert!(displayed.contains("msg-cafebabe"), "{displayed}");
                assert!(displayed.contains(REDACTED), "{displayed}");
                for credential in [&continuation, &signature, &unsigned, &signed_boc] {
                    assert!(!displayed.contains(credential), "{displayed}");
                }
            }
        }
    }

    #[test]
    fn sanitized_payload_redacts_secret_fields() {
        let err = validate_onchain_submit_response(json!({
            "error": {
                "code": "TVM_ERROR",
                "message": "compute phase failed",
                "data": {
                    "seed_phrase": "alpha beta gamma",
                    "provider_api_key": "sk-live",
                    "messageboc": "te6ccgEBAQEAAAAA",
                    "ext_message_token": {
                        "unsigned": "synthetic-unsigned",
                        "signature": "synthetic-signature",
                        "issuer": {"bm": "synthetic-continuation"}
                    },
                    "nested": [{"refresh_token": "synthetic-refresh"}],
                    "signed_boc": "synthetic-signed-boc",
                    "public_key": "0xabc",
                    "phase": "compute"
                }
            }
        }))
        .unwrap_err();
        let sanitized = err.sanitized_payload();
        assert_eq!(sanitized["error"]["data"]["seed_phrase"], REDACTED);
        assert_eq!(sanitized["error"]["data"]["provider_api_key"], REDACTED);
        assert_eq!(sanitized["error"]["data"]["messageboc"], REDACTED);
        assert_eq!(sanitized["error"]["data"]["ext_message_token"], REDACTED);
        assert_eq!(
            sanitized["error"]["data"]["nested"][0]["refresh_token"],
            REDACTED
        );
        assert_eq!(sanitized["error"]["data"]["signed_boc"], REDACTED);
        assert_eq!(sanitized["error"]["data"]["public_key"], "0xabc");
        assert_eq!(sanitized["error"]["message"], "compute phase failed");
    }

    #[test]
    fn structured_credential_echoed_in_upstream_message_is_not_displayed() {
        let credential = "synthetic-reusable-signature";
        let err = validate_onchain_submit_response(json!({
            "error": {
                "code": 621,
                "message": format!("rejected signature {credential}"),
                "data": {
                    "node_error": {
                        "extensions": {
                            "code": "TVM_ERROR",
                            "details": {
                                "phase": "compute",
                                "exit_code": 137,
                                "signature": credential
                            }
                        }
                    }
                }
            }
        }))
        .unwrap_err()
        .to_string();

        assert!(err.contains("signature"), "{err}");
        assert!(err.contains(REDACTED), "{err}");
        assert!(!err.contains(credential), "{err}");
    }

    #[test]
    fn signed_message_body_is_redacted_but_plain_body_is_public() {
        let sanitized = sanitize_onchain_submit_payload(&json!({
            "signed_message_body": "synthetic-signed-message-boc",
            "signedMessageBody": "synthetic-compact-signed-message-boc",
            "body": "execution failed"
        }));

        assert_eq!(sanitized["signed_message_body"], REDACTED);
        assert_eq!(sanitized["signedMessageBody"], REDACTED);
        assert_eq!(sanitized["body"], "execution failed");
    }

    #[test]
    fn echo_masking_ignores_short_values_and_masks_mid_word_occurrences() {
        let short = sanitize_onchain_submit_payload(&json!({
            "signature": "x",
            "message": "execution failed for x"
        }));
        assert_eq!(short["message"], "execution failed for x");

        let token = "reusable-token-505";
        let long = sanitize_onchain_submit_payload(&json!({
            "ext_message_token": {
                "unsigned": token,
                "signature": "reusable-signature-505",
                "issuer": {"bm": "public-ish-issuer"}
            },
            "message": format!("execution failed for {token}; prefix{token}suffix")
        }));
        assert_eq!(
            long["message"],
            format!("execution failed for {REDACTED}; prefix{REDACTED}suffix")
        );
    }

    #[test]
    fn echoed_signed_boc_is_not_displayed() {
        let signed_boc = "te6ccgEBAQEA-reusable-signed-boc";
        let displayed = validate_onchain_submit_response(json!({
            "error": {
                "code": "TVM_ERROR",
                "message": format!("submission rejected: prefix{signed_boc}suffix"),
                "data": {
                    "exit_code": 137,
                    "signed_message_body": signed_boc
                }
            }
        }))
        .unwrap_err()
        .to_string();

        assert!(displayed.contains(REDACTED), "{displayed}");
        assert!(!displayed.contains(signed_boc), "{displayed}");
    }

    #[test]
    fn final_display_uses_structural_redaction_and_exact_value_masking() {
        let credentials = [
            "Bearer auth-X",
            "camel-api-X",
            "correct horse battery staple",
            "object-ext-X",
            "object-signature-X",
            "object-unsigned-X",
            "object-boc-X",
            "password-hash-X",
            "secret-hash-X",
        ];
        let message = format!(
            "upstream echoed {}; providerApiKey={}; password rejected; signature verification failed",
            credentials[0], credentials[1]
        );

        for (exit_code, contract_label) in [
            (137, "dex::ERR_INVALID_ZKPROOF"),
            (403, "dex::ERR_INVALID_HISTORY_PROOF"),
        ] {
            let displayed = validate_onchain_submit_response(json!({
                "error": {
                    "code": 621,
                    "message": message,
                    "data": {
                        "node_error": {"extensions": {
                            "code": "TVM_ERROR",
                            "details": {
                                "exit_code": exit_code,
                                "authorization": credentials[0],
                                "providerApiKey": credentials[1],
                                "password": credentials[2],
                                "ext_message_token": {
                                    "unsigned": credentials[3],
                                    "signature": credentials[4],
                                    "issuer": {"bm": "public-ish-issuer"}
                                },
                                "signature": credentials[4],
                                "unsigned": {"body": credentials[5]},
                                "signed_boc": credentials[6],
                                "password_hash": credentials[7],
                                "secret_hash": credentials[8],
                                "token_type": "NACKL",
                                "token_contract": "0:public-contract",
                                "token_amount": 42,
                                "completion_tokens": 17,
                                "signature_status": "checked",
                                "signature_valid": false,
                                "transaction_hash": "tx-public",
                                "message_hash": "msg-public",
                                "diagnostic": "signature verification failed"
                            }
                        }}
                    }
                }
            }))
            .unwrap_err()
            .to_string();

            for credential in credentials {
                assert!(!displayed.contains(credential), "{displayed}");
            }
            for public in [
                "token_type",
                "token_contract",
                "token_amount",
                "completion_tokens",
                "signature_status",
                "signature_valid",
                "tx-public",
                "msg-public",
                "signature verification failed",
            ] {
                assert!(displayed.contains(public), "missing {public}: {displayed}");
            }
            assert!(
                displayed.contains(&format!("exit_code={exit_code}")),
                "{displayed}"
            );
            assert!(displayed.contains(contract_label), "{displayed}");
        }
    }
}
