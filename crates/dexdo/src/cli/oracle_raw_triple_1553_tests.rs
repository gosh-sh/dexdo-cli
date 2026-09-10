//! The run that creates a PMP stake deletes its own oracle-market manifest on the way out,
//! and `oracle cancel-stake` accepted nothing else. So the standard path produced a live stake that
//! a fully working client could not address: not "money lost", but a **recovered triple with
//! nowhere to type it**, which is what moves the repair into the client and makes it a flag rather
//! than an investigation.

//! The incident these numbers come from: note
//! `0:6544c564cb76c9220eb93305fa8322476c6bd5ea4be095e3da2a1d1bc5a396b1` held
//! **93.905000000 SHELL** behind a **0.020000000 SHELL** stake. Its triple was recovered from chain
//! and proven by recomputing the contract's own stake key -- without a single transaction -- so the
//! case below is reproducible offline forever.

use super::{resolve_pmp_exit_target, PmpExitTarget};
use crate::cli::args::{IdentityArgs, OraclePmpExitArgs};
use std::path::PathBuf;

/// The PMP that held the stake.
const PMP: &str = "0:030727d91c9efa89e35b0c419e74b7222e5666ee8d71414798e0b487b619ffdd";
const EVENT_ID: &str = "0x142a83b7261904e6652616197cc9e05a55d7838dd35c619cffe60ad810884409";
const ORACLE_LIST_HASH: &str = "0x3331caa42e2af57fd4ffb09a4f805bbcaf02c58b6f0cd76afe27bbadc171ea4e";
const TOKEN_TYPE: u32 = 2;
/// What the stake was holding, in raw ECC[2]. Named so the test says what it is about.
const LOCKED_RAW: u128 = 93_905_000_000;
const STAKE_RAW: u128 = 20_000_000;
const _: () = assert!(LOCKED_RAW / STAKE_RAW > 4_000);

fn args(
    manifest: Option<&str>,
    pmp: Option<&str>,
    event: Option<&str>,
    list: Option<&str>,
) -> OraclePmpExitArgs {
    OraclePmpExitArgs {
        identity: IdentityArgs {
            note_key: None,
            note_index: 0,
            note_addr: None,
        },
        manifest: manifest.map(PathBuf::from),
        pmp: pmp.map(str::to_string),
        event_id: event.map(str::to_string),
        oracle_list_hash: list.map(str::to_string),
        token_type: TOKEN_TYPE,
    }
}

fn raw() -> OraclePmpExitArgs {
    args(None, Some(PMP), Some(EVENT_ID), Some(ORACLE_LIST_HASH))
}

fn assert_incident(target: &PmpExitTarget) {
    assert_eq!(target.pmp, PMP, "the PMP the stake lives on");
    assert_eq!(target.event_id, EVENT_ID, "eventId of the stake");
    assert_eq!(
        target.oracle_list_hash, ORACLE_LIST_HASH,
        "oracleListHash of the stake"
    );
    assert_eq!(target.token_type, TOKEN_TYPE, "SHELL");
}

/// THE DEFECT ITSELF: with no manifest anywhere on disk, the exit is still addressable.
#[test]
fn issue_1553_the_incident_stake_is_addressable_without_any_manifest() {
    let target = resolve_pmp_exit_target(&raw(), "oracle cancel-stake")
        .expect("a recovered triple must be enough to address the stake");
    assert_incident(&target);
    assert!(
        target.source.contains("triple"),
        "the target must say the triple is where its identity came from, so a mismatch later \
         names the right side: {}",
        target.source
    );
}

/// A manifest that DOES survive keeps working, and both routes name the same thing. If they could
/// disagree, the raw route would be a second truth rather than the same one typed by hand.
#[test]
fn issue_1553_a_surviving_manifest_and_the_raw_triple_agree() {
    let dir = std::env::temp_dir().join(format!("dexdo-1553-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("oracle-market.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "network": "net-a",
            "root_oracle": "0:1515151515151515151515151515151515151515151515151515151515151515",
            "oracle": "0:43f9382bf7bb26608d9e3ad6f48582ac10734b64b24b46c2a62026d8e2039f33",
            "oracle_event_list": "0:b1a9ef39ad00cbc30e0509c486167298ad46e330123c1560a0ebba990f78a240",
            "oracle_list_hash": ORACLE_LIST_HASH,
            "event_id": EVENT_ID,
            "event_name": "weekly-noliquidity-1787160216",
            "pmp": PMP,
            "token_type": TOKEN_TYPE,
            "inference_order_book": "0:bcf1d27d959309886914262349c0857a44bd9f79b1600d3b08e176a97a4bcdd8",
            "frame_model": "qwen--qwen3--32b-oracle-noliquidity-1787160216",
            "deadline": 1_787_160_375u64,
            "bounds": ["1000000001"],
            "outcome_names": ["below-or-at-fill", "above-fill"],
        })
        .to_string(),
    )
    .expect("write manifest");

    let from_file = resolve_pmp_exit_target(
        &args(Some(path.to_str().expect("utf-8 path")), None, None, None),
        "oracle cancel-stake",
    )
    .expect("the manifest route must keep working");
    assert_incident(&from_file);
    assert!(
        from_file.source.contains("manifest"),
        "{}",
        from_file.source
    );

    let from_triple = resolve_pmp_exit_target(&raw(), "oracle cancel-stake").expect("raw route");
    assert_eq!(from_file.pmp, from_triple.pmp);
    assert_eq!(from_file.event_id, from_triple.event_id);
    assert_eq!(from_file.oracle_list_hash, from_triple.oracle_list_hash);
    assert_eq!(from_file.token_type, from_triple.token_type);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The controls. A route that accepted anything would look identical to a fix from the outside.
#[test]
fn issue_1553_the_two_routes_are_exclusive_and_neither_is_optional() {
    let neither = resolve_pmp_exit_target(&args(None, None, None, None), "oracle cancel-stake")
        .expect_err("addressing nothing must refuse");
    assert!(
        neither.to_string().contains("--manifest") && neither.to_string().contains("--pmp"),
        "the refusal must name both routes: {neither}"
    );

    let both = resolve_pmp_exit_target(
        &args(
            Some("/nonexistent/oracle-market.json"),
            Some(PMP),
            Some(EVENT_ID),
            Some(ORACLE_LIST_HASH),
        ),
        "oracle cancel-stake",
    )
    .expect_err("two sources that could disagree must refuse, not be silently ranked");
    assert!(both.to_string().contains("not both"), "{both}");

    // `--pmp` without the rest of the triple. clap's `requires_all` catches this at parse time;
    // the resolver refuses it too, because a struct built any other way must not slip past.
    let half = resolve_pmp_exit_target(
        &args(None, Some(PMP), None, Some(ORACLE_LIST_HASH)),
        "oracle cancel-stake",
    )
    .expect_err("a half triple must refuse");
    assert!(half.to_string().contains("--event-id"), "{half}");

    let mut wrong_currency = raw();
    wrong_currency.token_type = 0;
    let refused = resolve_pmp_exit_target(&wrong_currency, "oracle cancel-stake").expect_err(
        "a non-SHELL token type must refuse on the raw route as it does on the manifest",
    );
    assert!(refused.to_string().contains("SHELL"), "{refused}");
}
