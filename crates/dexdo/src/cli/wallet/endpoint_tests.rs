//! Every wallet command reads through a URL, not through whatever string arrived.

//! `wallet_read_endpoint` is the one place all three of them resolve it: `wallet onboard manual`
//! and `wallet onboard gosh-ai` prove a Hot through it, `wallet remove-archived` proves one empty.

//! If the endpoint reaches the HTTP client without a scheme, every read through it fails for a
//! transport reason. Removal then refuses with "read every balance... nothing was removed" -- the
//! same words an operator sees when the Hot really does hold funds, so an empty Hot becomes as
//! unremovable as a funded one; onboarding spends its whole activation timeout on "the chain read
//! failed; retrying", measured at 600 s on the acceptance stand.

//! Measured on the chain before the fix, with the acceptance suite's own endpoint form -- at the
//! time the endpoint was typed as `--endpoint`, a flag removed. The value now arrives from a
//! manifest, and a hand-written manifest can carry a bare host exactly as an operator once typed
//! one, so the normalisation these tests pin is the same normalisation:

//! ```text
//! Error: read every balance of archived binding ee20df7c... Hot 3675cd98...: read Hot... balances:
//! POST net-a.example/graphql; nothing was removed

//! Error: archived binding ee20df7c... Hot 3675cd98... still holds native=338236924000 and
//! ECC[2]=4100000000000; refusing permanent removal
//! ```

use super::*;

/// A manifest naming `endpoint`, written where only this test can see it.
fn manifest_with(endpoint: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    let dir = std::env::temp_dir().join(format!(
        "dexdo-wallet-endpoint-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create the manifest directory");
    let path = dir.join("deployed.test.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "network": "net-a",
            "version": "endpoint-normalisation-fixture",
            "superroot": "0:0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c",
            "dapp_config": "",
            "dapp_id": "0000000000000000000000000000000000000000000000000000000000000004",
            "endpoint": endpoint,
        })
        .to_string(),
    )
    .expect("write the manifest");
    path
}

#[test]
fn a_bare_host_in_the_manifest_becomes_a_url() {
    let manifest = manifest_with("net-a.example");
    let resolved = wallet_read_endpoint(Some(&manifest), crate::cli::wallet::test_network_a())
        .expect("a bare host is a valid endpoint");
    assert_eq!(resolved, "https://net-a.example");
}

#[test]
fn a_url_in_the_manifest_is_kept_as_given() {
    let manifest = manifest_with("https://net-a.example");
    let resolved = wallet_read_endpoint(Some(&manifest), crate::cli::wallet::test_network_a())
        .expect("an explicit URL is a valid endpoint");
    assert_eq!(resolved, "https://net-a.example");
}

// What stood here described the opposite contract: "an unreadable manifest is not a refusal here,
// the network's own default answers instead". That default was removed by and the sentence
// outlived it, attached to no test -- so an auditor reading this directory found two files stating
// contradictory contracts for one function. An unreadable manifest is a refusal, named as one, and
// `onboard_endpoint_source_1839_tests.rs` holds it.

/// An empty `endpoint` in the manifest is refused, and the refusal names the network it was for.
#[test]
fn an_empty_manifest_endpoint_is_refused() {
    let manifest = manifest_with("   ");
    let error = wallet_read_endpoint(Some(&manifest), crate::cli::wallet::test_network_a())
        .expect_err("an empty endpoint cannot be read from");
    assert!(
        error.to_string().contains("net-a"),
        "the refusal names the network it was resolving for: {error}"
    );
}
