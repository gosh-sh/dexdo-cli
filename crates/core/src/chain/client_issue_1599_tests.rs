//! our own admission ceiling, and the one reader that now raises a typed body error.

//! An earlier version of this header claimed a measured peak of SIX requests a second. That number
//! double-counted: it summed h2 stream-opens with HTTP/1.1 pool acquisitions, which are the same
//! requests seen twice. It is withdrawn, and no
//! corrected peak replaces it, because none was measured on that command. What stands from the run
//! is that mainnet answered it with `pool timed out`.

//! # What these tests do NOT claim

//! That the chain sees at most three requests a second. They pin OUR ADMISSIONS. How many HTTP
//! requests the SDK makes out of one admission is not ours and is not measured: one admission can
//! become more than one request. The divergence runs toward UNDERCOUNTING, so this is a floor on
//! restraint, not a proof of the chain-side rate.

use super::{ChainRequestCeiling, Deployed, GraphQlBodyError, RequestGate};

/// The smallest manifest `Deployed` accepts, so a test can state one field and mean it.
fn deployed_fixture() -> Deployed {
    serde_json::from_str(&format!(
        r#"{{
            "network": "net-a",
            "superroot": "0:{zeros}",
            "dapp_config": "0:{zeros}",
            "dapp_id": "{zeros}",
            "endpoint": "https://net-a.example"
        }}"#,
        zeros = "0".repeat(64),
    ))
    .expect("the fixture manifest parses")
}
use serde_json::json;

fn pool_timeout_errors() -> serde_json::Value {
    json!([{
        "message": "pool timed out while waiting for an open connection",
        "path": ["blockchain", "account", "messages"]
    }])
}

/// The ceiling is DATA the manifest carries, so a test states a value instead of naming a chain.

/// This asserted a `match` on the network label -- the production chain's name arm yielded 3 and
/// every other arm none. The
/// figure and its reason are unchanged; where it comes from is not, because a ceiling is a property
/// of the chain being dialled and the manifest is the document that describes that chain.
/// The committed manifests are checked below, so the production figure is still pinned -- by the
/// file that will still be right when a chain is added, which a label match would not be.
#[test]
fn the_ceiling_is_what_the_manifest_declares_and_nothing_otherwise() {
    let with = |per_second: Option<u32>| {
        let mut deployed = deployed_fixture();
        deployed.requests_per_second = per_second;
        ChainRequestCeiling::from_manifest(&deployed)
    };

    assert_eq!(with(Some(3)), ChainRequestCeiling::PerSecond(3));
    // A manifest naming no ceiling gets none. Not silently ceilinged: a campaign chain must not be
    // throttled into hours because the client assumed a figure nobody wrote down for it.
    assert_eq!(with(None), ChainRequestCeiling::Unlimited);
    // Zero is not a ceiling of zero requests -- that would be a client that cannot talk at all.
    assert_eq!(with(Some(0)), ChainRequestCeiling::Unlimited);
}

/// The owner's figure is pinned, now where it actually lives.

/// **This test is the reason the port is safe, so it has to actually pin the number.** The first
/// version of it asserted only that a declared ceiling was greater than zero -- which let the
/// production ceiling be DELETED from the manifest with the whole suite still green, silently
/// dropping that chain to unlimited. On `dev` the figure was a `const` and the compiler held it;
/// moving it into data moved that job here, and a check that does not do it is worse than the
/// const it replaced.

