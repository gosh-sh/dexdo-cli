//! The payment QR says which chain it is asking for, and says it in a form the wallet reads.

//! was caught with the money already on the screen: the owner nearly sent a mainnet transfer
//! against a shellnet request and stopped it by eye. Nothing in the flow could have stopped it.
//! The canonical address is IDENTICAL on both chains -- our own comment in the module beside this
//! one says so, and says the post-deploy address guard therefore cannot catch it either -- so the
//! link named a destination that was equally valid on the wrong chain, the wallet substituted
//! whichever network its own switch was set to, and neither side saw a disagreement.

//! The field to say it with did not exist then. It does now: `ackinacki-wallet` shipped `network`
//! and `flag` on `origin/rc/2` (`src/shared/qr/payment_uri.ts`, and the protocol document's
//! "Network binding" and "Message flag" sections). The wallet does not merely record the network --
//! it REFUSES a request whose network differs from the one selected, naming both. That refusal is
//! the whole value; our half is to state the network so there is something to refuse against.

//! **The label is passed through, not filtered, and that was decided against a first draft.**
//! `network` is not an ignored-if-unknown field: `parsePaymentNetwork` accepts exactly two values
//! and throws `invalid_network` on anything else, which rejects the WHOLE payment request. Since
//! the label is whatever manifest `DEXDO_MANIFEST` points at declares, so the first version
//! filtered it against those two values. That was wrong twice. It put network names back into a
//! client constant, which the manifest directive forbids and exists to hold. And it was the
//! less safe of the two: on an unknown label the field vanished silently, leaving a code that names
//! no chain -- which is this issue, restored quietly. Passed through, the wallet either honours the
//! label or refuses and says which. A refusal costs a scan; a silent omission costs a transfer onto
//! the wrong chain. The human-readable request names the network either way, on every manifest.

//! What the label may NOT do is stop being one value. It is appended into a `&`-delimited link, and
//! it comes from a downloaded file, so it is encoded -- see `a_label_cannot_smuggle_a_second_field`
//! below for what a label spelled `shellnet&flag=16` did before that.

use super::PaymentFlag;

/// The address shape every link in this file uses: canonical `dapp::account`, as
/// requires for anything an operator reads, and as the wallet's `isExtendedAddress` demands before
/// it will accept a `flag` at all.
fn extended_address() -> String {
    format!("{0}::{0}", "ef6ecd30".repeat(8))
}

/// A funding request for a manual Hot on `network`, short `ecc_shortfall` raw ECC[2] SHELL.
fn top_up_request(
    network: &str,
    ecc_shortfall: u128,
) -> crate::cli::wallet_funding::FundingRequest {
    crate::cli::wallet_funding::FundingRequest {
        provider: crate::cli::wallet::WalletProvider::Manual,
        network: network.to_string(),
        vault_address: None,
        hot_address: extended_address(),
        hot_dapp_id: "ef6ecd30".repeat(8),
        creator_pubkey: "pubkey".to_string(),
        required: [(dexdo_core::params::SHELL_CURRENCY_ID, ecc_shortfall)]
            .into_iter()
            .collect(),
        required_native: 0,
        shortfall: [(dexdo_core::params::SHELL_CURRENCY_ID, ecc_shortfall)]
            .into_iter()
            .collect(),
        native_shortfall: 0,
    }
}

/// The manifest's label reaches the link as the manifest wrote it, whatever it says.

/// An earlier version of this change filtered the label against the two values the wallet's parser

/// network name in a client constant in as many words, and exists to hold exactly that. And
/// the filter was the LESS safe behaviour: on an unknown label it silently dropped the binding, so
/// the operator was handed a code naming no chain -- which is itself, restored quietly. Sent
/// through, the label either satisfies the wallet or makes it refuse the request and say so.

/// Only whitespace is normalised, and only because the wallet trims before comparing too.
#[test]
fn the_label_the_manifest_declared_is_the_label_the_link_carries() {
    let address = extended_address();

    for label in ["mainnet", "shellnet", "net-a", "chain"] {
        let link = super::payment_link(&address, 2, label, PaymentFlag::None);
        assert!(
            link.contains(&format!("&network={label}")),
            "the client does not get an opinion about which chains exist: {link}"
        );
    }

    let trimmed = super::payment_link(&address, 2, "  shellnet\n", PaymentFlag::None);
    assert!(
        trimmed.contains("&network=shellnet") && !trimmed.contains("network= "),
        "a hand-edited manifest field carries whitespace the wallet would trim anyway: {trimmed}"
    );

    // Nothing to name means nothing is named -- a `network=` with an empty value is a claim about
    // a chain called nothing, and the wallet would reject the request for it.
    for absent in ["", "   "] {
        let link = super::payment_link(&address, 2, absent, PaymentFlag::None);
        assert!(
            !link.contains("network="),
            "an empty label is not a network: {link}"
        );
    }
}

