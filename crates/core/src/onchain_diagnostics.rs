use serde_json::Value;
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
    if let Some(err) = resp.get("error").filter(|v| !v.is_null()) {
        return Err(submit_error(block_manager_error_message(err), &resp));
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
    sanitize_value(value, None)
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
    parts.extend(bm_detail_fragments(err));
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
    let label = contract_error_label(action.code)
        .map(|l| format!(" ({l})"))
        .unwrap_or_default();
    format!(
        "action_result_code={}{} stage={}",
        action.code, label, action.stage
    )
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

fn sanitize_value(value: &Value, key: Option<&str>) -> Value {
    if key.is_some_and(is_secret_key) {
        return Value::String(REDACTED.to_string());
    }
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let v = if is_secret_key(k) {
                        Value::String(REDACTED.to_string())
                    } else {
                        sanitize_value(v, Some(k))
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|v| sanitize_value(v, key)).collect()),
        Value::String(s) if looks_like_raw_signed_body(s) => Value::String(REDACTED.to_string()),
        _ => value.clone(),
    }
}

fn is_secret_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase().replace(['-', '_'], "");
    k.contains("secret")
        || k.contains("seed")
        || k.contains("phrase")
        || k.contains("mnemonic")
        || k.contains("private")
        || k.contains("apikey")
        || k.contains("providertoken")
        || k.contains("accesstoken")
        || k.contains("refreshtoken")
        || k.contains("authtoken")
        || k.contains("authorization")
        || k.contains("password")
        || k.contains("signedmessage")
        || k.contains("messageboc")
        || matches!(k.as_str(), "key" | "boc" | "body" | "payload")
        || (k.ends_with("key") && !k.contains("pub") && !k.contains("public"))
}

fn looks_like_raw_signed_body(s: &str) -> bool {
    s.len() > 512
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'-' | b'_'))
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
        assert!(err.contains("stage=result.action"), "{err}");
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
    fn sanitized_payload_redacts_secret_fields() {
        let err = validate_onchain_submit_response(json!({
            "error": {
                "code": "TVM_ERROR",
                "message": "compute phase failed",
                "data": {
                    "seed_phrase": "alpha beta gamma",
                    "provider_api_key": "sk-live",
                    "signed_message_body": "te6ccgEBAQEAAAAA",
                    "public_key": "0xabc",
                    "phase": "compute"
                }
            }
        }))
        .unwrap_err();
        let sanitized = err.sanitized_payload();
        assert_eq!(sanitized["error"]["data"]["seed_phrase"], REDACTED);
        assert_eq!(sanitized["error"]["data"]["provider_api_key"], REDACTED);
        assert_eq!(sanitized["error"]["data"]["signed_message_body"], REDACTED);
        assert_eq!(sanitized["error"]["data"]["public_key"], "0xabc");
        assert_eq!(sanitized["error"]["message"], "compute phase failed");
    }
}
