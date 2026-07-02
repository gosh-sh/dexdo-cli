use crate::cli::args::{PolicyArgs, PolicyCommand, PolicyRoleArg};
use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub(crate) const POLICY_VERSION: u64 = 1;

const BUYER_NO_HANDOVER: &[&str] = &["wait_then_reclaim", "next_seller", "fail_closed"];
const BUYER_MALFORMED_HANDOVER: &[&str] = &["reclaim", "dispute", "fail_closed"];
const BUYER_DEAD_GATEWAY: &[&str] = &["retry_then_reclaim", "next_seller", "fail_closed"];
const BUYER_EMPTY_STREAM: &[&str] = &["reclaim", "next_seller", "fail_closed"];
const BUYER_STALLS: &[&str] = &["accept_delivered_then_reclaim", "dispute"];
const BUYER_SCAM: &[&str] = &["stop", "dispute", "stop_and_blacklist"];
const SELLER_AFTER_DONE: &[&str] = &["republish", "republish_with_backoff", "retire"];
const SELLER_BUYER_NO_SHOW: &[&str] = &["cleanup_and_republish", "cleanup_and_retire"];
const SELLER_DISPUTE: &[&str] = &["release_if_clean", "hold"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RuntimeRole {
    Buyer,
    Seller,
}

impl RuntimeRole {
    fn command_name(self) -> &'static str {
        match self {
            RuntimeRole::Buyer => "buyer",
            RuntimeRole::Seller => "seller",
        }
    }
}

#[derive(Clone, Copy)]
enum FieldKind {
    Choice(&'static [&'static str]),
    IntegerAtLeast(u64),
}

impl FieldKind {
    fn allowed(self) -> String {
        match self {
            FieldKind::Choice(options) => options.join(" | "),
            FieldKind::IntegerAtLeast(1) => "integer >=1".to_string(),
            FieldKind::IntegerAtLeast(n) => format!("integer >={n}"),
        }
    }
}

#[derive(Clone, Copy)]
struct PolicyField {
    path: &'static str,
    kind: FieldKind,
}

const BUYER_FIELDS: &[PolicyField] = &[
    PolicyField {
        path: "buyer.on.no_handover_after_match",
        kind: FieldKind::Choice(BUYER_NO_HANDOVER),
    },
    PolicyField {
        path: "buyer.on.malformed_handover",
        kind: FieldKind::Choice(BUYER_MALFORMED_HANDOVER),
    },
    PolicyField {
        path: "buyer.on.dead_gateway",
        kind: FieldKind::Choice(BUYER_DEAD_GATEWAY),
    },
    PolicyField {
        path: "buyer.on.empty_stream",
        kind: FieldKind::Choice(BUYER_EMPTY_STREAM),
    },
    PolicyField {
        path: "buyer.on.seller_stalls_mid_stream",
        kind: FieldKind::Choice(BUYER_STALLS),
    },
    PolicyField {
        path: "buyer.on.bad_output_scam",
        kind: FieldKind::Choice(BUYER_SCAM),
    },
    PolicyField {
        path: "buyer.failover.max_sellers_to_try",
        kind: FieldKind::IntegerAtLeast(1),
    },
    PolicyField {
        path: "buyer.failover.total_spend_cap_shells",
        kind: FieldKind::IntegerAtLeast(1),
    },
];