/// A link for a known network states it, and states it without disturbing the v1 fields.

/// The order matters more than it looks. The compact form has no `to=` key: the wallet takes
/// everything before the FIRST `&` as the recipient. An appended field is safe; a field inserted
/// ahead of the address would silently redefine the destination.
#[test]
fn the_link_states_a_known_network_after_the_fields_that_were_always_there() {
    let address = extended_address();

    for network in ["mainnet", "shellnet"] {
        let link = super::payment_link(&address, 2, network, PaymentFlag::None);

        assert!(
            link.starts_with(&format!("{address}&")),
            "the recipient is everything before the first `&`, so it stays first: {link}"
        );
        assert!(
            link.contains("&amount=2") && link.contains("&token=2"),
            "the v1 fields a wallet that knows nothing of  reads must survive: {link}"
        );
        assert!(
            link.contains(&format!("&network={network}")),
            "the link must name the chain it is asking for: {link}"
        );
    }

    // The two links differ, which is the entire point: before this, one string served both chains.
    assert_ne!(
        super::payment_link(&address, 2, "mainnet", PaymentFlag::None),
        super::payment_link(&address, 2, "shellnet", PaymentFlag::None),
        "a request for mainnet and a request for shellnet must not be the same string -- being \
         the same string is what  is"
    );
}

/// A label cannot smuggle a second field into the link, and specifically cannot smuggle the flag.

/// Found by review on the first version of this change. The label was appended raw, and the only
/// check upstream of it -- `WalletNetwork::from_manifest_label` -- asks whether it is a single
/// plain path component. `&` and `=` are legal filename bytes, so `shellnet&flag=16` passes that
/// check, and a top-up link built from it reads to the wallet as a request carrying flag 16: all
/// three preconditions hold, because the recipient is an extended address and the token is 2. The
/// operator's SHELL then lands on an ACTIVE wallet as native vmshell, which cannot be spent as
/// ECC[2] currency or converted back -- the exact outcome the private `PaymentFlag` exists to make
/// unreachable, reached around it through a file the operator DOWNLOADS rather than types.

/// The cure is the protocol's own rule rather than a list of allowed labels: values are
/// `application/x-www-form-urlencoded`, so the label is encoded. A strange label then arrives at
/// the wallet as one strange VALUE and is refused as `invalid_network` -- loudly, and as itself.
#[test]
fn a_label_cannot_smuggle_a_second_field_into_the_link() {
    let address = extended_address();

    for smuggled in [
        "shellnet&flag=16",
        "shellnet&mode=dex",
        "shellnet&token=1",
        "shellnet=x",
    ] {
        let link = super::payment_link(&address, 100, smuggled, PaymentFlag::None);
        let fields: Vec<&str> = link.split('&').skip(1).collect();

        assert_eq!(fields.len(), 3, "a label added fields of its own: {link}");
        assert!(
            !link.contains("flag="),
            "a top-up link must never carry a flag, however the label was spelled: {link}"
        );
        assert!(
            fields
                .last()
                .is_some_and(|last| last.starts_with("network=")),
            "the label must arrive as one value of the network field: {link}"
        );
    }

    // And the ordinary labels are untouched by the encoding -- an encoder that escaped these would
    // send the wallet a value it does not recognise and break every payment.
    for ordinary in ["mainnet", "shellnet", "net-a", "chain"] {
        assert!(
            super::payment_link(&address, 2, ordinary, PaymentFlag::None)
                .ends_with(&format!("&network={ordinary}")),
            "an ordinary label must survive unchanged: {ordinary}"
        );
    }
}

/// With nothing to name, the link is byte-identical to the one this change replaced.

/// Not "close to": byte-identical. It is the proof that appending fields did not disturb the v1
/// form a wallet that never heard of still reads.
#[test]
fn with_no_network_to_name_the_link_is_the_one_that_came_before() {
    let address = extended_address();
    let bare = super::payment_link(&address, 7, "", PaymentFlag::None);

    assert_eq!(
        bare,
        format!(
            "{address}&amount=7&token={}",
            dexdo_core::params::SHELL_CURRENCY_ID
        ),
        "with no network and no flag, the link is the v1 link and nothing else: {bare}"
    );
}

/// The wiring the issue is about: the top-up code takes its chain from the request.

