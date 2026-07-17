//! Seller command handler(Track C13, move-only).

use crate::cli::args::SellerArgs;
use crate::cli::commands::{
    enforce_model_registry_policy, expected_order_book_for_note,
    load_enabled_model_registry_policy, order_book_active_from_contracts, save_runtime_deal_handle,
    shellnet_doctor_preflight, RuntimeDealHandleInput,
};
use crate::cli::deals;
use crate::cli::policy;
use crate::cli::seller_policy::{
    apply_seller_dispute_policy, apply_seller_terminal_policy, classify_by_fact_advance_failure,
    is_err_not_open, AdvanceFailureDisposition, SellerTerminalPolicyOutcome,
};
use crate::cli::support::*;
use anyhow::{anyhow, bail, Result};
use dexdo::registry::{BuyerMissingBookPolicy, RegistryRole};
use dexdo_core::{DobParams, SellOfferOutcome};
use std::io::Write as _;

fn seller_offer_outcome_line(outcome: &SellOfferOutcome) -> String {
    match outcome {
        SellOfferOutcome::Rested { order_id } => {
            format!("seller_offer_outcome RESTED order_id={order_id}")
        }
        SellOfferOutcome::Matched => "seller_offer_outcome MATCHED".to_string(),
    }
}

fn seller_watch_cursor_path(
    deals_dir: Option<&std::path::Path>,
    token_contract: &str,
) -> Result<std::path::PathBuf> {
    Ok(deals::resolve_deals_dir(deals_dir)?
        .join("seller-watch")
        .join(format!(
            "{}.cursor.json",
            deals::make_handle_id(token_contract)
        )))
}

