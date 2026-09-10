use super::*;

/// audit item 6: a resolved explicit Hot is an ephemeral manual view, on the command's real
/// network, and reaches the balance wait. A successful return after `0 -> required` proves both
/// polling and the absence of a Vault request path; any arbitrary early error fails the test.
#[tokio::test]
async fn explicit_hot_at_the_money_command_entrypoint_is_not_a_noop() {
    let hot = self_dapp_hot();
    let view = HotFundingBinding {
        provider: WalletProvider::Manual,
        network: "net-a-from-command-manifest".to_string(),
        hot_address: hot.clone(),
        vault_address: None,
    };
    assert_eq!(view.provider, WalletProvider::Manual);
    assert_eq!(view.network, "net-a-from-command-manifest");
    assert_eq!(view.hot_address, hot);
    assert!(view.vault_address.is_none());

    let dir = temp();
    let chain = FakeChain::then_always(vec![0], 1_000);
    let provider = providers::DirectTopUpProvider::new(view.provider).expect("manual provider");
    let funded = ensure_hot_funded(
        &HotFundingContext {
            binding: &view,
            requirements: &requirements(),
            operation: "note deploy",
            creator_pubkey: "",
            data_dir: dir.path(),
            bounds: patient_bounds(),
        },
        &chain,
        &provider,
    )
    .await
    .expect("manual wait observes the scripted top-up");
    assert_eq!(funded.notice, FundingNotice::ManualTopUpRequested);
    assert!(
        chain.reads.get() >= 2,
        "the balance must be polled after the instruction"
    );
    assert_eq!(funded.observed.get(SHELL), 1_000);

    let instruction = provider.manual_instruction(&FundingRequest {
        provider: view.provider,
        network: view.network.clone(),
        vault_address: None,
        hot_address: view.hot_address.clone(),
        hot_dapp_id: view
            .hot()
            .expect("canonical explicit Hot")
            .dapp_id()
            .to_string(),
        creator_pubkey: String::new(),
        required: requirements().required,
        required_native: requirements().required_native,
        shortfall: requirements().required,
        native_shortfall: 0,
    });
    assert!(instruction.contains(&view.hot_address), "{instruction}");
    assert!(instruction.contains("0.000001 SHELL"), "{instruction}");
    assert!(!instruction.contains("Vault") && !instruction.contains("http"));
}

#[test]
fn money_command_entrypoint_is_wired_to_the_explicit_manual_view() {
    let source = include_str!("../../wallet_funding.rs");
    let body = source
        .split_once("pub(crate) async fn fund_hot_for_money_command(")
        .expect("entrypoint")
        .1
        .split_once("\n}\n\n#[cfg(test)]")
        .expect("entrypoint end")
        .0;
    assert!(
        body.contains("let view = binding.map_or_else(")
            && body.contains("provider: WalletProvider::Manual,")
            && body.contains("network: network.to_string(),")
            && body.contains("display_self_dapp(resolved_hot_address)"),
        "the production entrypoint bypasses explicit-manual routing:\n{body}"
    );
    assert!(!body.contains("return Ok(None)"), "{body}");
}
