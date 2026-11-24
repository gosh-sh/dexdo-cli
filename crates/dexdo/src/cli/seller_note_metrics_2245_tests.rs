//! the notes whose balances the seller writes are the notes of its active deals, read from
//! the deal records, numbered by their place in the pool.
use super::*;
use crate::cli::deals::{DealHandle, DealHandleRole, DEAL_HANDLE_VERSION};

fn address(byte: &str) -> String {
    format!("0:{}", byte.repeat(32))
}

fn handle(token_contract: &str, note: &str, role: DealHandleRole) -> DealHandle {
    DealHandle {
        version: DEAL_HANDLE_VERSION,
        handle: deals::make_handle_id(token_contract, role),
        role,
        network: "fixture".into(),
        token_contract: token_contract.into(),
        note_addr: note.into(),
        frame_model: "qwen/qwen3-32b".into(),
        model_hash: Some(dexdo_core::model_hash_for("qwen/qwen3-32b")),
        order_book: None,
        root_model: None,
        market: None,
        contracts: "manifest/deployed.manifest.json".into(),
        endpoint: None,
        created_order_ids: vec![],
        created_at_unix: 1,
    }
}

fn pool(dir: &Path, notes: &[&str]) -> PathBuf {
    let path = dir.join("pn_pool.json");
    let body = serde_json::json!({
        "token_type": dexdo_core::params::SHELL_CURRENCY_ID,
        "notes": notes.iter().map(|note| serde_json::json!({"address": note})).collect::<Vec<_>>(),
    });
    std::fs::write(&path, body.to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

#[test]
fn only_the_notes_of_active_seller_deals_are_written_by_their_pool_number() {
    let dir = tempfile::tempdir().unwrap();
    let deals_dir = dir.path().join("deals");
    let (active_tc, finished_tc, bought_tc) = (address("a1"), address("a2"), address("a3"));
    let (traded, idle, other, missing) =
        (address("b1"), address("b2"), address("b3"), address("b4"));
    let missing_tc = address("a4");
    for record in [
        handle(&active_tc, &traded, DealHandleRole::Seller),
        handle(&finished_tc, &idle, DealHandleRole::Seller),
        handle(&bought_tc, &other, DealHandleRole::Buyer),
        handle(&missing_tc, &missing, DealHandleRole::Seller),
    ] {
        deals::save_deal_handle(&deals_dir, &record).unwrap();
    }
    let pool = pool(dir.path(), &[&idle, &other, &traded]);
    // The seller works on `active_tc`, on `missing_tc` (whose note is not in the pool) and on
    // `bought_tc`, which this process holds as a buyer.
    let active = [active_tc.clone(), missing_tc, bought_tc];
    assert_eq!(
        active_notes(&active, &deals_dir, Some(&pool)).unwrap(),
        [(2, deals::normalize_addr(&traded))]
    );
    // A deal is matched by its account whichever form its address is written in.
    let dapp_form = format!("4:{}", active_tc.rsplit(':').next().unwrap());
    assert_eq!(
        active_notes(&[dapp_form], &deals_dir, Some(&pool)).unwrap(),
        [(2, deals::normalize_addr(&traded))]
    );
    // No active deal: nothing is read, not even the pool.
    assert!(active_notes(&[], &deals_dir, None).unwrap().is_empty());
}