pub(crate) fn enforce_seller_runtime_policy(policy: &policy::SellerRuntimePolicy) -> Result<()> {
    if policy.max_open_deals != 1 {
        bail!(
            "policy_action failure_class=seller.max_open_deals action=enforce token_contract=<not-posted> \
             state=pre_offer result=unsupported_max_open_deals requested={} supported=1; \
             current seller daemon owns exactly one per-deal TokenContract",
            policy.max_open_deals
        );
    }
    let mut unsupported = Vec::new();
    match policy.after_deal_done {
        policy::SellerAfterDealDoneAction::Retire => {}
        policy::SellerAfterDealDoneAction::Republish => {
            unsupported.push("seller.on.after_deal_done=republish");
        }
        policy::SellerAfterDealDoneAction::RepublishWithBackoff => {
            unsupported.push("seller.on.after_deal_done=republish_with_backoff");
        }
    }
    match policy.buyer_no_show {
        policy::SellerBuyerNoShowAction::CleanupAndRepublish => {
            unsupported.push("seller.on.buyer_no_show=cleanup_and_republish");
        }
        policy::SellerBuyerNoShowAction::CleanupAndRetire => {
            unsupported.push("seller.on.buyer_no_show=cleanup_and_retire");
        }
        policy::SellerBuyerNoShowAction::RetireGateway => {}
    }
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

pub(crate) async fn run_seller(args: SellerArgs) -> Result<()> {
    // Issue: the deal token_contract comes from `--market`(a provision manifest) or `--token-contract`.
    // The manifest's frame_model(if any) is validated against `--model` inside `seller_real_backend`.
    let (token_contract, market_frame_model, market_nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    // Review: the deal nonce comes from `--market`(the manifest) or the explicit `--nonce` flag --
    // never both(the manifest is the single source of truth). The real-shellnet seller path requires
    // it(see `seller_real_backend`); the mock path ignores it.
    if args.market.is_some() && args.nonce.is_some() {
        bail!("--market and --nonce are mutually exclusive -- the nonce comes from the manifest");
    }
    let seller_policy = if !args.mock.mock_chain {
        Some(policy::load_seller_runtime_policy(args.policy.as_deref())?)
    } else {
        None
    };
    if let Some(policy) = seller_policy.as_ref() {
        tracing::debug!(
            policy_after_deal_done = policy.after_deal_done.as_str(),
            policy_buyer_no_show = policy.buyer_no_show.as_str(),
            policy_dispute_against_me = policy.dispute_against_me.as_str(),
            policy_max_open_deals = policy.max_open_deals,
            "seller policy loaded"
        );
        enforce_seller_runtime_policy(policy)?;
    }
    // on the real path, the --market manifest's seller_note must be this seller's --note-addr -- else the
    // offer posts a non-canonical TC the InferenceOrderBook won't rest, and the seller never matches.
    if !args.mock.mock_chain {
        if let (Some(market), Some(note_addr)) =
            (args.market.as_deref(), args.identity.note_addr.as_deref())
        {
            let manifest = load_market(market)?;
            assert_market_seller_note(&manifest.seller_note, note_addr)?;
        }
        shellnet_doctor_preflight(&args.contracts, args.market.as_deref()).await?;
        if let Some(policy) = load_enabled_model_registry_policy(
            RegistryRole::Seller,
            &args.registry,
            &args.contracts,
        )? {
            let name = args
                .model
                .as_deref()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "real shellnet: set --model <name from config> (needed for model registry validation)"
                    )
                })?;
            let frame_model = dexdo::seller::ModelsConfig::load(&args.models)?
                .get(name)?
                .frame_model
                .clone();
            dexdo_core::validate_canonical_model_id(&frame_model)
                .map_err(|e| anyhow::anyhow!(e))?;
            check_market_model_match(market_frame_model.as_deref(), &frame_model, name)?;
            let expected_order_book = if let Some(market) = args.market.as_deref() {
                load_market(market)?.inference_order_book
            } else {
                let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "real shellnet: --note-addr is required to derive the seller order book"
                    )
                })?;
                expected_order_book_for_note(&args.contracts, note_addr, &frame_model).await?
            };
            let order_book_active =
                order_book_active_from_contracts(&args.contracts, &expected_order_book).await?;
            enforce_model_registry_policy(
                RegistryRole::Seller,
                &policy,
                &args.contracts,
                &frame_model,
                &expected_order_book,
                order_book_active,
                BuyerMissingBookPolicy::Reject,
            )
            .await?;
        }
    }
    let deal_nonce = market_nonce.or(args.nonce);
    // Upstream(model) and chain are selected independently: `--mock-model` -> mock upstream,
    // otherwise a real model from the config; `--mock-chain` -> mock chain, otherwise real shellnet
    // (per-role backend behind the feature).
    let upstream = if args.mock.mock_model {
        dexdo::seller::UpstreamConfig::Mock
    } else {
        let name = args
            .model
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "set --model <name from config> (or --mock-model for a mock upstream)"
                )
            })?;
        let models = dexdo::seller::ModelsConfig::load(&args.models)?;
        let mc = models.get(name)?;
        mc.require_api_key_present()?;
        dexdo::seller::UpstreamConfig::OpenAi(dexdo::seller::OpenAiConfig::from_model(mc))
    };
    let seller_frame_model_for_handle = if args.mock.mock_chain {
        None
    } else {
        let name = args
            .model
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "real shellnet: set --model <name from config> (needed for deal handle)"
                )
            })?;
        Some(
            dexdo::seller::ModelsConfig::load(&args.models)?
                .get(name)?
                .frame_model
                .clone(),
        )
    };
    let (chain, note) = if args.mock.mock_chain {
        let endpoints_file = resolve_endpoints_file(args.endpoints_file.clone())?;
        mock_chain_and_note(endpoints_file, &args.identity)?
    } else {
        seller_real_backend(&args, market_frame_model.as_deref(), deal_nonce)?
    };
    // the seller daemon publishes offers WITHOUT going through `provision_market`'s note-current gate, so
    // a note orphaned by a contract redeploy(stale code_hash) would hit a raw `TVM_ERROR` from `postSellOffer`.
    // Gate here: fail closed with an actionable "re-mint" message before any seller-chain read/write path.
    chain.assert_note_current().await?;
    // a withdrawn PrivateNote is final for seller writes. Fail before even reading per-deal TC terms, so a
    // withdrawn note surfaces the fresh-note action instead of any later TC/postSellOffer error.
    chain.assert_note_can_post_sell_offer().await?;
    // Real-shellnet offer terms are bound to the deployed per-deal TokenContract, not prompt/default values.
    // `deploy-market` creates only the shared model book; `dexdo provision` creates the per-deal TC carrying
    // price/maxTicks. The mock path keeps the prior fixed defaults.
    let (offer_ticks, offer_price) = if args.mock.mock_chain {
        (1024u64, args.price_per_tick)
    } else {
        let (price, ticks) = chain
            .sell_offer_terms(&token_contract)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "seller requires a deployed per-deal TokenContract; `dexdo deploy-market` only deploys \
                     the shared model order book. Run `dexdo provision --frame-model ... --nonce ...` and pass \
                     its --market manifest, or pass --token-contract plus --nonce for an already-provisioned TC."
                )
            })?;
        println!(
            "posting offer: {ticks} ticks (= {} model tokens) at {price} SHELL/tick",
            (ticks as u128).saturating_mul(DobParams::canonical().tick_size as u128)
        );
        (ticks, price)
    };
    let gateway_advertise = args.gateway_advertise_addr();
    let cfg = dexdo::seller::SellerConfig {
        token_contract: token_contract.clone(),
        price_per_tick: offer_price,
        max_ticks: offer_ticks,
        gateway_advertise: gateway_advertise.clone(),
        mock_token_count: args.mock_token_count,
    };
    // Resume path: a matched buyer can fund this per-deal TC while no seller process was live (the deal ends up
    // `funded-but-never-opened`). Because a `(sellerPubkey, nonce)` TC is single-use, re-posting the offer would
    // fail -- but the stream can still be opened. This pre-offer probe MUST be non-blocking: fresh normal
    // sellers must post their ask immediately, while `read_match` remains the later wait-loop after the ask rests.
    let already_matched = match chain.read_openable_match_now(&token_contract).await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(e) => {
            return Err(anyhow!(
                "seller: existing-match resume preflight failed for {token_contract}: {e}"
            ));
        }
    };
    if already_matched {
        tracing::info!(
            token_contract = %token_contract,
            "seller: TC already funded by a matched buyer (funded-but-never-opened) -- resuming: skipping offer post, opening stream"
        );
    } else {
        // a deterministic per-deal TC(sellerPubkey + nonce) is single-use. If a prior deal already used this
        // nonce's TC(opened/funded/disputed/residual), the seller's pre-stream steps revert with a raw `TVM_ERROR`
        // (ERR_ALREADY_OPEN 321). Fail closed BEFORE post_offer with an actionable "fresh --nonce / recover+destroy"
        // message(the mock backend no-ops; a fresh active-but-unfunded TC passes).
        chain.assert_token_contract_fresh(&token_contract).await?;
        tracing::info!(token_contract = %token_contract, "seller posting offer, awaiting buy + match");
        dexdo::seller::post_offer_with_note(note.as_ref(), chain.as_ref(), &cfg).await?;
        if let Some(outcome) = chain.confirm_offer_outcome(&token_contract).await? {
            println!("{}", seller_offer_outcome_line(&outcome));
        }
    }
    let seller =
        dexdo::seller::start_gateway_with_note(args.gateway_listen, upstream, note).await?;
    println!(
        "seller_ready token_contract={} gateway={} gateway_listen={} readiness={}",
        token_contract,
        gateway_advertise,
        args.gateway_listen,
        if already_matched {
            "resumed_funded_tc"
        } else {
            "exact_tc_offer_accepted"
        }
    );
    let _ = std::io::stdout().flush();
    // match wait + access-handover provisioning belong to the long-running gateway path, not the
    // one-shot seller post flow. The watcher polls the note/fill source(or mock equivalent) with a durable
    // cursor and waits indefinitely while the offer is open; no 300s seller deadline tears down a resting ask.
    let watch = dexdo::seller::SellerMatchWatchConfig {
        cursor_path: seller_watch_cursor_path(args.deals_dir.as_deref(), &token_contract)?,
        poll_interval: dexdo::seller::DEFAULT_MATCH_POLL_INTERVAL,
    };
    let matched =
        dexdo::seller::watch_and_serve_match(&seller, chain.as_ref(), &cfg, &watch).await?;
    println!(
        "seller_match_opened token_contract={} gateway={} gateway_listen={} cursor={}",
        matched.token_contract,
        gateway_advertise,
        args.gateway_listen,
        watch.cursor_path.display()
    );
    let _ = std::io::stdout().flush();
    if !args.mock.mock_chain {
        let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
            anyhow::anyhow!("real shellnet: --note-addr is required to save the deal handle")
        })?;
        save_runtime_deal_handle(
            RuntimeDealHandleInput {
                role: deals::DealHandleRole::Seller,
                deals_dir: args.deals_dir.as_deref(),
                token_contract: &token_contract,
                note_addr,
                frame_model: seller_frame_model_for_handle.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("real shellnet: missing frame_model for deal handle")
                })?,
                market_path: args.market.as_deref(),
                contracts: &args.contracts,
                endpoint: Some(deals::DealEndpointInfo {
                    kind: "gateway".to_string(),
                    value: gateway_advertise.clone(),
                }),
            },
            true,
        )?;
    }
    if let Some(policy) = seller_policy.as_ref() {
        if apply_seller_dispute_policy(chain.as_ref(), &token_contract, policy, "pre-advance")
            .await?
        {
            return Ok(());
        }
    }
    // on the shipped real-money path, drive the seller's by-fact advance. Both safety
    // prerequisites the lead required are met: `drive_advance` is **delivery-bounded** (finalized
    // ticks <= the gateway's delivered canonical-token count, `seller.state.delivery(tc)`, with a merged
    // regression) and it exits on `deal_closed()`. The buyer session-scoped STOP keeps the deal alive
    // across requests so the probe is accepted and ticks finalize by-fact(`AmicableSplit`, no `BurnBoth`).
    // Real-chain only -- the mock chain has no `getConfig` advance window.
    // Two money-path requirements:
    // 1. The stream-phase cadence is `getConfig().settleWindow`; a getter failure must NOT become a silent
    // wrong cadence(advancing too early -> the contract rejects the tick -> the loop dies). Read it
    // FAIL-LOUD before spawning, with TC context -- no default cadence on the real path.
    // 2. `drive_advance` propagates real advance failures as money-path faults. So the task is
    // SUPERVISED, not fire-and-forget: an `Err` is propagated out of `run_seller` (non-zero exit -- by-fact
    // settlement is dead, the gateway must not keep serving as if healthy). Only clean terminals
    // (`Ok(finalized)` / `deal_closed`) are logged and let the gateway serve until shutdown.
    let advance_task = if !args.mock.mock_chain {
        let delivery = seller.state.delivery(&token_contract);
        let settle = chain.deal_settle_window(&token_contract).await.map_err(|e| {
            anyhow::anyhow!(
                "--token-contract {token_contract}: getConfig().settleWindow is unreadable, refusing to \
                 start by-fact advance on a guessed cadence: {e}"
            )
        })?;
        let windows = dexdo::seller::AdvanceWindows::from_settle_window(settle);
        let advance_chain = chain.clone();
        let advance_note = seller.note.clone();
        let advance_tc = token_contract.clone();
        let tick_budget = cfg.max_ticks as u128;
        let tick_size = dexdo_core::DobParams::canonical().tick_size;
        Some(tokio::spawn(async move {
            dexdo::seller::drive_advance(
                advance_chain.as_ref(),
                &advance_tc,
                advance_note.as_ref(),
                windows,
                tick_budget,
                tick_size,
                delivery.count,
                delivery.done,
            )
            .await
        }))
    } else {
        None
    };
    tracing::info!("stream open; serving until shutdown");
    let mut server_task = seller.server_task;
    match advance_task {
        // Supervise: whichever of {by-fact advance, gateway server} ends first decides the exit.
        Some(advance_task) => {
            tokio::select! {
                advanced = advance_task => match advanced {
                    Ok(Ok(finalized)) => {
                        tracing::info!(
                            token_contract = %token_contract, finalized,
                            "drive_advance: finalized ticks by-fact (<= delivered), deal closed; serving until shutdown"
                        );
                        if let Some(policy) = seller_policy.as_ref() {
                            match apply_seller_terminal_policy(&token_contract, policy, finalized)? {
                                SellerTerminalPolicyOutcome::StopServing => {
                                    server_task.abort();
                                    return Ok(());
                                }
                            }
                        }
                        server_task.await?;
                    }
                    Ok(Err(e)) => {
                        if is_err_not_open(&e) {
                            match classify_by_fact_advance_failure(
                                chain.as_ref(),
                                &token_contract,
                                &e,
                            )
                            .await
                            {
                                Ok(AdvanceFailureDisposition::BenignTerminal { reason }) => {
                                    tracing::info!(
                                        token_contract = %token_contract,
                                        %reason,
                                        "drive_advance: ERR_NOT_OPEN is terminal for this unopened/no-money deal"
                                    );
                                    println!(
                                        "by_fact_advance_terminal token_contract={token_contract} \
                                         action=retire_gateway {reason}"
                                    );
                                    server_task.abort();
                                    return Ok(());
                                }
                                Ok(AdvanceFailureDisposition::Fault { reason }) => {
                                    return Err(anyhow::anyhow!(
                                        "--token-contract {token_contract}: by-fact advance failed \
                                         (money-path fault), stopping the seller: {e}; ERR_NOT_OPEN \
                                         terminal check: {reason}"
                                    ));
                                }
                                Err(classify_err) => {
                                    return Err(anyhow::anyhow!(
                                        "--token-contract {token_contract}: by-fact advance failed \
                                         (money-path fault), stopping the seller: {e}; ERR_NOT_OPEN \
                                         terminal check: reason=terminal_classification_failed \
                                         error={classify_err}"
                                    ));
                                }
                            }
                        }
                        if let Some(policy) = seller_policy.as_ref() {
                            if apply_seller_dispute_policy(
                                chain.as_ref(),
                                &token_contract,
                                policy,
                                "advance-error",
                            )
                            .await?
                            {
                                server_task.abort();
                                return Ok(());
                            }
                        }
                        return Err(anyhow::anyhow!(
                            "--token-contract {token_contract}: by-fact advance failed (money-path fault), \
                             stopping the seller: {e}"
                        ));
                    }
                    Err(join) => {
                        return Err(anyhow::anyhow!(
                            "--token-contract {token_contract}: by-fact advance task panicked: {join}"
                        ));
                    }
                },
                served = &mut server_task => served?,
            }
        }
        None => server_task.await?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::seller_offer_outcome_line;
    use dexdo_core::SellOfferOutcome;

    /// static guard -- the seller publishes its offer and confirms THIS TC either rested in the IOB or
    /// immediately matched/funded BEFORE binding the gateway, so "gateway listening" cannot false-green as market
    /// readiness on an empty and unmatched book.
    #[test]
    fn seller_gateway_listens_only_after_offer_acceptance_guard() {
        let source = include_str!("seller.rs");
        let terms = source
            .find(&["sell_", "offer_terms(&token_contract)"].concat())
            .expect("seller reads authoritative TC terms before posting");
        let resume_probe = source
            .find(&["read_", "openable_match_now(&token_contract)"].concat())
            .expect("seller uses a non-blocking resume probe before posting");
        let post = source
            .find(&["post_offer", "_with_note(note.as_ref()"].concat())
            .expect("seller posts the offer before opening the gateway");
        let withdrawn = source
            .find(&["assert_note_can_", "post_sell_offer()"].concat())
            .expect("seller checks withdrawn note state before posting");
        let accepted = source
            .find(&["confirm_", "offer_outcome(&token_contract)"].concat())
            .expect("seller confirms this TC's postSellOffer outcome");
        let gateway = source
            .find(&["start_gateway", "_with_note(args.gateway_listen"].concat())
            .expect("seller starts the gateway");
        let real_backend = include_str!("../../../core/src/shellnet/backends.rs");
        let guard = real_backend
            .find("async fn confirm_offer_outcome(")
            .expect("real seller outcome confirmation present");
        let guard_body = &real_backend[guard..];

        assert!(
            terms < post,
            "seller offer terms must come from the deployed TC before posting"
        );
        assert!(
            terms < resume_probe && resume_probe < post,
            "fresh seller startup must use the non-blocking resume probe before post_offer"
        );
        assert!(
            !source[terms..post].contains("read_match(&token_contract)"),
            "fresh seller startup must not call the read_match wait-loop before post_offer"
        );
        assert!(!source.contains(&["assert_", "no_active_sell_order"].concat()));
        assert!(
            withdrawn < post,
            "seller must reject withdrawn notes before postSellOffer"
        );
        assert!(
            post < accepted,
            "seller must publish the offer before checking IOB acceptance"
        );
        assert!(
            accepted < gateway,
            "seller gateway must not listen before this TC's offer rested or immediately matched"
        );
        assert!(
            guard_body.contains("read_openable_match_once(tc)"),
            "post-offer acceptance must allow an immediate funded/openable match"
        );
        assert!(
            guard_body.contains("seller_offer_events_since"),
            "post-offer acceptance must inspect this seller note's exact placement/fill events"
        );
        assert!(guard_body.contains("retry_seller_read"));
        assert!(!guard_body.contains("active_sell_order_ids_for_exact_tc_bounded"));
    }

    #[test]
    fn seller_offer_placed_reports_rested_with_order_id() {
        assert_eq!(
            seller_offer_outcome_line(&SellOfferOutcome::Rested { order_id: 835 }),
            "seller_offer_outcome RESTED order_id=835"
        );
    }

    #[test]
    fn seller_offer_immediate_match_reports_matched() {
        assert_eq!(
            seller_offer_outcome_line(&SellOfferOutcome::Matched),
            "seller_offer_outcome MATCHED"
        );
    }

    /// seller-side ModelRegistry validation must happen before any offer write can move into
    /// `postSellOffer`.
    #[test]
    fn seller_model_registry_preflight_precedes_offer_post() {
        let source = include_str!("seller.rs");
        let start = source
            .find("pub(crate) async fn run_seller")
            .expect("run_seller present");
        let end = source[start..]
            .find("#[cfg(test)]\nmod tests")
            .map(|offset| start + offset)
            .expect("run_seller end marker present");
        let body = &source[start..end];

        let registry = body
            .find("load_enabled_model_registry_policy")
            .expect("seller registry policy load present");
        let role = body[registry..]
            .find("RegistryRole::Seller")
            .map(|offset| registry + offset)
            .expect("seller registry role present");
        let enforce = body[registry..]
            .find("enforce_model_registry_policy(")
            .map(|offset| registry + offset)
            .expect("seller registry preflight present");
        let post = body
            .find(&["post_offer", "_with_note(note.as_ref()"].concat())
            .expect("seller post_offer present");

        assert!(
            registry < role && role < enforce && enforce < post,
            "seller registry validation must run before postSellOffer"
        );
    }

    /// regression: `run_seller` must not own the old bounded match wait. After the offer is posted/rested
    /// and the gateway is listening, match wait + handover provisioning are delegated to the gateway watcher.
    #[test]
    fn seller_run_path_uses_gateway_watcher_not_bounded_read_match() {
        let source = include_str!("seller.rs");
        let start = source
            .find("pub(crate) async fn run_seller")
            .expect("run_seller present");
        let end = source[start..]
            .find("#[cfg(test)]\nmod tests")
            .map(|offset| start + offset)
            .expect("run_seller end marker present");
        let body = &source[start..end];

        assert!(
            body.contains("watch_and_serve_match"),
            "seller match wait must be gateway-owned"
        );
        assert!(
            body.contains("seller_watch_cursor_path"),
            "gateway watcher must persist a cursor"
        );
        assert!(
            body.contains("DEFAULT_MATCH_POLL_INTERVAL"),
            "gateway watcher must use the ~30s default poll interval"
        );
        assert!(
            !body.contains("read_match(&token_contract)"),
            "run_seller must not block on the old read_match loop"
        );
        assert!(
            !body.contains("DEAL_WAIT_SECS"),
            "run_seller must not carry the old 300s seller deadline"
        );

        let ready = body.find("seller_ready").expect("seller_ready printed");
        let watch = body.find("watch_and_serve_match").expect("watcher started");
        assert!(
            ready < watch,
            "seller posts/rests and reports readiness before entering the long-running watcher"
        );
    }

    #[test]
    fn policy_seller_fields_dispatch_or_fail_closed_explicitly() {
        let source = include_str!("seller.rs");
        let seller_policy_source = include_str!("seller_policy.rs");
        let enforce = source
            .find("fn enforce_seller_runtime_policy")
            .expect("seller max-open policy helper present");
        let run = source
            .find("pub(crate) async fn run_seller")
            .expect("run_seller present");
        let helpers = &source[enforce..run];
        assert!(
            helpers.contains("supported=1"),
            "seller max_open_deals must be enforced before offer posting"
        );
        assert!(
            seller_policy_source.contains("chain.release_dispute(token_contract)"),
            "seller dispute_against_me=release_if_clean must invoke release_dispute"
        );
        assert!(
            seller_policy_source.contains("policy_action_unsupported"),
            "seller unsupported republish/cleanup surfaces must fail closed explicitly"
        );
        assert!(
            seller_policy_source.contains("action=retire_gateway"),
            "seller buyer_no_show=retire_gateway must have an explicit runtime terminal action"
        );

        let end = source[run..]
            .find("#[cfg(test)]\nmod tests")
            .map(|offset| run + offset)
            .expect("run_seller end marker present");
        let body = &source[run..end];
        let enforce = body
            .find("enforce_seller_runtime_policy(policy)?")
            .expect("seller policy enforcement present");
        let doctor = body
            .find("shellnet_doctor_preflight")
            .expect("real shellnet preflight present");
        let post_offer = body
            .find("dexdo::seller::post_offer_with_note")
            .expect("seller offer post present");
        assert!(enforce < doctor);
        assert!(enforce < post_offer);
        assert!(body.contains("apply_seller_dispute_policy"));
        assert!(body.contains("apply_seller_terminal_policy"));

        let advance_error = body
            .find("Ok(Err(e)) => {")
            .expect("supervised advance error branch present");
        let join_error = body[advance_error..]
            .find("Err(join)")
            .map(|offset| advance_error + offset)
            .expect("advance error branch end marker present");
        let branch = &body[advance_error..join_error];
        assert!(
            branch.contains("is_err_not_open(&e)")
                && branch.contains("classify_by_fact_advance_failure")
                && branch.contains("by_fact_advance_terminal"),
            "ERR_NOT_OPEN must be classified before the seller turns it into a process fault"
        );
        let classify = branch
            .find("classify_by_fact_advance_failure")
            .expect("ERR_NOT_OPEN classifier present");
        let policy = branch
            .find("apply_seller_dispute_policy")
            .expect("non-ERR_NOT_OPEN dispute policy fallback present");
        assert!(
            classify < policy,
            "unsafe ERR_NOT_OPEN must return a money-path fault before generic dispute policy can consume it"
        );
    }
}