/// What is asserted: at least one committed manifest declares a ceiling, and every ceiling declared
/// anywhere is exactly the owner's figure. No manifest is named -- a second chain earning a ceiling
/// of its own is an owner decision, and this going red is how that decision gets made deliberately
/// rather than by someone editing a file.
#[test]
fn the_owners_ceiling_is_the_figure_the_committed_manifests_carry() {
    /// The owner's figure: a burst is answered with `pool timed out while waiting for an
    /// open connection` at HTTP 200, and a retry on top of a self-inflicted overload makes the
    /// overload worse.
    const OWNERS_CEILING: u32 = 3;

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../manifest");
    let mut declared = Vec::new();

    for entry in std::fs::read_dir(&dir).expect("read the committed manifest directory") {
        let path = entry.expect("read a manifest directory entry").path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.ends_with(".manifest.json") {
            continue;
        }
        let deployed = Deployed::load(&path).unwrap_or_else(|error| panic!("load {name}: {error}"));
        if let Some(per_second) = deployed.requests_per_second {
            declared.push((name.to_string(), per_second));
        }
    }

    /// How many committed manifests carry a ceiling.

    /// Pinned, not merely "at least one". With one carrier the two are the same check, and that is
    /// exactly why the weaker form is a trap: give a second manifest a ceiling and deleting the
    /// first one's would leave this green while that chain silently drops to unlimited. Counting
    /// makes both directions loud -- a ceiling lost, and a ceiling gained without anyone deciding.
    const CARRIERS: usize = 1;

    assert_eq!(
        declared.len(),
        CARRIERS,
        "{} committed manifest(s) declare `requests_per_second`, expected {CARRIERS}: {declared:?}. \
         A ceiling lost means a chain now runs unmetered; a ceiling gained is an owner's decision \
         about a chain's tolerance, not an edit. Either way, say so here.",
        declared.len()
    );
    for (name, per_second) in &declared {
        assert_eq!(
            *per_second, OWNERS_CEILING,
            "{name} declares a ceiling of {per_second}. The owner's figure is {OWNERS_CEILING}; a \
             different one is a decision, not an edit."
        );
    }
}

/// The gate itself: a fourth admission inside one second must wait for the window to roll.

/// This is the assertion the negative control below breaks on purpose.
#[tokio::test]
async fn a_fourth_admission_in_one_second_waits_for_the_window() {
    let gate = RequestGate::new(ChainRequestCeiling::PerSecond(3));
    let started = std::time::Instant::now();

    for _ in 0..3 {
        gate.admit().await;
    }
    let three = started.elapsed();
    assert!(
        three < std::time::Duration::from_millis(200),
        "three admissions fit the window and must not wait; took {three:?}"
    );

    gate.admit().await;
    let four = started.elapsed();
    assert!(
        four >= std::time::Duration::from_millis(900),
        "the fourth must wait for the first to leave the one-second window, waited {four:?}"
    );
    // An upper bound as well as a lower one. A one-sided assertion is satisfied by waiting FOREVER,
    // so a gate that slept a whole minute per request -- an outage on the money path dressed as
    // politeness -- would still be green. The wait is bounded by the window, so anything past it by
    // more than scheduling slack is a different bug, not a stricter version of this one.
    assert!(
        four < std::time::Duration::from_millis(2_000),
        "the fourth waits for the window to roll, not longer; waited {four:?}"
    );
}

/// `Unlimited` means no ceiling, not "unset". A network that carries none must never wait.
#[tokio::test]
async fn an_unlimited_network_never_waits() {
    let gate = RequestGate::new(ChainRequestCeiling::Unlimited);
    let started = std::time::Instant::now();
    for _ in 0..50 {
        gate.admit().await;
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(200),
        "a campaign network must not be throttled: 50 admissions took {elapsed:?}"
    );
}

/// The ext-out page reader now raises a typed body error, so the READ predicate recognises a pool
/// timeout where it previously saw an untyped string. One reader, not two: the TokenContract
/// inbound-call site was reverted to its untyped form as out of this batch's scope.
#[test]
fn a_typed_pool_timeout_is_transient_for_a_read() {
    let error = anyhow::Error::new(GraphQlBodyError::from_errors(&pool_timeout_errors()))
        .context("account abcd ext-out GraphQL errors");
    assert!(super::is_transient_read_failure(&error));
}

/// The boundary this whole split exists to hold: the predicate that gates the MONEY SUBMIT retry
/// must not have moved. Extended from PR1598 to cover the one site typed here.

/// The context is the one production actually builds. A fabricated context for a site that is NOT
/// typed would assert about a case that cannot arise, which reads as coverage and is not.
#[test]
fn the_submit_predicate_is_not_widened() {
    let context = "account abcd ext-out GraphQL errors";
    let error =
        anyhow::Error::new(GraphQlBodyError::from_errors(&pool_timeout_errors())).context(context);
    assert!(
        !super::is_transient_transport_failure(&error),
        "is_transient_transport_failure feeds is_transient_submit_failure; widening it would \
         retry a SUBMIT, which is a different question from retrying a read: {context}"
    );
}

// ---------------------------------------------------------------------------------------------
// review finding 4: WIRING.