/// Review measured that nothing reached it -- replacing the network at the call site with an empty
/// string left every other test green, because they all handed the label in by hand. The decision
/// is a named function now, so this drives the same value the product does.
#[test]
fn the_top_up_code_takes_its_chain_from_the_request_it_was_built_for() {
    use crate::cli::wallet::WalletProvider;
    use crate::cli::wallet_funding::top_up_payment_code;

    for network in ["shellnet", "mainnet", "net-a"] {
        let request = top_up_request(network, 100 * dexdo_core::params::SHELL_UNIT);
        let code = top_up_payment_code(WalletProvider::Manual, &request)
            .expect("a manual provider short of ECC[2] SHELL prints a code");

        assert_eq!(
            code.network, network,
            "the code must ask on the chain the request was built for"
        );
        assert_eq!(code.address, request.hot_address);
        assert_eq!(
            code.whole_shell, 100,
            "and for the shortfall that was measured"
        );
    }

    // A provider that tops up elsewhere prints nothing here, and neither does a request with no
    // ECC[2] shortfall -- both silences are correct, and pinning them keeps the guard from being
    // satisfied by a function that always answers `Some`.
    assert!(
        top_up_payment_code(
            WalletProvider::GoshAi,
            &top_up_request("shellnet", 100 * dexdo_core::params::SHELL_UNIT)
        )
        .is_none(),
        "Gosh.ai is topped up on a web page, not by a transfer to this address"
    );
    assert!(
        top_up_payment_code(WalletProvider::Manual, &top_up_request("shellnet", 0)).is_none(),
        "no ECC[2] shortfall is nothing to ask for"
    );
}

/// The deploy asks for gas; the top-up must not.

/// These are opposite requests and the difference is irreversible. `flag=16` tells the wallet to
/// convert the SHELL into native vmshell on arrival, which is exactly what an UNINIT address needs
/// -- the deploy spends gas, and `manual_onboard_step` watches the NATIVE balance for it. Sending
/// the same flag to the Active wallet a top-up funds would turn spendable ECC[2] SHELL into gas
/// that, in the wallet protocol's own words, "cannot be spent as ECC[2] currency or converted
/// back".
#[test]
fn the_deploy_asks_for_gas_and_the_top_up_asks_for_currency() {
    let address = extended_address();

    let deploy = super::manual_deploy_payment_link(&address, "shellnet");
    assert!(
        deploy.contains("&flag=16"),
        "the deploy funds an address with no contract, and the gas it spends is what must land: \
         {deploy}"
    );

    let top_up = super::payment_link(&address, 100, "shellnet", PaymentFlag::None);
    assert!(
        !top_up.contains("flag"),
        "a top-up credits an Active wallet with spendable SHELL. Carrying flag 16 here converts \
         it to native gas on arrival, and that cannot be undone: {top_up}"
    );

    // Driven through the printing path, not just the builder. The first version of this test
    // passed `PaymentFlag::None` in by hand and then asserted it came back out -- so mutating the
    // TOP-UP CALL SITE to ask for recipient gas left all six tests green. The flag that matters is
    // the one the top-up path chooses, not the one a test chose for it.
    let mut drawn = Vec::new();
    super::write_payment_qr(&mut drawn, &address, 100, "shellnet");

    let as_drawn = |link: &str| {
        let code = crate::cli::qr_compact::smallest_code(link.as_bytes()).expect("the link fits");
        let mut rendered = Vec::new();
        crate::cli::qr_display::write_qr(&mut rendered, &code).expect("draw it");
        rendered
    };
    let currency = as_drawn(&top_up);
    let gas = as_drawn(&super::payment_link(
        &address,
        100,
        "shellnet",
        PaymentFlag::RecipientGas,
    ));

    assert!(
        drawn
            .windows(currency.len())
            .any(|w| w == currency.as_slice()),
        "the top-up path must print the code that asks for spendable SHELL"
    );
    assert!(
        !drawn.windows(gas.len()).any(|w| w == gas.as_slice()),
        "the top-up path printed the code that converts the operator's SHELL to native gas on \
         arrival -- irreversibly, and on an Active wallet that wanted currency"
    );
}

/// The flag is only ever sent where the wallet will accept it.

/// `assertPaymentFlagContext` rejects a flagged request unless the mode is `regular`, the token is
/// `2`, and the recipient is an extended address. Ours is all three -- but the test pins it,
/// because a future caller passing a legacy `0:<hex>` address would produce a link that fails as a
/// whole rather than merely ignoring the field.
#[test]
fn a_flagged_link_carries_the_context_the_wallet_requires_for_a_flag() {
    let address = extended_address();
    let link = super::manual_deploy_payment_link(&address, "mainnet");

    assert!(
        link.contains(&format!("&token={}", dexdo_core::params::SHELL_CURRENCY_ID)),
        "a flag is accepted only alongside token 2: {link}"
    );
    assert!(
        !link.contains("&mode="),
        "mode is absent, which the protocol defines as `regular` -- the only mode a flag is valid \
         in: {link}"
    );
    // Pinned as the wallet's own regex, `/^([0-9a-f]{64})::([0-9a-f]{64})$/`, not merely as two
    // 64-character halves: `assertPaymentFlagContext` throws `unsupported_flag_context` for the
    // WHOLE request when a flagged link's recipient does not match. Adding the flag turned a
    // tolerant link into an all-or-nothing one, so an uppercase half would now cost the payment
    // rather than a nicety. `CanonicalAddress` lowercases both halves today, which is what keeps
    // this latent -- and is exactly why it is worth pinning here.
    let recipient = link.split('&').next().unwrap_or_default();
    let halves: Vec<&str> = recipient.split("::").collect();
    assert!(
        halves.len() == 2
            && halves.iter().all(|half| {
                half.len() == 64
                    && half
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            }),
        "a flag is accepted only for an extended `<dapp64>::<account64>` recipient in lowercase \
         hex, and a flagged link that is not one is rejected whole: {recipient}"
    );
}