const SELLER_FIELDS: &[PolicyField] = &[
    PolicyField {
        path: "seller.on.after_deal_done",
        kind: FieldKind::Choice(SELLER_AFTER_DONE),
    },
    PolicyField {
        path: "seller.on.buyer_no_show",
        kind: FieldKind::Choice(SELLER_BUYER_NO_SHOW),
    },
    PolicyField {
        path: "seller.on.dispute_against_me",
        kind: FieldKind::Choice(SELLER_DISPUTE),
    },
    PolicyField {
        path: "seller.max_open_deals",
        kind: FieldKind::IntegerAtLeast(1),
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoHandoverAfterMatchAction {
    WaitThenReclaim,
    NextSeller,
    FailClosed,
}

impl NoHandoverAfterMatchAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::WaitThenReclaim => "wait_then_reclaim",
            Self::NextSeller => "next_seller",
            Self::FailClosed => "fail_closed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BadOutputScamAction {
    Stop,
    Dispute,
    StopAndBlacklist,
}

impl BadOutputScamAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Dispute => "dispute",
            Self::StopAndBlacklist => "stop_and_blacklist",
        }
    }

    pub(crate) fn as_verification_action(self) -> dexdo::buyer::api::VerificationBailAction {
        match self {
            Self::Stop => dexdo::buyer::api::VerificationBailAction::Stop,
            Self::Dispute => dexdo::buyer::api::VerificationBailAction::Dispute,
            Self::StopAndBlacklist => dexdo::buyer::api::VerificationBailAction::StopAndBlacklist,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MalformedHandoverAction {
    Reclaim,
    Dispute,
    FailClosed,
}

impl MalformedHandoverAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Reclaim => "reclaim",
            Self::Dispute => "dispute",
            Self::FailClosed => "fail_closed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeadGatewayAction {
    RetryThenReclaim,
    NextSeller,
    FailClosed,
}

impl DeadGatewayAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::RetryThenReclaim => "retry_then_reclaim",
            Self::NextSeller => "next_seller",
            Self::FailClosed => "fail_closed",
        }
    }

    pub(crate) fn as_api_action(self) -> dexdo::buyer::api::DeadGatewayAction {
        match self {
            Self::RetryThenReclaim => dexdo::buyer::api::DeadGatewayAction::RetryThenReclaim,
            Self::NextSeller => dexdo::buyer::api::DeadGatewayAction::NextSeller,
            Self::FailClosed => dexdo::buyer::api::DeadGatewayAction::FailClosed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmptyStreamAction {
    Reclaim,
    NextSeller,
    FailClosed,
}

impl EmptyStreamAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Reclaim => "reclaim",
            Self::NextSeller => "next_seller",
            Self::FailClosed => "fail_closed",
        }
    }

    pub(crate) fn as_api_action(self) -> dexdo::buyer::api::EmptyStreamAction {
        match self {
            Self::Reclaim => dexdo::buyer::api::EmptyStreamAction::Reclaim,
            Self::NextSeller => dexdo::buyer::api::EmptyStreamAction::NextSeller,
            Self::FailClosed => dexdo::buyer::api::EmptyStreamAction::FailClosed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SellerStallsMidStreamAction {
    AcceptDeliveredThenReclaim,
    Dispute,
}

impl SellerStallsMidStreamAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::AcceptDeliveredThenReclaim => "accept_delivered_then_reclaim",
            Self::Dispute => "dispute",
        }
    }

    pub(crate) fn as_api_action(self) -> dexdo::buyer::api::SellerStallsMidStreamAction {
        match self {
            Self::AcceptDeliveredThenReclaim => {
                dexdo::buyer::api::SellerStallsMidStreamAction::AcceptDeliveredThenReclaim
            }
            Self::Dispute => dexdo::buyer::api::SellerStallsMidStreamAction::Dispute,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SellerAfterDealDoneAction {
    Republish,
    RepublishWithBackoff,
    Retire,
}

impl SellerAfterDealDoneAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Republish => "republish",
            Self::RepublishWithBackoff => "republish_with_backoff",
            Self::Retire => "retire",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SellerBuyerNoShowAction {
    CleanupAndRepublish,
    CleanupAndRetire,
}

impl SellerBuyerNoShowAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::CleanupAndRepublish => "cleanup_and_republish",
            Self::CleanupAndRetire => "cleanup_and_retire",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SellerDisputeAgainstMeAction {
    ReleaseIfClean,
    Hold,
}

impl SellerDisputeAgainstMeAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ReleaseIfClean => "release_if_clean",
            Self::Hold => "hold",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct BuyerRuntimePolicy {
    pub(crate) no_handover_after_match: NoHandoverAfterMatchAction,
    pub(crate) malformed_handover: MalformedHandoverAction,
    pub(crate) dead_gateway: DeadGatewayAction,
    pub(crate) empty_stream: EmptyStreamAction,
    pub(crate) seller_stalls_mid_stream: SellerStallsMidStreamAction,
    pub(crate) bad_output_scam: BadOutputScamAction,
    pub(crate) max_sellers_to_try: u64,
    pub(crate) total_spend_cap_shells: u64,
}

impl BuyerRuntimePolicy {
    pub(crate) fn as_api_failure_policy(&self) -> dexdo::buyer::api::BuyerApiFailurePolicy {
        dexdo::buyer::api::BuyerApiFailurePolicy {
            verification_bail: self.bad_output_scam.as_verification_action(),
            dead_gateway: self.dead_gateway.as_api_action(),
            empty_stream: self.empty_stream.as_api_action(),
            seller_stalls_mid_stream: self.seller_stalls_mid_stream.as_api_action(),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SellerRuntimePolicy {
    pub(crate) after_deal_done: SellerAfterDealDoneAction,
    pub(crate) buyer_no_show: SellerBuyerNoShowAction,
    pub(crate) dispute_against_me: SellerDisputeAgainstMeAction,
    pub(crate) max_open_deals: u64,
}

#[derive(Debug, Clone)]
struct PolicyProblem {
    key: String,
    allowed: String,
}

fn role_fields(role: RuntimeRole) -> &'static [PolicyField] {
    match role {
        RuntimeRole::Buyer => BUYER_FIELDS,
        RuntimeRole::Seller => SELLER_FIELDS,
    }
}

pub(crate) fn default_policy_path() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var_os("APPDATA")
            .ok_or_else(|| anyhow!("could not determine %APPDATA%; pass --policy/--path"))?;
        return Ok(PathBuf::from(appdata).join("dexdo").join("policy.json"));
    }
    #[cfg(not(target_os = "windows"))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(xdg).join("dexdo").join("policy.json"));
        }
        let home = std::env::var_os("HOME")
            .ok_or_else(|| anyhow!("could not determine $HOME; pass --policy/--path"))?;
        Ok(PathBuf::from(home)
            .join(".config")
            .join("dexdo")
            .join("policy.json"))
    }
}

