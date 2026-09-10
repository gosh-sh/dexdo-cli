//! (3 of 3): the dashboard HTML renders a TokenContract in its self-DApp form.

//! `render_html` puts the TokenContract through `dexdo_core::address::display_self_dapp`. A per-deal
//! TokenContract is a SELF-DApp account -- its DApp id is its own account id -- so a real one renders
//! `<account>::<account>` and the `0:` spelling never reaches the page. The existing test asserts
//! `html.contains("0:feedface")`, a form production cannot emit, and it is satisfied whether the
//! renderer canonicalises or not.

//! Measured under: with `display_self_dapp` removed from that cell,
//! `cargo test --workspace --locked` stayed at 1902 passed / 0 failed.

//! The HTML is split into whitespace-delimited tokens with tag punctuation trimmed, so the check is
//! on a whole cell value. A substring search for the account id would match both spellings at once,
//! which is precisely the distinction under test.

use super::{render_html, DashboardAccounting, DashboardDeal, DashboardSnapshot, DashboardSource};

const TC: &str = "b7e4a91c0d5f6382a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90a1b";

fn legacy() -> String {
    format!("0:{TC}")
}

/// A per-deal TokenContract is self-DApp: `display_self_dapp` reconstructs `<account>::<account>`.
fn self_dapp() -> String {
    format!("{TC}::{TC}")
}

/// Tokens of the rendered page: split on whitespace, then strip the tag characters that abut a cell
/// value. `<td>x</td>` is one token in the source and must be read as the value `x`.
fn tokens(html: &str) -> Vec<String> {
    html.split_whitespace()
        .flat_map(|chunk| chunk.split(['<', '>']))
        .map(|token| token.trim_matches(['/', '"', '\'']).to_string())
        .filter(|token| !token.is_empty())
        .collect()
}

fn deal(token_contract: &str) -> DashboardDeal {
    DashboardDeal {
        handle: "buyer-tc-open".into(),
        role: "buyer".into(),
        network: "net-a".into(),
        token_contract: token_contract.to_string(),
        frame_model: Some("qwen/qwen3-32b".into()),
        model_hash: None,
        state: "opened".into(),
        funded: Some(true),
        opened: Some(true),
        disputed: Some(false),
        terminal: Some(false),
        gateway_endpoint: Some("127.0.0.1:8443".into()),
        actor_note: None,
        counterparty_note: None,
        accounting: DashboardAccounting::default(),
    }
}

fn page(token_contract: &str) -> String {
    render_html(&DashboardSnapshot {
        version: 1,
        generated_at_unix: 1_754_006_400,
        source: DashboardSource {
            kind: "handles".into(),
            json_endpoint: "/api/dashboard.json".into(),
            handle_count: 1,
        },
        buyer: vec![deal(token_contract)],
        seller: vec![],
    })
}

/// The premise: this fixture is an address `display_self_dapp` would actually rewrite.
#[test]
fn the_fixture_is_a_token_contract_the_renderer_rewrites() {
    assert_eq!(TC.len(), 64);
    assert_eq!(
        dexdo_core::address::display_self_dapp(&legacy()),
        self_dapp(),
        "the renderer must change this fixture, or the test proves nothing"
    );
    assert_ne!(legacy(), self_dapp());
}

/// The page carries the self-DApp form, and not the spelling it was handed.
#[test]
fn the_row_names_the_token_contract_in_its_self_dapp_form() {
    let html = page(&legacy());
    let tokens = tokens(&html);

    assert!(
        tokens.iter().any(|token| token == &self_dapp()),
        "the row does not carry the self-DApp TokenContract as a cell value: {html}"
    );
    // Negative control: the legacy spelling handed in must not survive into the page. This is the
    // assertion the existing test makes in reverse, on a fixture that cannot tell the two apart.
    assert!(
        !tokens.iter().any(|token| token == &legacy()),
        "the row printed the legacy spelling the renderer is supposed to rewrite: {html}"
    );
    // The row is still a row, so this cannot pass by rendering an empty page.
    assert!(
        tokens.iter().any(|token| token == "buyer-tc-open"),
        "{html}"
    );
}

/// The DApp half is the ACCOUNT's own, not the shared dexdo DApp. A renderer that used
/// `display` instead of `display_self_dapp` would put `DEXDO_DAPP_ID` there and still look canonical.
#[test]
fn the_dapp_half_is_the_accounts_own_and_not_the_shared_one() {
    let html = page(&legacy());
    let tokens = tokens(&html);
    let shared = format!("{}::{TC}", dexdo_core::DEXDO_DAPP_ID);
    assert!(
        !tokens.iter().any(|token| token == &shared),
        "a per-deal TokenContract was rendered as a shared-DApp account: {html}"
    );
}
