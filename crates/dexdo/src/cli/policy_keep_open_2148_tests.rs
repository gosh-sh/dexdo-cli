//! `keep_open` is an answer to every `buyer.on.*` field and to nothing else.
use super::*;
use serde_json::json;

const BUYER_ON: [&str; 6] = [
    "buyer.on.no_handover_after_match",
    "buyer.on.malformed_handover",
    "buyer.on.dead_gateway",
    "buyer.on.empty_stream",
    "buyer.on.seller_stalls_mid_stream",
    "buyer.on.bad_output_scam",
];

fn all_keep_open() -> Value {
    json!({
        "version": 1,
        "buyer": {
            "on": {
                "no_handover_after_match": "keep_open",
                "malformed_handover": "keep_open",
                "dead_gateway": "keep_open",
                "empty_stream": "keep_open",
                "seller_stalls_mid_stream": "keep_open",
                "bad_output_scam": "keep_open"
            },
            "failover": {"max_sellers_to_try": 1, "total_spend_cap_shells": 1}
        }
    })
}

#[test]
fn every_buyer_on_field_accepts_keep_open_and_the_runtime_carries_it() {
    for path in BUYER_ON {
        assert!(accepts(path, "keep_open"), "{path}");
    }
    let policy = all_keep_open();
    assert!(validate_value(&policy, RuntimeRole::Buyer).is_empty());
    let runtime = buyer_runtime_policy_of(&policy).unwrap();
    assert_eq!(
        runtime.no_handover_after_match,
        NoHandoverAfterMatchAction::KeepOpen
    );
    assert_eq!(
        runtime.malformed_handover,
        MalformedHandoverAction::KeepOpen
    );
    assert_eq!(runtime.dead_gateway, DeadGatewayAction::KeepOpen);
    assert_eq!(runtime.empty_stream, EmptyStreamAction::KeepOpen);
    assert_eq!(
        runtime.seller_stalls_mid_stream,
        SellerStallsMidStreamAction::KeepOpen
    );
    assert_eq!(runtime.bad_output_scam, BadOutputScamAction::KeepOpen);
    for (field, said) in [
        (runtime.no_handover_after_match.as_str(), "no_handover"),
        (runtime.malformed_handover.as_str(), "malformed"),
        (runtime.dead_gateway.as_str(), "dead_gateway"),
        (runtime.empty_stream.as_str(), "empty_stream"),
        (runtime.seller_stalls_mid_stream.as_str(), "stalls"),
        (runtime.bad_output_scam.as_str(), "scam"),
    ] {
        assert_eq!(field, "keep_open", "{said}");
    }
    let api = runtime.as_api_failure_policy();
    assert_eq!(
        api.verification_bail,
        dexdo::buyer::api::VerificationBailAction::KeepOpen
    );
    assert_eq!(
        api.dead_gateway,
        dexdo::buyer::api::DeadGatewayAction::KeepOpen
    );
    assert_eq!(
        api.empty_stream,
        dexdo::buyer::api::EmptyStreamAction::KeepOpen
    );
    assert_eq!(
        api.seller_stalls_mid_stream,
        dexdo::buyer::api::SellerStallsMidStreamAction::KeepOpen
    );
}

#[test]
fn keep_open_is_rejected_by_every_field_outside_buyer_on() {
    for field in BUYER_FIELDS.iter().chain(SELLER_FIELDS.iter()) {
        if BUYER_ON.contains(&field.path) {
            continue;
        }
        assert!(!accepts(field.path, "keep_open"), "{}", field.path);
    }
    assert!(!accepts("seller.on.chain_unavailable", "keep_open"));

    let mut policy = all_keep_open();
    policy["seller"] = json!({
        "on": {
            "after_deal_done": "keep_open",
            "buyer_no_show": "keep_open",
            "dispute_against_me": "keep_open",
            "chain_unavailable": "keep_open"
        },
        "max_open_deals": 1
    });
    let keys = validate_value(&policy, RuntimeRole::Seller)
        .into_iter()
        .map(|problem| problem.key)
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        vec![
            "seller.on.after_deal_done",
            "seller.on.buyer_no_show",
            "seller.on.dispute_against_me",
            "seller.on.chain_unavailable",
        ]
    );
    // The buyer half of the same file stays valid.
    assert!(validate_value(&policy, RuntimeRole::Buyer).is_empty());
}

#[test]
fn scaffold_legend_states_keep_open_for_every_buyer_on_field() {
    let mut value = json!({});
    scaffold_roles(&mut value, PolicyRoleArg::Buyer);
    for path in BUYER_ON {
        let legend = get_path(&value, &format!("_legend.allowed.{path}"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{path} has no legend"));
        assert!(
            legend.split(" | ").any(|option| option == "keep_open"),
            "{path}: {legend}"
        );
    }
    let mut seller = json!({});
    scaffold_roles(&mut seller, PolicyRoleArg::Seller);
    assert!(!seller.to_string().contains("keep_open"), "{seller}");
}