/// The top-up asks on a named chain too, not only the deploy.

/// Both moments print a code, and both were silent about the network -- the top-up got its QR in
/// which is what put a scannable code in front of an operator at the one moment they are
/// holding a phone. Fixing only the deploy would leave the newer of the two paths exactly as
/// found it. Driven through the provider that actually prints it.
#[test]
fn the_top_up_instruction_names_the_chain_it_wants_the_money_on() {
    use crate::cli::wallet::WalletProvider;
    use crate::cli::wallet_funding::{providers, HotFundingProvider};

    // Both providers served by the direct top-up flow, because review found the network had been
    // given to one of them and not the other, and the address is equally ambiguous for both.
    for provider in [WalletProvider::Manual, WalletProvider::GoshAi] {
        let top_up = providers::DirectTopUpProvider::new(provider)
            .expect("this provider is served by the direct top-up flow");

        for network in ["shellnet", "mainnet", "net-a"] {
            let request = top_up_request(network, 100 * dexdo_core::params::SHELL_UNIT);
            let said = top_up.manual_instruction(&request);

            assert!(
                said.contains(network),
                "the operator is asked for money against an address that reads the same on both \
                 chains, and is not told which one: {said}"
            );
        }
    }
}

/// The request explains the conversion the code performs, instead of asserting it happens by itself.

/// This text used to say the SHELL "lands as native gas" on an address with no contract yet. The
/// repository's own measurements say otherwise: `ledger.md` records ECC[2] sent at flag 1 arriving
/// as ECC[2] and only flag 16 arriving as native vmshell, and states the same rule.
/// Being uninit is not what converts. The QR now carries `flag=16` and the wallet ticks the
/// auto-convert box from it -- but the address is printed whole precisely so it can be typed by
/// hand, and that path converts nothing. The sentence has to cover both, because the failure is
/// silent: the SHELL arrives as currency, `manual_onboard_step` watches a native balance that never
/// moves, and the command waits out its timeout with the operator's money already spent.
#[test]
fn the_request_says_what_makes_the_shell_arrive_as_gas() {
    let shown = super::render_manual_deploy_funding_request(&extended_address(), 0, "shellnet");
    let said = shown.to_lowercase();

    assert!(
        said.contains("auto-convert"),
        "the operator is not told what turns their SHELL into the gas the deploy spends: {shown}"
    );
    assert!(
        said.contains("scan"),
        "the scanned path is the one that sets it, and it is the ordinary path: {shown}"
    );
    assert!(
        said.contains("by hand"),
        "the address is printed to be copied, and a copied address converts nothing -- the text \
         has to say so: {shown}"
    );
    assert!(
        !said.contains("no contract yet it lands as native gas"),
        "the claim that being uninit is what converts is measured false in ledger.md: {shown}"
    );
}

/// The request the operator reads names the network, on every manifest.

/// This is the half that does not depend on the wallet having been updated, and it is the half
/// that would have stopped the incident: the owner was reading this text with a phone in hand.
/// It is stated for a label the link cannot carry too -- a private manifest is exactly the case
/// where the machine-readable guard is absent, so the human-readable one matters most.
#[test]
fn the_funding_request_names_the_network_even_when_the_link_cannot() {
    let address = extended_address();

    for network in ["shellnet", "mainnet", "net-a"] {
        let shown = super::render_manual_deploy_funding_request(&address, 0, network);

        assert!(
            shown.contains(network),
            "the operator is being asked to send money and is not told onto which chain: {shown}"
        );
        assert!(
            shown.contains(&address),
            "the address stays whole beside it: {shown}"
        );
    }

    // And it is the network that changes the text, not something incidental.
    assert_ne!(
        super::render_manual_deploy_funding_request(&address, 0, "mainnet"),
        super::render_manual_deploy_funding_request(&address, 0, "shellnet"),
        "the request must read differently for the two chains"
    );
}