fn resolve_policy_path(explicit: Option<&Path>) -> Result<PathBuf> {
    explicit
        .map(PathBuf::from)
        .map_or_else(default_policy_path, Ok)
}

fn read_policy(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow!("read policy {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| anyhow!("parse policy {}: {e}", path.display()))
}

fn get_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = value;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

fn set_missing_path(value: &mut Value, path: &str, new_value: Value) {
    if get_path(value, path).is_some() {
        return;
    }
    let mut cur = value;
    let mut parts = path.split('.').peekable();
    while let Some(part) = parts.next() {
        let is_leaf = parts.peek().is_none();
        if is_leaf {
            cur.as_object_mut()
                .expect("policy root is object")
                .insert(part.to_string(), new_value);
            return;
        }
        let obj = cur.as_object_mut().expect("policy root is object");
        cur = obj
            .entry(part.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !cur.is_object() {
            *cur = Value::Object(Map::new());
        }
    }
}

fn field_valid(value: Option<&Value>, kind: FieldKind) -> bool {
    match (value, kind) {
        (Some(Value::String(s)), _) if s == "UNSET" => false,
        (Some(Value::String(s)), FieldKind::Choice(options)) => options.contains(&s.as_str()),
        (Some(Value::Number(n)), FieldKind::IntegerAtLeast(min)) => {
            n.as_u64().is_some_and(|v| v >= min)
        }
        _ => false,
    }
}

fn problem(key: impl Into<String>, allowed: impl Into<String>) -> PolicyProblem {
    PolicyProblem {
        key: key.into(),
        allowed: allowed.into(),
    }
}

fn validate_object_key_set(value: &Value, path: &str, allowed_keys: &[&str]) -> Vec<PolicyProblem> {
    let Some(object) = value.as_object() else {
        return vec![problem(path, "object")];
    };
    object
        .keys()
        .filter(|key| !allowed_keys.contains(&key.as_str()))
        .map(|key| problem(format!("{path}.{key}"), "remove unknown field"))
        .collect()
}

fn validate_unknown_fields(value: &Value) -> Vec<PolicyProblem> {
    let Some(root) = value.as_object() else {
        return vec![problem("policy", "JSON object")];
    };
    let mut problems = Vec::new();
    problems.extend(validate_object_key_set(
        value,
        "policy",
        &["version", "buyer", "seller", "_legend"],
    ));
    if let Some(buyer) = root.get("buyer") {
        problems.extend(validate_object_key_set(buyer, "buyer", &["on", "failover"]));
        if let Some(on) = buyer.get("on") {
            problems.extend(validate_object_key_set(
                on,
                "buyer.on",
                &[
                    "no_handover_after_match",
                    "malformed_handover",
                    "dead_gateway",
                    "empty_stream",
                    "seller_stalls_mid_stream",
                    "bad_output_scam",
                ],
            ));
        }
        if let Some(failover) = buyer.get("failover") {
            problems.extend(validate_object_key_set(
                failover,
                "buyer.failover",
                &["max_sellers_to_try", "total_spend_cap_shells"],
            ));
        }
    }
    if let Some(seller) = root.get("seller") {
        problems.extend(validate_object_key_set(
            seller,
            "seller",
            &["on", "max_open_deals"],
        ));
        if let Some(on) = seller.get("on") {
            problems.extend(validate_object_key_set(
                on,
                "seller.on",
                &["after_deal_done", "buyer_no_show", "dispute_against_me"],
            ));
        }
    }
    problems
}

fn validate_value(value: &Value, role: RuntimeRole) -> Vec<PolicyProblem> {
    let mut problems = Vec::new();
    problems.extend(validate_unknown_fields(value));
    if !matches!(
        get_path(value, "version").and_then(Value::as_u64),
        Some(v) if v <= POLICY_VERSION
    ) {
        problems.push(problem("version", format!("integer 0..={POLICY_VERSION}")));
    }
    for field in role_fields(role) {
        if !field_valid(get_path(value, field.path), field.kind) {
            problems.push(problem(field.path, field.kind.allowed()));
        }
    }
    problems
}

fn format_incomplete_error(path: &Path, role: RuntimeRole, problems: &[PolicyProblem]) -> String {
    let mut out = format!(
        "policy ({}) is incomplete - dexdo {} will not place an order.\n",
        path.display(),
        role.command_name()
    );
    out.push_str("Unanswered/invalid (no defaults allowed):\n");
    for p in problems {
        out.push_str(&format!("  {} -> {}\n", p.key, p.allowed));
    }
    out.push_str("Run `dexdo policy init` to scaffold, fill every field, then retry.");
    out
}

pub(crate) fn validate_policy_file(explicit: Option<&Path>, role: RuntimeRole) -> Result<Value> {
    let path = resolve_policy_path(explicit)?;
    let value = match read_policy(&path) {
        Ok(value) => value,
        Err(e) => {
            bail!(
                "policy ({}) is missing or unreadable - dexdo {} will not place an order.\n\
                 Unanswered/invalid (no defaults allowed):\n  {}.* -> all required policy keys\n\
                 Run `dexdo policy init` to scaffold, fill every field, then retry.\nCause: {e}",
                path.display(),
                role.command_name(),
                role.command_name()
            );
        }
    };
    let problems = validate_value(&value, role);
    if !problems.is_empty() {
        bail!("{}", format_incomplete_error(&path, role, &problems));
    }
    Ok(value)
}

pub(crate) fn load_buyer_runtime_policy(explicit: Option<&Path>) -> Result<BuyerRuntimePolicy> {
    let value = validate_policy_file(explicit, RuntimeRole::Buyer)?;
    let choice = |key: &str| {
        get_path(&value, key)
            .and_then(Value::as_str)
            .expect("validated choice")
    };
    let int = |key: &str| {
        get_path(&value, key)
            .and_then(Value::as_u64)
            .expect("validated integer")
    };
    let no_handover_after_match = match choice("buyer.on.no_handover_after_match") {
        "wait_then_reclaim" => NoHandoverAfterMatchAction::WaitThenReclaim,
        "next_seller" => NoHandoverAfterMatchAction::NextSeller,
        "fail_closed" => NoHandoverAfterMatchAction::FailClosed,
        _ => unreachable!("validated choice"),
    };
    let malformed_handover = match choice("buyer.on.malformed_handover") {
        "reclaim" => MalformedHandoverAction::Reclaim,
        "dispute" => MalformedHandoverAction::Dispute,
        "fail_closed" => MalformedHandoverAction::FailClosed,
        _ => unreachable!("validated choice"),
    };
    let dead_gateway = match choice("buyer.on.dead_gateway") {
        "retry_then_reclaim" => DeadGatewayAction::RetryThenReclaim,
        "next_seller" => DeadGatewayAction::NextSeller,
        "fail_closed" => DeadGatewayAction::FailClosed,
        _ => unreachable!("validated choice"),
    };
    let empty_stream = match choice("buyer.on.empty_stream") {
        "reclaim" => EmptyStreamAction::Reclaim,
        "next_seller" => EmptyStreamAction::NextSeller,
        "fail_closed" => EmptyStreamAction::FailClosed,
        _ => unreachable!("validated choice"),
    };
    let seller_stalls_mid_stream = match choice("buyer.on.seller_stalls_mid_stream") {
        "accept_delivered_then_reclaim" => SellerStallsMidStreamAction::AcceptDeliveredThenReclaim,
        "dispute" => SellerStallsMidStreamAction::Dispute,
        _ => unreachable!("validated choice"),
    };
    let bad_output_scam = match choice("buyer.on.bad_output_scam") {
        "stop" => BadOutputScamAction::Stop,
        "dispute" => BadOutputScamAction::Dispute,
        "stop_and_blacklist" => BadOutputScamAction::StopAndBlacklist,
        _ => unreachable!("validated choice"),
    };
    Ok(BuyerRuntimePolicy {
        no_handover_after_match,
        malformed_handover,
        dead_gateway,
        empty_stream,
        seller_stalls_mid_stream,
        bad_output_scam,
        max_sellers_to_try: int("buyer.failover.max_sellers_to_try"),
        total_spend_cap_shells: int("buyer.failover.total_spend_cap_shells"),
    })
}

pub(crate) fn load_seller_runtime_policy(explicit: Option<&Path>) -> Result<SellerRuntimePolicy> {
    let value = validate_policy_file(explicit, RuntimeRole::Seller)?;
    let choice = |key: &str| {
        get_path(&value, key)
            .and_then(Value::as_str)
            .expect("validated choice")
    };
    let int = |key: &str| {
        get_path(&value, key)
            .and_then(Value::as_u64)
            .expect("validated integer")
    };
    let after_deal_done = match choice("seller.on.after_deal_done") {
        "republish" => SellerAfterDealDoneAction::Republish,
        "republish_with_backoff" => SellerAfterDealDoneAction::RepublishWithBackoff,
        "retire" => SellerAfterDealDoneAction::Retire,
        _ => unreachable!("validated choice"),
    };
    let buyer_no_show = match choice("seller.on.buyer_no_show") {
        "cleanup_and_republish" => SellerBuyerNoShowAction::CleanupAndRepublish,
        "cleanup_and_retire" => SellerBuyerNoShowAction::CleanupAndRetire,
        _ => unreachable!("validated choice"),
    };
    let dispute_against_me = match choice("seller.on.dispute_against_me") {
        "release_if_clean" => SellerDisputeAgainstMeAction::ReleaseIfClean,
        "hold" => SellerDisputeAgainstMeAction::Hold,
        _ => unreachable!("validated choice"),
    };
    Ok(SellerRuntimePolicy {
        after_deal_done,
        buyer_no_show,
        dispute_against_me,
        max_open_deals: int("seller.max_open_deals"),
    })
}

fn scaffold_roles(value: &mut Value, role: PolicyRoleArg) {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    set_missing_path(value, "version", Value::from(POLICY_VERSION));
    if get_path(value, "version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v < POLICY_VERSION)
    {
        value
            .as_object_mut()
            .unwrap()
            .insert("version".to_string(), Value::from(POLICY_VERSION));
    }
    set_missing_path(
        value,
        "_legend.policy_file",
        Value::from("Fill every field. UNSET is not accepted by real buyer/seller startup."),
    );
    set_missing_path(
        value,
        "_legend.default_path",
        Value::from("~/.config/dexdo/policy.json; Windows %APPDATA%\\dexdo\\policy.json"),
    );
    let add_buyer = matches!(role, PolicyRoleArg::Buyer | PolicyRoleArg::Both);
    let add_seller = matches!(role, PolicyRoleArg::Seller | PolicyRoleArg::Both);
    if add_buyer {
        for field in BUYER_FIELDS {
            set_missing_path(value, field.path, Value::from("UNSET"));
            set_missing_path(
                value,
                &format!("_legend.allowed.{}", field.path),
                Value::from(field.kind.allowed()),
            );
        }
    }
    if add_seller {
        for field in SELLER_FIELDS {
            set_missing_path(value, field.path, Value::from("UNSET"));
            set_missing_path(
                value,
                &format!("_legend.allowed.{}", field.path),
                Value::from(field.kind.allowed()),
            );
        }
    }
}

fn write_policy(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create policy directory {}: {e}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(value)?;
    std::fs::write(path, format!("{json}\n"))
        .map_err(|e| anyhow!("write policy {}: {e}", path.display()))
}

pub(crate) fn run_policy(args: PolicyArgs) -> Result<()> {
    match args.command {
        PolicyCommand::Init(args) => {
            let path = resolve_policy_path(args.path.as_deref())?;
            let mut value = if path.exists() {
                read_policy(&path)?
            } else {
                Value::Object(Map::new())
            };
            scaffold_roles(&mut value, args.role);
            write_policy(&path, &value)?;
            println!("policy initialized path={}", path.display());
            Ok(())
        }
        PolicyCommand::Show(args) => {
            let path = resolve_policy_path(args.path.as_deref())?;
            let value = read_policy(&path)?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        PolicyCommand::Edit(args) => {
            let path = resolve_policy_path(args.path.as_deref())?;
            if !path.exists() {
                let mut value = Value::Object(Map::new());
                scaffold_roles(&mut value, PolicyRoleArg::Both);
                write_policy(&path, &value)?;
            }
            let editor = std::env::var("VISUAL")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    std::env::var("EDITOR")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                })
                .ok_or_else(|| {
                    anyhow!(
                        "policy edit needs $VISUAL or $EDITOR; edit {} manually",
                        path.display()
                    )
                })?;
            let status = std::process::Command::new(editor)
                .arg(&path)
                .status()
                .map_err(|e| anyhow!("open editor for {}: {e}", path.display()))?;
            if !status.success() {
                bail!("editor exited with status {status}");
            }
            Ok(())
        }
    }
}

#[cfg(feature = "shellnet")]
pub(crate) fn doctor_policy_line(explicit: Option<&Path>) -> Result<String> {
    let path = resolve_policy_path(explicit)?;
    if !path.exists() {
        return Ok(format!("policy: missing ({})", path.display()));
    }
    let value = read_policy(&path)?;
    let mut problems = validate_value(&value, RuntimeRole::Buyer);
    problems.extend(validate_value(&value, RuntimeRole::Seller));
    problems.sort_by(|a, b| a.key.cmp(&b.key));
    problems.dedup_by(|a, b| a.key == b.key);
    if problems.is_empty() {
        Ok(format!("policy: OK ({})", path.display()))
    } else {
        Ok(format!(
            "policy: incomplete ({}) keys={}",
            path.display(),
            problems
                .iter()
                .map(|p| p.key.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ))
    }
}

#[cfg(test)]
pub(crate) fn dispatch_levers(key: &str, action: &str) -> &'static [&'static str] {
    match (key, action) {
        ("buyer.on.no_handover_after_match", "wait_then_reclaim") => {
            &["cleanup_unopened", "reclaim_command"]
        }
        ("buyer.on.no_handover_after_match", "next_seller") => {
            &["cleanup_unopened", "place_buy_by_model"]
        }
        ("buyer.on.no_handover_after_match", "fail_closed") => &["policy_fail_closed"],
        ("buyer.on.malformed_handover", "reclaim") => &["cleanup_unopened"],
        ("buyer.on.malformed_handover", "dispute") => &["stream_dispute"],
        ("buyer.on.malformed_handover", "fail_closed") => &["policy_fail_closed"],
        ("buyer.on.dead_gateway", "retry_then_reclaim") => &["retry_gateway", "seller_timeout"],
        ("buyer.on.dead_gateway", "next_seller") => &["seller_timeout", "place_buy_by_model"],
        ("buyer.on.dead_gateway", "fail_closed") => &["policy_fail_closed"],
        ("buyer.on.empty_stream", "reclaim") => &["seller_timeout"],
        ("buyer.on.empty_stream", "next_seller") => &["seller_timeout", "place_buy_by_model"],
        ("buyer.on.empty_stream", "fail_closed") => &["policy_fail_closed"],
        ("buyer.on.seller_stalls_mid_stream", "accept_delivered_then_reclaim") => {
            &["accept_delivered", "seller_timeout"]
        }
        ("buyer.on.seller_stalls_mid_stream", "dispute") => &["stream_dispute"],
        ("buyer.on.bad_output_scam", "stop") => &["stream_stop"],
        ("buyer.on.bad_output_scam", "dispute") => &["stream_dispute"],
        ("buyer.on.bad_output_scam", "stop_and_blacklist") => &["policy_fail_closed_unsupported"],
        ("seller.on.after_deal_done", "republish") => &["policy_fail_closed_unsupported"],
        ("seller.on.after_deal_done", "republish_with_backoff") => {
            &["policy_fail_closed_unsupported"]
        }
        ("seller.on.after_deal_done", "retire") => &["retire_offer"],
        ("seller.on.buyer_no_show", "cleanup_and_republish") => &["policy_fail_closed_unsupported"],
        ("seller.on.buyer_no_show", "cleanup_and_retire") => &["cleanup_unopened", "retire_offer"],
        ("seller.on.dispute_against_me", "release_if_clean") => &["release_dispute"],
        ("seller.on.dispute_against_me", "hold") => &["hold_dispute"],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn complete_policy() -> Value {
        json!({
            "version": 1,
            "buyer": {
                "on": {
                    "no_handover_after_match": "wait_then_reclaim",
                    "malformed_handover": "reclaim",
                    "dead_gateway": "retry_then_reclaim",
                    "empty_stream": "reclaim",
                    "seller_stalls_mid_stream": "accept_delivered_then_reclaim",
                    "bad_output_scam": "dispute"
                },
                "failover": {
                    "max_sellers_to_try": 1,
                    "total_spend_cap_shells": 1
                }
            },
            "seller": {
                "on": {
                    "after_deal_done": "republish",
                    "buyer_no_show": "cleanup_and_republish",
                    "dispute_against_me": "release_if_clean"
                },
                "max_open_deals": 1
            }
        })
    }

    #[test]
    fn complete_buyer_and_seller_policy_validate_silently() {
        let policy = complete_policy();
        assert!(validate_value(&policy, RuntimeRole::Buyer).is_empty());
        assert!(validate_value(&policy, RuntimeRole::Seller).is_empty());
    }

    #[test]
    fn missing_and_unset_policy_fail_closed_with_exact_keys() {
        let policy = json!({
            "version": 1,
            "buyer": {"on": {"dead_gateway": "UNSET"}, "failover": {"max_sellers_to_try": 1}}
        });
        let problems = validate_value(&policy, RuntimeRole::Buyer);
        let keys = problems.iter().map(|p| p.key.as_str()).collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                "buyer.on.no_handover_after_match",
                "buyer.on.malformed_handover",
                "buyer.on.dead_gateway",
                "buyer.on.empty_stream",
                "buyer.on.seller_stalls_mid_stream",
                "buyer.on.bad_output_scam",
                "buyer.failover.total_spend_cap_shells",
            ]
        );
        let msg =
            format_incomplete_error(Path::new("/tmp/policy.json"), RuntimeRole::Buyer, &problems);
        assert!(
            msg.contains("buyer.on.dead_gateway -> retry_then_reclaim | next_seller | fail_closed")
        );
        assert!(msg.contains("buyer.failover.total_spend_cap_shells -> integer >=1"));
    }

    #[test]
    fn old_version_with_new_missing_key_fails_only_that_key() {
        let mut policy = complete_policy();
        policy["version"] = Value::from(0);
        policy["buyer"]["on"]
            .as_object_mut()
            .unwrap()
            .remove("dead_gateway");
        let problems = validate_value(&policy, RuntimeRole::Buyer);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].key, "buyer.on.dead_gateway");
        assert_eq!(
            policy["buyer"]["on"]["bad_output_scam"],
            Value::from("dispute"),
            "old answers remain intact"
        );
    }

    #[test]
    fn future_policy_version_fails_closed_but_old_versions_keep_answers() {
        let mut policy = complete_policy();
        policy["version"] = Value::from(POLICY_VERSION + 1);
        let problems = validate_value(&policy, RuntimeRole::Buyer);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].key, "version");
        assert_eq!(problems[0].allowed, format!("integer 0..={POLICY_VERSION}"));

        policy["version"] = Value::from(0);
        assert!(
            validate_value(&policy, RuntimeRole::Buyer).is_empty(),
            "old complete policy answers remain valid until a new required key is missing"
        );
    }

    #[test]
    fn unknown_policy_fields_fail_closed_with_exact_keys() {
        let mut policy = complete_policy();
        policy
            .as_object_mut()
            .unwrap()
            .insert("debug".to_string(), Value::from(true));
        policy["buyer"]["on"]
            .as_object_mut()
            .unwrap()
            .insert("dead_gateway_alias".to_string(), Value::from("retry"));
        policy["seller"]
            .as_object_mut()
            .unwrap()
            .insert("implicit_defaults".to_string(), Value::from(false));

        let problems = validate_value(&policy, RuntimeRole::Buyer);
        let keys = problems.iter().map(|p| p.key.as_str()).collect::<Vec<_>>();
        assert_eq!(
            keys,
            vec![
                "policy.debug",
                "buyer.on.dead_gateway_alias",
                "seller.implicit_defaults",
            ]
        );
        assert!(problems.iter().all(|p| p.allowed == "remove unknown field"));
    }

    #[test]
    fn integer_policy_fields_reject_zero_string_and_missing() {
        let mut policy = complete_policy();
        policy["buyer"]["failover"]["max_sellers_to_try"] = Value::from(0);
        policy["buyer"]["failover"]["total_spend_cap_shells"] = Value::from("7");
        policy["seller"]["max_open_deals"] = Value::from(0);
        let buyer = validate_value(&policy, RuntimeRole::Buyer);
        assert_eq!(
            buyer.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(),
            vec![
                "buyer.failover.max_sellers_to_try",
                "buyer.failover.total_spend_cap_shells"
            ]
        );
        let seller = validate_value(&policy, RuntimeRole::Seller);
        assert_eq!(
            seller.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(),
            vec!["seller.max_open_deals"]
        );
    }

    #[test]
    fn init_preserves_existing_answers_and_adds_only_missing_fields() {
        let mut policy = json!({
            "version": 0,
            "buyer": {
                "on": {"bad_output_scam": "stop"},
                "failover": {"max_sellers_to_try": 2}
            }
        });
        scaffold_roles(&mut policy, PolicyRoleArg::Buyer);
        assert_eq!(policy["version"], Value::from(POLICY_VERSION));
        assert_eq!(
            policy["buyer"]["on"]["bad_output_scam"],
            Value::from("stop")
        );
        assert_eq!(
            policy["buyer"]["failover"]["max_sellers_to_try"],
            Value::from(2)
        );
        assert_eq!(policy["buyer"]["on"]["dead_gateway"], Value::from("UNSET"));
        assert!(
            policy.get("seller").is_none(),
            "--role buyer must not add seller fields"
        );
    }

    #[test]
    fn every_policy_choice_maps_to_existing_lever_name() {
        for field in BUYER_FIELDS.iter().chain(SELLER_FIELDS.iter()) {
            if let FieldKind::Choice(options) = field.kind {
                for action in options {
                    let levers = dispatch_levers(field.path, action);
                    assert!(
                        !levers.is_empty(),
                        "{}={action} has no existing lever mapping",
                        field.path
                    );
                    assert!(
                        levers.iter().all(|lever| !lever.trim().is_empty()),
                        "{}={action} has an empty lever name",
                        field.path
                    );
                }
            }
        }
    }

    #[test]
    fn buyer_runtime_policy_extracts_selected_actions() {
        let dir = std::env::temp_dir().join(format!(
            "dexdo-policy-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("policy.json");
        std::fs::write(&path, serde_json::to_string(&complete_policy()).unwrap()).unwrap();
        let policy = load_buyer_runtime_policy(Some(&path)).unwrap();
        assert_eq!(
            policy.no_handover_after_match,
            NoHandoverAfterMatchAction::WaitThenReclaim
        );
        assert_eq!(policy.malformed_handover, MalformedHandoverAction::Reclaim);
        assert_eq!(policy.dead_gateway, DeadGatewayAction::RetryThenReclaim);
        assert_eq!(policy.empty_stream, EmptyStreamAction::Reclaim);
        assert_eq!(
            policy.seller_stalls_mid_stream,
            SellerStallsMidStreamAction::AcceptDeliveredThenReclaim
        );
        assert_eq!(policy.bad_output_scam, BadOutputScamAction::Dispute);
        assert_eq!(
            policy.bad_output_scam.as_verification_action(),
            dexdo::buyer::api::VerificationBailAction::Dispute
        );
        assert_eq!(policy.max_sellers_to_try, 1);
        assert_eq!(policy.total_spend_cap_shells, 1);
        let seller_policy = load_seller_runtime_policy(Some(&path)).unwrap();
        assert_eq!(
            seller_policy.after_deal_done,
            SellerAfterDealDoneAction::Republish
        );
        assert_eq!(
            seller_policy.buyer_no_show,
            SellerBuyerNoShowAction::CleanupAndRepublish
        );
        assert_eq!(
            seller_policy.dispute_against_me,
            SellerDisputeAgainstMeAction::ReleaseIfClean
        );
        assert_eq!(seller_policy.max_open_deals, 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}
