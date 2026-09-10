use crate::cli::args::{PolicyArgs, PolicyCommand, PolicyRoleArg, PolicyValidateRoleArg};
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
const SELLER_BUYER_NO_SHOW: &[&str] = &[
    "cleanup_and_republish",
    "cleanup_and_retire",
    "retire_gateway",
];
const SELLER_DISPUTE: &[&str] = &["release_if_clean", "hold"];
const SELLER_RUNTIME_AFTER_DONE: &[&str] = &["retire"];
const SELLER_RUNTIME_BUYER_NO_SHOW: &[&str] = &["retire_gateway"];

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

    fn runtime_supported_values() -> &'static [&'static str] {
        SELLER_RUNTIME_AFTER_DONE
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SellerBuyerNoShowAction {
    CleanupAndRepublish,
    CleanupAndRetire,
    RetireGateway,
}

impl SellerBuyerNoShowAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::CleanupAndRepublish => "cleanup_and_republish",
            Self::CleanupAndRetire => "cleanup_and_retire",
            Self::RetireGateway => "retire_gateway",
        }
    }

    fn runtime_supported_values() -> &'static [&'static str] {
        SELLER_RUNTIME_BUYER_NO_SHOW
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

/// What to offer an operator who has to fill this field in.

/// The schema accepts more than the runtime executes: `seller.on.after_deal_done` parses
/// `republish` and `republish_with_backoff`, and `seller.on.buyer_no_show` parses
/// `cleanup_and_republish` and `cleanup_and_retire`, but this daemon cannot perform a fresh-TC
/// republish or a buyer-side cleanup, and refuses them at startup.

/// Listing them as plain choices sends the operator to fill the policy with a value that fails a
/// minute later, at a place that names a different field. So the ones the runtime executes are
/// offered first, and the rest are named as what they are -- parsed, not executable today.
fn allowed_for_operator(path: &str, kind: FieldKind) -> String {
    let runtime: &[&str] = match path {
        "seller.on.after_deal_done" => SELLER_RUNTIME_AFTER_DONE,
        "seller.on.buyer_no_show" => SELLER_RUNTIME_BUYER_NO_SHOW,
        _ => return kind.allowed(),
    };
    let FieldKind::Choice(all) = kind else {
        return kind.allowed();
    };
    let rest = all
        .iter()
        .filter(|option| !runtime.contains(*option))
        .copied()
        .collect::<Vec<_>>();
    if rest.is_empty() {
        return runtime.join(" | ");
    }
    format!(
        "{} (accepted but not executable by this runtime: {})",
        runtime.join(" | "),
        rest.join(" | ")
    )
}

pub(crate) fn validate_seller_runtime_capabilities(policy: &SellerRuntimePolicy) -> Result<()> {
    let unsupported = [
        (
            "seller.on.after_deal_done",
            policy.after_deal_done.as_str(),
            SellerAfterDealDoneAction::runtime_supported_values(),
        ),
        (
            "seller.on.buyer_no_show",
            policy.buyer_no_show.as_str(),
            SellerBuyerNoShowAction::runtime_supported_values(),
        ),
    ]
    .into_iter()
    .filter(|(_, selected, supported)| !supported.contains(selected))
    .map(|(field, selected, _)| format!("{field}={selected}"))
    .collect::<Vec<_>>();
    if !unsupported.is_empty() {
        bail!(
            "policy_action failure_class=policy_validation action=fail_closed token_contract=<not-posted> \
             state=pre_offer result=unsupported_policy_choice runtime=seller unsupported_choices={} \
             next_action=edit_policy diagnostic=seller runtime cannot execute fresh-TC republish or \
             buyer-side cleanup_unopened from this seller daemon before/following an offer; supported seller \
             terminal actions today are seller.on.after_deal_done=retire and \
             seller.on.buyer_no_show=retire_gateway",
            unsupported.join(",")
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct PolicyProblem {
    key: String,
    allowed: String,
}

/// Every field a role's policy file must carry, by path.

/// Read from the same table the validator reads, so a question set built against this cannot ask
/// for a field the file does not need, or miss one it does. That is the whole guarantee: an
/// operator who answers every question ends up with a file that loads.
#[cfg(test)]
pub(crate) fn required_paths(role: RuntimeRole) -> Vec<&'static str> {
    role_fields(role).iter().map(|field| field.path).collect()
}

/// Would the validator accept `value` for `path`? Answered through the same rule it uses.

/// For the questions that offer choices: an answer whose wording an operator reads has to carry a
/// value the file accepts, and nothing but this table decides which those are.
#[cfg(test)]
pub(crate) fn accepts(path: &str, value: &str) -> bool {
    let optional = seller_chain_unavailable_field();
    let Some(field) = [RuntimeRole::Seller, RuntimeRole::Buyer]
        .into_iter()
        .flat_map(role_fields)
        .copied()
        .chain(std::iter::once(optional))
        .find(|field| field.path == path)
    else {
        return false;
    };
    field_valid(Some(&Value::from(value)), field.kind)
}

fn role_fields(role: RuntimeRole) -> &'static [PolicyField] {
    match role {
        RuntimeRole::Buyer => BUYER_FIELDS,
        RuntimeRole::Seller => SELLER_FIELDS,
    }
}

fn seller_chain_unavailable_field() -> PolicyField {
    PolicyField {
        path: "seller.on.chain_unavailable",
        kind: FieldKind::Choice(dexdo::seller::gateway::ChainUnavailableAction::supported_values()),
    }
}

pub(crate) fn default_policy_path() -> Result<PathBuf> {
    if let Some(root) = crate::cli::data_dir::explicit() {
        return Ok(root.join("policy.json"));
    }
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

fn set_path(value: &mut Value, path: &str, new_value: Value) {
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

fn set_missing_path(value: &mut Value, path: &str, new_value: Value) {
    if get_path(value, path).is_none() {
        set_path(value, path, new_value);
    }
}

fn refresh_seller_legend(value: &mut Value) {
    set_path(
        value,
        "_legend.allowed.seller.on.after_deal_done",
        Value::from(SellerAfterDealDoneAction::runtime_supported_values().join(" | ")),
    );
    set_path(
        value,
        "_legend.allowed.seller.on.buyer_no_show",
        Value::from(SellerBuyerNoShowAction::runtime_supported_values().join(" | ")),
    );
    set_path(
        value,
        "_legend.allowed.seller.on.chain_unavailable",
        Value::from(dexdo::seller::gateway::ChainUnavailableAction::supported_values().join(" | ")),
    );
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
                &[
                    "after_deal_done",
                    "buyer_no_show",
                    "dispute_against_me",
                    "chain_unavailable",
                ],
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
            problems.push(problem(
                field.path,
                allowed_for_operator(field.path, field.kind),
            ));
        }
    }
    let chain_unavailable = seller_chain_unavailable_field();
    if role == RuntimeRole::Seller
        && get_path(value, chain_unavailable.path).is_some()
        && !field_valid(
            get_path(value, chain_unavailable.path),
            chain_unavailable.kind,
        )
    {
        problems.push(problem(
            chain_unavailable.path,
            chain_unavailable.kind.allowed(),
        ));
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

/// Ask the operator the rules their role needs, and hand back the file with the answers in it.

/// The questions are situations rather than field paths ([`super::policy_questions`]), and the
/// values they produce are the ones this module already validates -- a test pins both, so an answer
/// that would not load cannot be offered.

/// `base` is whatever is on disk, so answering fills the gaps rather than replacing choices the
/// operator has already made.
fn ask_the_rules(role: RuntimeRole, base: &Value) -> Result<Value> {
    use crate::cli::policy_questions::{
        BUYER_COUNTS, BUYER_QUESTIONS, SELLER_COUNTS, SELLER_QUESTIONS,
    };

    let (questions, counts) = match role {
        RuntimeRole::Buyer => (BUYER_QUESTIONS, BUYER_COUNTS),
        RuntimeRole::Seller => (SELLER_QUESTIONS, SELLER_COUNTS),
    };
    let mut value = base.clone();
    scaffold_roles(
        &mut value,
        match role {
            RuntimeRole::Buyer => PolicyRoleArg::Buyer,
            RuntimeRole::Seller => PolicyRoleArg::Seller,
        },
    );
    eprintln!(
        "{}",
        crate::cli::choose::title(&format!(
            "Setting up the {} rules -- what to do when the other side lets you down",
            role.command_name()
        ))
    );
    eprintln!(
        "{}",
        crate::cli::choose::note(&[
            "Asked once. The answers are written to this instance's policy file,",
            "and no command asks again unless you change them.",
            "",
            "They are needed in advance because every one of these happens",
            "while money is already committed.",
        ])
    );
    for question in questions {
        // Already answered with something valid: leave it alone. Re-asking would invite an operator
        // to change a rule they came here for an unrelated reason.
        if field_valid(get_path(&value, question.path), field_kind(question.path)) {
            continue;
        }
        // Only what this role's runtime can carry out. Where that leaves one answer there is no
        // question to ask: it is stated and set, because a menu of one is a question with no
        // decision in it.
        let offered = question.offering(runtime_supported(question.path));
        // A question this runtime can carry out no answer to is the interview's own defect, not the
        // operator's: it is refused rather than skipped. The line here used to CONSTRUCT that error
        // and drop it on the floor, so the run went on and ended in the generic "the rules are not
        // filled in" -- which sends an operator to fill a field the client would refuse anyway.
        let Some((first, _)) = offered.first() else {
            bail!(
                "{}: this runtime can carry out none of the answers to that question, so the rules \
                 cannot be completed by asking. Fill the file by hand or upgrade the client.",
                question.path
            );
        };
        eprintln!();
        eprintln!("{}", crate::cli::choose::heading(question.situation));
        eprintln!("{}", crate::cli::choose::aside(question.because));
        if offered.len() == 1 {
            eprintln!(
                "{}",
                crate::cli::choose::answered(&format!(
                    "{} -- the only thing this client can do today",
                    first.says
                ))
            );
            set_path(&mut value, question.path, Value::from(first.value));
            continue;
        }
        let rows: Vec<String> = offered
            .iter()
            .map(|(answer, suggested)| {
                if *suggested {
                    format!("{} (suggested)", answer.says)
                } else {
                    answer.says.to_string()
                }
            })
            .collect();
        let picked = crate::cli::choose::ask("What should it do?", rows)?
            .ok_or_else(|| anyhow!("no answer chosen; nothing was written"))?;
        // No panic on a path an operator is standing on: the rules file decides what a money command
        // does when the other side misbehaves, and a client that aborts here leaves them with a
        // backtrace instead of an answer. The invariant is real -- what was offered came from these
        // very answers -- so a break of it is the client's own fault, said as such.
        let chosen = question
            .answers
            .iter()
            .position(|answer| answer.value == offered[picked].0.value)
            .ok_or_else(|| {
                anyhow!(
                    "{}: the answer picked is not one this question offers, which is a defect in \
                     this client; nothing was written",
                    question.path
                )
            })?;
        // The menu is erased on the way out, so the answer is left behind in its place: an
        // interview that shows only the questions afterwards reads as though nothing was decided.
        eprintln!(
            "{}",
            crate::cli::choose::answered(question.answers[chosen].says)
        );
        set_path(
            &mut value,
            question.path,
            Value::from(question.answers[chosen].value),
        );
    }
    for count in counts {
        if field_valid(get_path(&value, count.path), field_kind(count.path)) {
            continue;
        }
        eprintln!();
        eprintln!("{}", crate::cli::choose::heading(count.situation));
        eprintln!("{}", crate::cli::choose::aside(count.because));
        let answer = crate::cli::choose::ask_number(
            &format!("  {}", count.unit),
            count.suggested,
            count.least,
        )?;
        record_count(&mut value, count, answer)?;
    }
    Ok(value)
}

/// Whether the number this count asks for is stated in SHELL, while the field it fills holds raw
/// ECC[2] units.

/// Path-keyed, like [`runtime_supported`] and `field_kind` beside it, because the unit belongs to
/// the FIELD rather than to the wording of the question: `total_spend_cap_shells` is compared
/// against `escrow * attempt` in raw units (`cli::buyer`), the tests that write it write raw, and
/// the published buyer document states it in raw. The interview asks in SHELL because that is what
/// a person has; the two meet here and nowhere else.

/// Exactly one count is money today. A second one added to the interview without a line here would
/// be recorded a billion times small, so a test pins this list against the prompts' own units.
fn count_is_stated_in_shell(path: &str) -> bool {
    matches!(path, "buyer.failover.total_spend_cap_shells")
}

/// Put one number the operator stated into the rules file -- the boundary where an interview answer
/// becomes a value on disk, and the only place the two units meet.

/// A count stated in SHELL is converted here and nowhere later, through the very parser `--escrow`,
/// `--budget` and `--amount` carry (`cli::args::parse_shell_amount`, over
/// `dexdo_core::shell_amount_raw`): one grammar for every SHELL figure a person states, one place
/// that knows what a SHELL is worth in raw units, and the same refusal by name for a figure that is
/// really a stale raw one -- an operator pasting `24600000000` out of the buyer document into a
/// prompt asking for SHELL is told so instead of having it multiplied again.

/// Everything else is a count of things -- deals, sellers -- and is written exactly as answered.

/// the interview asked "how much may be spent in total, in SHELL?", suggested `20`, and wrote
/// `20` into a field the runtime reads as 20 raw ECC[2] units -- two hundredths of a microSHELL. The
/// first failover attempt projected a spend of some 8e9 against it and the client stopped with
/// `result=total_spend_cap_reached`, naming the operator's own ceiling for what was a unit defect.
fn record_count(
    value: &mut Value,
    count: &crate::cli::policy_questions::Count,
    answer: u64,
) -> Result<()> {
    let recorded = if count_is_stated_in_shell(count.path) {
        let raw = crate::cli::args::parse_shell_amount(&answer.to_string())
            .map_err(|why| anyhow!("{}: {why}", count.path))?;
        // Refused rather than truncated: the rules file holds this as a JSON integer the loader
        // reads with `as_u64`, and a figure that does not fit is one no run could honour.
        u64::try_from(raw).map_err(|_| {
            anyhow!(
                "{}: {answer} SHELL is beyond the range this field can hold",
                count.path
            )
        })?
    } else {
        answer
    };
    set_path(value, count.path, Value::from(recorded));
    Ok(())
}

/// The answers a role's RUNTIME can actually carry out today, where that is narrower than what the
/// file accepts.

/// The two are not the same thing, and the gap is not cosmetic: a seller whose policy says
/// `after_deal_done=republish` writes a valid file and then refuses to start, because the daemon
/// cannot republish onto a fresh TokenContract. An interview that offered those answers would walk
/// the operator into that refusal, which is exactly what it exists to prevent.

/// `None` where every accepted value is executable.
pub(crate) fn runtime_supported(path: &str) -> Option<&'static [&'static str]> {
    match path {
        "seller.on.after_deal_done" => Some(SELLER_RUNTIME_AFTER_DONE),
        "seller.on.buyer_no_show" => Some(SELLER_RUNTIME_BUYER_NO_SHOW),
        _ => None,
    }
}

/// What the validator accepts for one path, so the interview can tell an answered field from an
/// unanswered one by exactly the same rule.
fn field_kind(path: &str) -> FieldKind {
    [RuntimeRole::Seller, RuntimeRole::Buyer]
        .into_iter()
        .flat_map(role_fields)
        .find(|field| field.path == path)
        .map(|field| field.kind)
        .unwrap_or(FieldKind::IntegerAtLeast(1))
}

/// Read and check, and never write: what `policy validate` runs.

/// The asking variant below fills the gaps in place, which is right when a command is on its way to
/// spending and wrong for a command whose whole job is to report. A check that repairs what it
/// checks can only ever answer "fine".
pub(crate) fn inspect_policy_file(explicit: Option<&Path>, role: RuntimeRole) -> Result<Value> {
    let path = resolve_policy_path(explicit)?;
    let value = read_policy(&path)?;
    let problems = validate_value(&value, role);
    if problems.is_empty() {
        return Ok(value);
    }
    bail!("{}", format_incomplete_error(&path, role, &problems));
}

pub(crate) fn validate_policy_file(explicit: Option<&Path>, role: RuntimeRole) -> Result<Value> {
    let path = resolve_policy_path(explicit)?;
    // Missing is not the same as unreadable. A file that is not there yet can be written from the
    // operator's answers; one that exists and cannot be parsed is a file to look at, never to
    // overwrite -- it may hold rules somebody set deliberately.
    let absent = !path.exists();
    let value = match read_policy(&path) {
        Ok(value) => value,
        Err(_) if absent && crate::cli::interaction::may_ask() => {
            let answered = ask_the_rules(role, &serde_json::json!({}))?;
            write_policy(&path, &answered)?;
            eprintln!("rules written to {}", path.display());
            answered
        }
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
    if problems.is_empty() {
        return Ok(value);
    }
    // Incomplete, and somebody is there to answer: ask for the gaps rather than send them off to
    // edit JSON by hand. `dexdo policy init` writes every field as `UNSET`, so this is the state an
    // operator following the instructions lands in.
    if crate::cli::interaction::may_ask() {
        let answered = ask_the_rules(role, &value)?;
        if validate_value(&answered, role).is_empty() {
            write_policy(&path, &answered)?;
            eprintln!("rules written to {}", path.display());
            return Ok(answered);
        }
    }
    bail!("{}", format_incomplete_error(&path, role, &problems));
}

pub(crate) fn load_buyer_runtime_policy(explicit: Option<&Path>) -> Result<BuyerRuntimePolicy> {
    buyer_runtime_policy_of(&validate_policy_file(explicit, RuntimeRole::Buyer)?)
}

/// The rules as the buyer runtime will hold them, including what it can actually execute.

/// Split from the read for the reason the seller's half was, and reached by `policy validate` the
/// same way: the buyer arm of that command used to run the shape check alone. Today every value the
/// shape check accepts has a runtime action behind it -- so this refuses nothing, and that is the
/// point: the moment an option is offered that the runtime cannot carry out, `policy validate` says
/// so instead of reporting "fine" and letting `dexdo buyer` refuse the same file later.

/// Which is also why the fallthroughs are refusals and not `unreachable!`: an option added to the
/// accepted list without an action here is a client defect, and a defect on the money path is a
/// refusal, never a panic.
pub(crate) fn buyer_runtime_policy_of(value: &Value) -> Result<BuyerRuntimePolicy> {
    let cannot = |key: &str, said: &str| {
        anyhow!(
            "{key} -> {said} (accepted by the file's shape, but this runtime has no action for it)"
        )
    };
    let choice = |key: &str| -> Result<&str> {
        get_path(value, key)
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("{key} is not one of the answers this file may carry"))
    };
    let int = |key: &str| -> Result<u64> {
        get_path(value, key)
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("{key} is not a whole number"))
    };
    let key = "buyer.on.no_handover_after_match";
    let no_handover_after_match = match choice(key)? {
        "wait_then_reclaim" => NoHandoverAfterMatchAction::WaitThenReclaim,
        "next_seller" => NoHandoverAfterMatchAction::NextSeller,
        "fail_closed" => NoHandoverAfterMatchAction::FailClosed,
        said => return Err(cannot(key, said)),
    };
    let key = "buyer.on.malformed_handover";
    let malformed_handover = match choice(key)? {
        "reclaim" => MalformedHandoverAction::Reclaim,
        "dispute" => MalformedHandoverAction::Dispute,
        "fail_closed" => MalformedHandoverAction::FailClosed,
        said => return Err(cannot(key, said)),
    };
    let key = "buyer.on.dead_gateway";
    let dead_gateway = match choice(key)? {
        "retry_then_reclaim" => DeadGatewayAction::RetryThenReclaim,
        "next_seller" => DeadGatewayAction::NextSeller,
        "fail_closed" => DeadGatewayAction::FailClosed,
        said => return Err(cannot(key, said)),
    };
    let key = "buyer.on.empty_stream";
    let empty_stream = match choice(key)? {
        "reclaim" => EmptyStreamAction::Reclaim,
        "next_seller" => EmptyStreamAction::NextSeller,
        "fail_closed" => EmptyStreamAction::FailClosed,
        said => return Err(cannot(key, said)),
    };
    let key = "buyer.on.seller_stalls_mid_stream";
    let seller_stalls_mid_stream = match choice(key)? {
        "accept_delivered_then_reclaim" => SellerStallsMidStreamAction::AcceptDeliveredThenReclaim,
        "dispute" => SellerStallsMidStreamAction::Dispute,
        said => return Err(cannot(key, said)),
    };
    let key = "buyer.on.bad_output_scam";
    let bad_output_scam = match choice(key)? {
        "stop" => BadOutputScamAction::Stop,
        "dispute" => BadOutputScamAction::Dispute,
        "stop_and_blacklist" => BadOutputScamAction::StopAndBlacklist,
        said => return Err(cannot(key, said)),
    };
    Ok(BuyerRuntimePolicy {
        no_handover_after_match,
        malformed_handover,
        dead_gateway,
        empty_stream,
        seller_stalls_mid_stream,
        bad_output_scam,
        max_sellers_to_try: int("buyer.failover.max_sellers_to_try")?,
        total_spend_cap_shells: int("buyer.failover.total_spend_cap_shells")?,
    })
}

pub(crate) fn load_seller_runtime_policy(explicit: Option<&Path>) -> Result<SellerRuntimePolicy> {
    seller_runtime_policy_of(&validate_policy_file(explicit, RuntimeRole::Seller)?)
}

/// The rules as the seller runtime will hold them, including what it can actually execute.

/// Split from the read so that `policy validate` runs the SAME check without the asking-and-writing
/// half. It reported "fine" on `after_deal_done=republish` for exactly as long as the two were one
/// function: shape validation accepts that value, and only this capability check refuses it -- the
/// runtime cannot republish onto a fresh TokenContract from the seller daemon.
pub(crate) fn seller_runtime_policy_of(value: &Value) -> Result<SellerRuntimePolicy> {
    let value = value.clone();
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
        "retire_gateway" => SellerBuyerNoShowAction::RetireGateway,
        _ => unreachable!("validated choice"),
    };
    let dispute_against_me = match choice("seller.on.dispute_against_me") {
        "release_if_clean" => SellerDisputeAgainstMeAction::ReleaseIfClean,
        "hold" => SellerDisputeAgainstMeAction::Hold,
        _ => unreachable!("validated choice"),
    };
    let policy = SellerRuntimePolicy {
        after_deal_done,
        buyer_no_show,
        dispute_against_me,
        max_open_deals: int("seller.max_open_deals"),
    };
    validate_seller_runtime_capabilities(&policy)?;
    Ok(policy)
}

pub(crate) fn load_seller_chain_unavailable_action(
    explicit: Option<&Path>,
) -> Result<dexdo::seller::gateway::ChainUnavailableAction> {
    let value = validate_policy_file(explicit, RuntimeRole::Seller)?;
    let Some(value) = get_path(&value, "seller.on.chain_unavailable").and_then(Value::as_str)
    else {
        return Ok(dexdo::seller::gateway::ChainUnavailableAction::default());
    };
    Ok(
        dexdo::seller::gateway::ChainUnavailableAction::from_config(value)
            .expect("validated optional chain-unavailable action"),
    )
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
        set_missing_path(
            value,
            "seller.on.chain_unavailable",
            Value::from(dexdo::seller::gateway::ChainUnavailableAction::default().as_str()),
        );
        refresh_seller_legend(value);
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
            let mut value = read_policy(&path)?;
            if get_path(&value, "seller").is_some()
                || get_path(&value, "_legend.allowed.seller").is_some()
            {
                refresh_seller_legend(&mut value);
            }
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
        PolicyCommand::Validate(args) => match args.role {
            // Read-only on purpose: this command reports, and a report that fills in what it found
            // missing can only ever say "fine".

            // Read-only is NOT the same as check-less, and conflating the two is how this command
            // started passing `after_deal_done=republish`: the shape check accepts that value and
            // only the runtime-capability check refuses it. So the same check the seller runs at
            // startup runs here too, over the value that was read rather than over one this command
            // repaired.
            PolicyValidateRoleArg::Buyer => {
                let value = inspect_policy_file(args.path.as_deref(), RuntimeRole::Buyer)?;
                buyer_runtime_policy_of(&value)?;
                Ok(())
            }
            PolicyValidateRoleArg::Seller => {
                let value = inspect_policy_file(args.path.as_deref(), RuntimeRole::Seller)?;
                seller_runtime_policy_of(&value)?;
                Ok(())
            }
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoctorPolicyStatus {
    Ready,
    Missing,
    Incomplete,
}

impl DoctorPolicyStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Missing => "missing",
            Self::Incomplete => "incomplete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DoctorPolicyAssessment {
    pub(crate) status: DoctorPolicyStatus,
    pub(crate) problems: Vec<String>,
}

pub(crate) fn doctor_policy_assessment(explicit: Option<&Path>) -> Result<DoctorPolicyAssessment> {
    let path = resolve_policy_path(explicit)?;
    if !path.exists() {
        return Ok(DoctorPolicyAssessment {
            status: DoctorPolicyStatus::Missing,
            problems: Vec::new(),
        });
    }
    let value = read_policy(&path)?;
    let mut problems = validate_value(&value, RuntimeRole::Buyer);
    problems.extend(validate_value(&value, RuntimeRole::Seller));
    problems.sort_by(|a, b| a.key.cmp(&b.key));
    problems.dedup_by(|a, b| a.key == b.key);
    if problems.is_empty() {
        Ok(DoctorPolicyAssessment {
            status: DoctorPolicyStatus::Ready,
            problems: Vec::new(),
        })
    } else {
        Ok(DoctorPolicyAssessment {
            status: DoctorPolicyStatus::Incomplete,
            problems: problems
                .iter()
                .map(|p| p.key.as_str())
                .map(str::to_string)
                .collect(),
        })
    }
}

pub(crate) fn doctor_policy_line(assessment: &DoctorPolicyAssessment) -> String {
    match assessment.status {
        DoctorPolicyStatus::Ready => "ready".to_string(),
        DoctorPolicyStatus::Missing => "not configured (optional for doctor)".to_string(),
        DoctorPolicyStatus::Incomplete => format!("incomplete: {}", assessment.problems.join(", ")),
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
        ("buyer.on.dead_gateway", "next_seller") => &["one_shot_policy_fail_closed"],
        ("buyer.on.dead_gateway", "fail_closed") => &["policy_fail_closed"],
        ("buyer.on.empty_stream", "reclaim") => &["seller_timeout"],
        ("buyer.on.empty_stream", "next_seller") => &["one_shot_policy_fail_closed"],
        ("buyer.on.empty_stream", "fail_closed") => &["policy_fail_closed"],
        ("buyer.on.seller_stalls_mid_stream", "accept_delivered_then_reclaim") => {
            &["accept_delivered", "seller_timeout"]
        }
        ("buyer.on.seller_stalls_mid_stream", "dispute") => &["stream_dispute"],
        ("buyer.on.bad_output_scam", "stop") => &["stream_stop"],
        ("buyer.on.bad_output_scam", "dispute") => &["stream_dispute"],
        ("buyer.on.bad_output_scam", "stop_and_blacklist") => &["policy_fail_closed_unsupported"],
        ("seller.on.after_deal_done", "republish") => &["pre_offer_policy_fail_closed"],
        ("seller.on.after_deal_done", "republish_with_backoff") => {
            &["pre_offer_policy_fail_closed"]
        }
        ("seller.on.after_deal_done", "retire") => &["retire_offer"],
        ("seller.on.buyer_no_show", "cleanup_and_republish") => &["pre_offer_policy_fail_closed"],
        ("seller.on.buyer_no_show", "cleanup_and_retire") => &["pre_offer_policy_fail_closed"],
        ("seller.on.buyer_no_show", "retire_gateway") => &["retire_gateway"],
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
                    "after_deal_done": "retire",
                    "buyer_no_show": "retire_gateway",
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
            SellerAfterDealDoneAction::Retire
        );
        assert_eq!(
            seller_policy.buyer_no_show,
            SellerBuyerNoShowAction::RetireGateway
        );
        assert_eq!(
            seller_policy.dispute_against_me,
            SellerDisputeAgainstMeAction::ReleaseIfClean
        );
        assert_eq!(seller_policy.max_open_deals, 1);
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
#[path = "policy_1196_tests.rs"]
mod issue_1196_tests;

/// A check must not repair what it checks: `policy validate` reports and writes nothing, even on a
/// terminal where a command on its way to spending would have asked.
#[test]
fn validate_reports_an_incomplete_file_without_touching_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("policy.json");
    let mut scaffold = serde_json::json!({});
    scaffold_roles(&mut scaffold, PolicyRoleArg::Seller);
    let bytes = serde_json::to_vec_pretty(&scaffold).expect("serialize");
    std::fs::write(&path, &bytes).expect("write the scaffold");

    assert!(inspect_policy_file(Some(&path), RuntimeRole::Seller).is_err());
    assert_eq!(
        std::fs::read(&path).expect("read back"),
        bytes,
        "the file must be byte-for-byte what it was"
    );
}

/// both roles can be checked, because both are refused at startup when their rules are
/// incomplete. `policy init` has always scaffolded either; only the check was seller-only, so a
/// buyer could write a policy and had no way to find out whether it would load.
#[test]
fn validate_accepts_both_roles_and_uses_each_role_own_loader() {
    use crate::cli::args::PolicyValidateRoleArg;
    use clap::ValueEnum as _;

    let roles: Vec<PolicyValidateRoleArg> = PolicyValidateRoleArg::value_variants().to_vec();
    assert!(roles.contains(&PolicyValidateRoleArg::Buyer), "{roles:?}");
    assert!(roles.contains(&PolicyValidateRoleArg::Seller), "{roles:?}");

    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("policy.json");
    // A file scaffolded for the seller alone must fail the BUYER check: same file, different role,
    // and the check has to be the role's own loader rather than a shared "looks like JSON".
    let mut scaffold = serde_json::json!({});
    scaffold_roles(&mut scaffold, PolicyRoleArg::Seller);
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&scaffold).expect("serialize"),
    )
    .expect("write the scaffold");
    assert!(
        load_seller_runtime_policy(Some(&path)).is_err(),
        "a scaffold is UNSET until filled in"
    );
    assert!(
        load_buyer_runtime_policy(Some(&path)).is_err(),
        "a seller file has no buyer rules"
    );
}

#[cfg(test)]
#[path = "policy_fixture_gate_tests.rs"]
mod policy_fixture_gate_tests;

#[cfg(test)]
#[path = "policy_1492_tests.rs"]
mod issue_1492_tests;