// The tests above pin the gate. They do not pin that any production reader USES it -- and the pager
// tests in client.rs pass an `Unlimited` gate, whose behaviour is identical whether the `admit()` is
// present or deleted. So the whole ceiling could be quietly unwired and every test stayed green.

// These tests assert the wiring itself: the production reader is run against a local stub, and the
// gate is asked how many admissions it granted. Delete an `admit()` from the production path and the
// count drops, so one of these fails. That is the property, stated as a test rather than as trust.
// ---------------------------------------------------------------------------------------------

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One page of ext-out messages, and the dapp-id lookup, from a local socket.

/// The SDK's dapp-id lookup is a GET on `/v2/account`; the pager is a POST. The stub answers both by
/// dispatching on the request line, so a reader that makes either call is served without the test
/// having to know the order.
async fn serve_stub(listener: tokio::net::TcpListener, connections: usize) {
    for _ in 0..connections {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = Vec::new();
        let head = loop {
            let mut chunk = [0_u8; 4096];
            let Ok(read) = socket.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            request.extend_from_slice(&chunk[..read]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                break String::from_utf8_lossy(&request[..end]).to_string();
            }
        };
        let body = if head.starts_with("GET") {
            json!({ "dapp_id": "2".repeat(64) }).to_string()
        } else {
            json!({"data": {"blockchain": {"account": {
                "info": {"id": "1".repeat(64)},
                "messages": {
                    "pageInfo": {"startCursor": null, "hasPreviousPage": false},
                    "edges": []
                }
            }}}})
            .to_string()
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = socket.write_all(response.as_bytes()).await;
    }
}

async fn stub_endpoint(connections: usize) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind stub");
    let endpoint = format!("http://{}", listener.local_addr().expect("stub address"));
    (endpoint, tokio::spawn(serve_stub(listener, connections)))
}

/// The page fetch itself takes a slot. Delete the `admit()` in `fetch_ext_out_page` and this fails.
#[tokio::test]
async fn the_ext_out_pager_admits_before_it_fetches_a_page() {
    let (endpoint, stub) = stub_endpoint(1).await;
    let gate = RequestGate::new(ChainRequestCeiling::Unlimited);

    super::fetch_ext_out_page(
        &gate,
        &reqwest::Client::new(),
        &endpoint,
        &format!("0:{}", "1".repeat(64)),
        &format!("0:{}", "2".repeat(64)),
        100,
        None,
    )
    .await
    .expect("stub page");

    assert_eq!(
        gate.admissions(),
        1,
        "one page fetch is one admission; 0 means the production admit is gone"
    );
    stub.abort();
}

/// The dapp-id lookup is a request of its own, so it takes a slot of its own. Delete either
/// `admit()` on this path and the count is no longer two.
#[tokio::test]
async fn the_ext_out_reader_admits_for_the_dapp_lookup_and_for_the_page() {
    let (endpoint, stub) = stub_endpoint(2).await;
    let gate = RequestGate::new(ChainRequestCeiling::Unlimited);

    let messages: Vec<()> = super::fetch_all_ext_out_messages(
        &gate,
        &reqwest::Client::new(),
        &endpoint,
        &format!("0:{}", "1".repeat(64)),
        |_| Ok(None),
    )
    .await
    .expect("stub read");

    assert!(messages.is_empty());
    assert_eq!(
        gate.admissions(),
        2,
        "the dapp-id lookup and the one page are two requests, so two admissions"
    );
    stub.abort();
}

/// The book-event fold has its OWN dapp-id lookup, which went out ungated until's review. It
/// is a separate call site from the one above, so it needs its own assertion: neither ratchet
/// counter can see it, and the reader returns the same fold either way.
#[tokio::test]
async fn the_book_event_fold_admits_for_its_own_dapp_lookup() {
    let (endpoint, stub) = stub_endpoint(2).await;
    let gate = RequestGate::new(ChainRequestCeiling::Unlimited);

    crate::chain::book_events::read_book_event_fold(
        &gate,
        &reqwest::Client::new(),
        &endpoint,
        &format!("0:{}", "1".repeat(64)),
        crate::chain::book_events::BookEventFold::default(),
    )
    .await
    .expect("stub fold");

    assert_eq!(
        gate.admissions(),
        2,
        "the fold's dapp-id lookup and its one page are two requests, so two admissions"
    );
    stub.abort();
}
