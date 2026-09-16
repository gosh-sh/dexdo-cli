//! Market/pool lifecycle administration command handlers.

use crate::cli::args::{DestroyArgs, MarketDeployArgs, ProvisionArgs, ProvisionNonceArg};
use crate::cli::commands::{
    chain_doctor_preflight, enforce_model_registry_policy,
    enforce_model_registry_policy_with_endpoint, load_enabled_model_registry_policy,
    order_book_active, preload_default_model_registry_with_endpoint, preload_model_registry_policy,
    preload_model_registry_policy_with_endpoint, resolve_model_registry_target,
    resolve_model_registry_target_with_endpoint, resolve_registry_content_identity, BookTarget,
};
use crate::cli::policy;
use crate::cli::support::{
    default_deposit_shells, deposit_per_deploy, deposit_per_deploy_with_overhead,
    ensure_provision_deposit_covered, prompt_deposit_shells, require_model_name,
    require_provision_nonce, resolve_market_fields, validate_price_step, SHELL_UNIT,
};
use anyhow::Result;
use dexdo::registry::{BuyerMissingBookPolicy, RegistryRole, RegistrySuggestions};
use dexdo_core::params::{
    MARKET_DEPLOY_ACTIVATION_MAX_READS, MARKET_DEPLOY_ACTIVATION_POLL_INTERVAL,
};
use rand::RngCore as _;
use std::future::Future;

async fn choose_provision_nonce<G, C, Fut>(
    requested: ProvisionNonceArg,
    mut generate: G,
    mut candidate_is_available: C,
) -> Result<u64>
where
    G: FnMut() -> u64,
    C: FnMut(u64) -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    match requested {
        ProvisionNonceArg::Explicit(nonce) => Ok(nonce),
        ProvisionNonceArg::Auto => loop {
            let candidate = generate();
            if candidate_is_available(candidate).await? {
                return Ok(candidate);
            }
        },
    }
}

/// refuse a provision whose model name the buyer's registry lookup would reject.

/// The market a provision brings up is only useful if a buyer can find it, and the buyer finds it by
/// resolving the model name in the ModelRegistry. When that resolution fails the buyer refuses --
/// fail-closed and correct -- but by then the seller has already paid for an order book, a RootModel
/// and a TokenContract that will sit there and never trade.

/// So the answer is taken here, from the same resolver, before the first deploy. The error the
/// operator sees is the resolver's own, which is the buyer's: the same candidate list, the same
/// registry address, so a typo in `--frame-model` is distinguishable from a correct name at the
/// moment it is cheap to fix.

/// `--allow-unverified-model` is the opt-out, and it is deliberately the buyer's flag rather than a
/// new one: the defect reports is that one identity rule had two different defaults on its two
/// sides. Matching the buyer means matching its default AND its escape hatch.

/// `command` names the command the operator actually ran. `deploy-market` asks this same
/// question now, and a refusal hard-coded to say "provision" would tell them a command they did not
/// run had refused -- the class of defect this branch exists to remove.
pub(crate) async fn ensure_model_resolves(
    caller: ModelResolutionCaller,
    contracts: &std::path::Path,
    endpoint: Option<&str>,
    requested_model: &str,
    allow_unverified_model: bool,
) -> Result<()> {
    // The FLAG axis, judged before the registry is read.

    // A name that claims capability flags is claiming a grammar, and a claimed grammar can be wrong
    // on its own terms -- `--toolz`, `--fp9`, `--tools--w8k` out of order -- without the chain
    // having any part in the answer. That verdict needs no network, so it is reached without one,
    // and the refusal names the offending token instead of the generic "does not resolve" the
    // registry read would otherwise have produced for the same name.
    ensure_model_flags_are_canonical(caller, requested_model, allow_unverified_model)?;
    let resolved = resolve_registry_content_identity(
        RegistryRole::Seller,
        contracts,
        endpoint,
        requested_model,
        // The operator already said "go on". `model_resolution_result` below
        // turns this Err into a warning when the flag is set, so a suggestion list computed here
        // is built for a refusal that will not happen. The warning still reports the fact.
        if allow_unverified_model {
            RegistrySuggestions::Skip
        } else {
            RegistrySuggestions::Compute
        },
    )
    .await;
    // `.map(drop)` and that is now correct rather than merely convenient. reported the
    // discarded name as the defect, and the narrower truth is that DISCARDING was never the
    // problem: PROCEEDING on a spelling the registry holds differently was. With that refused
    // below, a successful resolution returns a name equal to the one that was asked about, so
    // there is nothing left to carry.
    model_resolution_result(caller, requested_model, allow_unverified_model, resolved).map(drop)
}

/// Does a name that CLAIMS capability flags spell them the one canonical way?

/// **The source, and there is one.** The flag grammar is defined exactly once in this tree, by
/// `dexdo_core::parse_canonical_model_id` (`crates/core/src/manifest.rs`), whose own doc states it:
/// `producer--model--version[--unit][--w<N>k][--<N>p][--tools][--think][--<precision>]`, each slot at
/// most once, the order fixed, and rejecting rather than normalizing.

/// CALLS that definition rather than restating it: a second statement of a grammar is a second
/// grammar, and two of them on one name is the defect was about.

/// **This is not the shape gate removed, and the bound is what makes that true.**
/// `validate_canonical_model_id` used to stand on the operate commands and require three `--` parts
/// of EVERY name, so `Qwen3-32B` -- which the 4.0.36 catalog actually holds -- was refused before the
/// catalog was asked. This runs only when the name ALREADY has more than three parts, which is to say
/// only when it claims a flag tail. No name the registry can hold reaches it: the catalog carries no
/// `--` at all (`grep -c -- "--" contracts/canonical-model-ids.md` is 0 over its 11257 lines, and the
/// control -- the same pattern over a copy with one flagged name prepended -- reports 1), so a name
/// with a flag tail is never a registered spelling. `.nth(3)` is the same base arity the dropped-flag
/// scan below counts with `.skip(3)`.

/// **`--allow-unverified-model` bypasses this, and that is the directive's own scoping rather than a
/// softening.** Section 5 lists exactly one thing the flag does not cover -- a name the registry
/// confirmed under a different spelling, a confirmed model plus an operator typo -- and that arm
/// stays un-bypassable where it already is, below. Everything else the flag covers, and its stated
/// business purpose is trading a model the catalog does not confirm: a NEW one, or the operator's
/// OWN. An own name is precisely a name this grammar has no authority over, and
/// `acme--vision--v1--internal` is indistinguishable in code from a canonical name carrying one bad
/// flag. Refusing it under the flag would be the shape gate returning at a new address -- a
/// name refused for its shape.

/// So on the default path the gate bites, and under the flag the operator has already said the
/// catalog is not the authority on this name. Measured before choosing: of the 14 names with a flag
/// tail anywhere in this tree, the 5 that fail this grammar are all deliberate negative fixtures
/// (`crates/core/tests/canonical_model_id_1225.rs`, `crates/core/src/manifest.rs`, and the alias case
/// in `registry.rs`); no operator-facing name in the tree is affected either way.
pub(crate) fn ensure_model_flags_are_canonical(
    caller: ModelResolutionCaller,
    requested_model: &str,
    allow_unverified_model: bool,
) -> Result<()> {
    if allow_unverified_model || requested_model.split("--").nth(3).is_none() {
        return Ok(());
    }
    let command = caller.command();
    let paid_for = caller.paid_for();
    dexdo_core::parse_canonical_model_id(requested_model).map_err(|reason| {
        anyhow::anyhow!(
            "{command} refuses before deploying anything: `{requested_model}` carries capability \
             flags and {reason}. The canonical flag grammar is \
             `producer--model--version[--<unit>][--w<N>k][--<N>p][--tools][--think][--<precision>]` \
             -- every flag is one of those tokens, each slot appears at most once, and the slots keep \
             that order. Nothing was sent, and {paid_for} had this gone through"
        )
    })?;
    Ok(())
}

/// Did the registry ANSWER "no", or could it not be asked?

/// the two were reported identically, and the difference is what the operator does next. A
/// name the catalog does not carry is fixed by changing the name. An endpoint that is rate-limiting
/// (the chain answers 403 above three requests a second from one address), a manifest whose
/// ModelRegistry account is absent, a network that is down -- none of those are the name's fault,
/// and telling the operator to "fix the --frame-model name, or pass --allow-unverified-model" for
/// one of them advises editing a correct name or disabling the check that never ran.

/// Recognised by TYPE -- `dexdo::registry::RegistryAnswered`, which the resolver attaches to every
/// verdict it reaches by reading the catalog. Anything without it never got an answer.

/// A word list over the rendered error was the first attempt and is not sound: that text
/// interpolates the claimed model, the candidates, the suggestions and the registry address, so an
/// operator's own model name (`acme--vision--403b`) or a future registry account id containing
/// `403`/`503`/`dns` would flip a real membership miss into "retry later" -- which this function's
/// own reason says is the worse of the two errors. It also missed most genuine read failures: a
/// manifest with no ModelRegistry key, an inactive registry account, an empty account BOC, and every
/// transport error whose words were not among the eleven listed.
fn registry_was_unreachable(error: &anyhow::Error) -> bool {
    !dexdo::registry::registry_answered(error)
}

/// Which command is asking, and therefore what it pays for if it goes ahead on a name no buyer can
/// resolve.

/// An enum and not a `&str`: the refusal states a COST, and an operator reads that cost to
/// decide whether to pass the opt-out. A string match needs a catch-all, and a third caller that
/// spelled its name differently would silently be told the provision spend -- a false statement
/// about money, of exactly the kind the caller-naming fix was made to stop. Adding a variant here
/// makes both arms fail to compile until they are answered.
#[derive(Clone, Copy)]
pub(crate) enum ModelResolutionCaller {
    /// Deploys the order book, the RootModel and the per-deal TokenContract.
    Provision,
    /// Deploys the order book, and nothing else.
    DeployMarket,
    /// Lists the model: posts the sell offer, and deploys the order book first when it is absent.

    /// The book deploy is not hypothetical and it is why this variant exists. The seller's
    /// own `post_offer` reads `inference_orderbook_stats` and, on `None`, calls
    /// `deploy_inference_orderbook` out of the note before it writes the ask
    /// (`crates/core/src/chain/backends.rs`, "An operate exception:... deploy it
    /// (model listing)"). So `dexdo seller` is a command that CREATES a market under whatever name
    /// it was handed, which is exactly the spend `provision` and `deploy-market` already ask the
    /// registry about first.
    Seller,
}

impl ModelResolutionCaller {
    /// The command as the operator typed it, which is how the refusal must name it.
    fn command(self) -> &'static str {
        match self {
            Self::Provision => "provision",
            Self::DeployMarket => "deploy-market",
            Self::Seller => "seller",
        }
    }

    /// What goes unrecoverably to gas if this command proceeds on an unresolvable name.
    fn paid_for(self) -> &'static str {
        match self {
            Self::Provision => {
                "the order book, RootModel and TokenContract would be paid for and never trade"
            }
            Self::DeployMarket => "the order book would be paid for and never trade",
            Self::Seller => {
                "the order book this listing deploys when it is absent would be paid for and never \
                 trade"
            }
        }
    }
}

/// The decision the resolution feeds, split out so it is exercised without a node -- the shape
/// `cli::buyer` uses for the same call.
fn model_resolution_result(
    caller: ModelResolutionCaller,
    requested_model: &str,
    allow_unverified_model: bool,
    resolved: Result<String>,
) -> Result<Option<String>> {
    let command = caller.command();
    let paid_for = caller.paid_for();
    match resolved {
        Ok(registry_model) if registry_model == requested_model => Ok(Some(registry_model)),
        // RESOLVED, BUT SPELLED DIFFERENTLY -- and this is a refusal, not a substitution.

        // The registry holds this model; the file spells it another way. The client could rewrite
        // the name to the registered one and carry on, and `markets address` does exactly that for
        // a read-only answer. On a path that SPENDS, the owner's rule is the opposite
        // replacing what the operator typed reads as
        // the client being wrong rather than as help, and the operator never learns their file is
        // wrong -- it stays wrong for the next command that has no registry to ask.

        // The opt-out deliberately does NOT reach this arm. `--allow-unverified-model` means "the
        // registry does not confirm this model"; a model it confirmed under another spelling is a
        // confirmed model and a typo. Letting the flag through here would make it the way to leave
        // a wrong spelling on chain, which is the outcome it exists to prevent, not to enable.
        Ok(registry_model) => {
            // The advice is "write the registered name", and for one shape it would be wrong.

            // A name carrying capability flags loses them on the way to the candidate list --
            // `model_id_alias` drops them -- so `qwen--qwen3--32b--w8k--tools` resolves to
            // `Qwen3-32B`, and telling the operator to write that hands them a DIFFERENT market:
            // the unflagged one. Nor is there a spelling that keeps both: `Qwen3-32B--w8k--tools`
            // is read by `parse_canonical_model_id` as producer `Qwen3-32B`, model `w8k`, version
            // `tools`, and resolves to nothing.

            // Whether a flagged market may exist under the registry rule at all is not decided
            // here. What is decided is that the refusal does not pretend to have an answer it does
            // not have: it says the flags are the reason and stops, rather than handing over a name
            // that means something else.
            let dropped: Vec<&str> = requested_model
                .split("--")
                .skip(3)
                .filter(|token| {
                    // Token, not substring. `contains` would call a flag "kept" because its letters
                    // appear somewhere in the registered name -- `dexdo-executor` records that class
                    // by itself, and it fails in the direction that hides a dropped flag rather than
                    // inventing one. A capability flag is a whole `--` segment or it is nothing.
                    !registry_model
                        .split(|c: char| !c.is_ascii_alphanumeric() && c != '.')
                        .any(|part| part.eq_ignore_ascii_case(token))
                })
                .collect();
            if !dropped.is_empty() {
                return Err(anyhow::anyhow!(
                    "{command} refuses before deploying anything: the seller ModelRegistry lookup \
                     holds this model as `{registry_model}`, which is `{requested_model}` without \
                     `{}`. Those are capability flags and they are part of the market's identity, \
                     so writing `{registry_model}` would list a different, unflagged book -- and \
                     there is no spelling that carries both, because the flag grammar is built on \
                     the `producer--model--version` form the registry does not use. Nothing was \
                     sent, and {paid_for} had this gone through",
                    dropped.join("`, `")
                ));
            }
            Err(anyhow::anyhow!(
                "{command} refuses before deploying anything: the seller ModelRegistry lookup \
                 holds this model as `{registry_model}`, and `{requested_model}` is a different \
                 name to the chain -- the book address is `sha256` over the exact bytes, so \
                 listing under the second one produces a market no buyer taking the name from the \
                 registry can reach, and {paid_for}. Write `{registry_model}`. Nothing was sent"
            ))
        }
        // The opt-out branch is split by the same question as the refusals below, and it is FIRST
        // among the two so that neither states something the run does not know. With the opt-out
        // set and the registry unread, the old single arm logged "model name does not resolve in
        // the ModelRegistry" -- a verdict on a name nobody asked about, recorded next to a market
        // the operator had just paid for and describing it as unreachable when it may be fine.
        Err(error) if allow_unverified_model && registry_was_unreachable(&error) => {
            tracing::warn!(
                command = %command,
                frame_model = %requested_model,
                error = %error,
                "the ModelRegistry could not be read, so whether this model name resolves is \
                 unknown; deploying anyway because --allow-unverified-model was set"
            );
            Ok(None)
        }
        Err(error) if allow_unverified_model => {
            tracing::warn!(
                command = %command,
                frame_model = %requested_model,
                error = %error,
                "model name does not resolve in the ModelRegistry; deploying anyway because \
                 --allow-unverified-model was set. No buyer whose content-identity check is on can \
                 resolve this market"
            );
            Ok(None)
        }
        // The registry could not be ASKED. The name is not accused, and the opt-out is not advised
        // as a fix: it would proceed without the check rather than pass it.
        Err(error) if registry_was_unreachable(&error) => Err(error.context(format!(
            "{command} refuses before deploying anything: the ModelRegistry could not be read, so \
             whether `{requested_model}` is a name a buyer can resolve is UNKNOWN -- this is not a \
             statement about the name. Nothing was sent. Retry when the registry is reachable; \
             --allow-unverified-model proceeds without the check, and then {paid_for} if the name \
             turns out to be unresolvable"
        ))),
        Err(error) => Err(error.context(format!(
            "{command} refuses before deploying anything for `{requested_model}`: this is the \
             resolution every buyer performs, so a market listed under this name could not be \
             bought and {paid_for}. Fix the --frame-model name, or pass --allow-unverified-model \
             to {command} it anyway"
        ))),
    }
}

/// Provision a per-deal market: the seller note brings up the
/// `InferenceOrderBook` (`deployInferenceOrderBook`), asks `SuperRoot` for the `RootModel`
/// (`deployRootModel` -- SuperRoot deploys it and carries its own value), and pre-funds + deploys the
/// per-deal `TokenContract` from its own ECC[2] (`fundDeployShell` -> external seller-signed deploy),
/// **no operator multisig in the operate path**. Emits a `MarketManifest` whose `token_contract` is
/// the deployed, active address.
pub(crate) async fn run_provision(args: ProvisionArgs) -> Result<()> {
    run_provision_with_deal_gas_overhead(args, None).await
}

pub(crate) async fn run_provision_with_deal_gas_overhead(
    args: ProvisionArgs,
    supplied_deal_gas_overhead_raw: Option<u128>,
) -> Result<()> {
    use dexdo_core::{KeyPair, RealChainBackend};
    // A3: reject an invalid limit price at the command boundary, before any key/file/network read or write.
    validate_price_step(args.price_per_tick)?;
    policy::load_seller_runtime_policy(args.policy.as_deref())?;
    // The manifest path comes from the environment now. The flag it used to come from is
    // gone, and with it the case where an operator typed something unprintable -- what is left is a
    // path this process was handed, which still has to be text before it can be passed on as one.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let manifest = manifest_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds a path that is not printable text: {}",
            dexdo_core::params::MANIFEST_PATH_VAR,
            manifest_path.display()
        )
    })?;
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Seller, &args.registry, &manifest_path)?;
    // the model name is judged by the REGISTRY and by nothing else. There is no second,
    // client-side grammar a name must also satisfy -- `ensure_model_resolves` below asks the buyer's
    // own question against the on-chain catalog, and that answer is the whole rule.

    // What was here: `validate_canonical_model_id`, the `producer--model--version` shape check,
    // applied whenever role-scoped registry validation was switched off. It never proved membership
    // and the 4.0.36 catalog does not share that grammar -- it seeds
    // `Qwen/Qwen3-32B` as `Qwen3-32B`. So a name the registry HAS was refused before the registry
    // was consulted, and the operator was told to rewrite a name that was already correct.

    // `require_model_name` is not that grammar returning: it refuses a name that names nothing at
    // all, which no registry can answer for and which `sha256` would happily turn into a book.
    let requested_model = require_model_name(
        &args.frame_model,
        "--frame-model",
        "Pass a name `dexdo markets` lists.",
    )?;
    // Which note this deal is funded from, asked AFTER every refusal the arguments have already
    // earned -- the price and policy checks above, an unreadable manifest path, and a model name
    // that names nothing.

    // moved it here. It used to be the first thing the command did, so `provision` with an
    // empty `--frame-model` built the menu, read a balance per note (up to 5s each, half a minute
    // on a nine-note pool), took the operator's answer and only then refused for the name.
    let note_addr = match args.identity.note_addr.clone() {
        Some(address) => address,
        None => crate::cli::note_pick::ask_which_note(&manifest_path, None).await?,
    };
    // Prime the buyer-equivalent content registry now; its existing strict/opt-out decision below
    // still owns how a cached read failure is reported.
    let _ = preload_default_model_registry_with_endpoint(&manifest_path, None).await;
    preload_model_registry_policy_with_endpoint(
        RegistryRole::Seller,
        registry_policy.as_ref(),
        &manifest_path,
        None,
    )
    .await?;
    crate::cli::commands::chain_doctor_preflight_with_endpoint(&manifest_path, None, None).await?;
    // ASK THE BUYER'S QUESTION, HERE, BEFORE ANYTHING IS SPENT.

    // Everything below this line costs money -- the order book, the RootModel and the per-deal
    // TokenContract are deployed and paid for out of the note. The buyer resolves the model name
    // against the ModelRegistry unconditionally and refuses when it does not resolve, so a name that
    // fails that resolution produces a complete market no buyer can ever reach: the operator finds
    // out only if someone happens to tell them, and the spend repeats per nonce.

    // The seller's own registry check exists, but it lives behind `--model-registry-validation` and
    // is off by default -- the guard was reachable and, on the default path, nothing called it. This
    // asks the SAME question the buyer asks, using the same resolver, and asks it first.
    ensure_model_resolves(
        ModelResolutionCaller::Provision,
        &manifest_path,
        None,
        &requested_model,
        args.allow_unverified_model,
    )
    .await?;
    let target = resolve_model_registry_target_with_endpoint(
        RegistryRole::Seller,
        registry_policy.as_ref(),
        &manifest_path,
        None,
        &requested_model,
        BookTarget {
            frame_model: requested_model.clone(),
            model_hash: dexdo_core::model_hash_for(&requested_model),
            order_book: None,
            root_model: None,
            note_addr: Some(note_addr.clone()),
        },
    )
    .await?;
    let frame_model = target.frame_model;
    // The key is READ here and not where the note was settled above: a missing key file must not
    // preempt the manifest check that used to come first. Settling which note to spend from is a
    // question; reading its secret is a file, and the order of the refusals is a contract of its
    // own -- `provision_supported_policy_passes_policy_preflight_and_reaches_contracts_trap`
    // pins it.
    let seed = crate::cli::support::note_owner_secret_for(
        args.identity.note_key.as_deref(),
        &note_addr,
        None,
        "a real chain provisioning",
        "note owner key",
    )?
    .to_string();
    let chain = RealChainBackend::connect_with_endpoint(manifest, None)?;
    let deal_gas_overhead_raw = dexdo_core::params::resolve_deal_gas_overhead_raw(
        chain.network(),
        supplied_deal_gas_overhead_raw,
    )
    .map_err(anyhow::Error::msg)?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
    let note = dexdo_core::address::parse_chain_address(&note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    if let Some(policy) = registry_policy.as_ref() {
        let expected_order_book = chain
            .inference_orderbook_address(&note, &target.model_hash, dexdo_core::TICK_SIZE)
            .await?
            .with_workchain();
        let order_book_active = order_book_active(&chain, &expected_order_book).await?;
        enforce_model_registry_policy_with_endpoint(
            RegistryRole::Seller,
            policy,
            &manifest_path,
            None,
            crate::cli::commands::RegistryBookExpectation {
                frame_model: &frame_model,
                expected_order_book: &expected_order_book,
                order_book_active,
                buyer_missing_book_policy: BuyerMissingBookPolicy::Reject,
            },
        )
        .await?;
    }
    // A numeric nonce remains deterministic. `auto` draws from the OS CSPRNG and rejects a candidate
    // read-only when its deterministic address is active or has immutable history. The provisioning
    // path checks again before deployDeal, so this selection cannot weaken the money guard.
    let requested_nonce = require_provision_nonce(args.nonce)?;
    let mut rng = rand::rngs::OsRng;
    let nonce = choose_provision_nonce(
        requested_nonce,
        || rng.next_u64(),
        |candidate| chain.token_contract_nonce_is_available(&keys, candidate),
    )
    .await?;
    if requested_nonce == ProvisionNonceArg::Auto {
        eprintln!("selected random deal nonce: {nonce}");
    }
    // the note deposit is a user-chosen provision parameter (default >=100 SHELL), framed by deal volume --
    // NOT a MIN_BALANCE-anchored per-op gas knob. 1 SHELL = 1e9 raw ECC[2]. The deposit is split across the
    // RootModel + per-deal `TokenContract` deploys, funded from the note's own ECC[2].
    // the deposit follows THIS deal. `max_ticks` is what sets it -- the deal's `TokenContract`
    // pays its own compute and `MAX_CLAIM_DELTA = TICK_SIZE` caps a claim at one tick, so a deal of
    // `max_ticks` ticks takes `max_ticks` claims and its lifetime gas is linear in them.
    let default_deposit_shells = if supplied_deal_gas_overhead_raw.is_none() {
        default_deposit_shells(args.max_ticks)
    } else {
        dexdo_core::params::min_deploy_shells_with_overhead(args.max_ticks, deal_gas_overhead_raw)
    };
    let deposit_shells = match args.deposit_shells {
        Some(n) => n,
        None => prompt_deposit_shells()?.unwrap_or(default_deposit_shells),
    };
    // Fail-closed: overflow and a below-floor deposit are explicit errors, not a silent clamp/warn.
    let per_deploy = if supplied_deal_gas_overhead_raw.is_none() {
        deposit_per_deploy(deposit_shells, args.max_ticks)?
    } else {
        deposit_per_deploy_with_overhead(
            deposit_shells,
            args.max_ticks,
            Some(deal_gas_overhead_raw),
        )?
    };
    eprintln!(
        "note deposit: {deposit_shells} SHELL; ~{} SHELL for the per-deal TokenContract \
         deploy after fundDeployShell (the RootModel is deployed by SuperRoot and needs no note funding). This \
         {}-tick deal needs {} raw nanovmshell over its whole life, so its floor is {} SHELL. Unused deploy \
         remainder burns at destroy.",
        per_deploy / SHELL_UNIT,
        args.max_ticks,
        dexdo_core::params::deal_gas_requirement_raw_with_overhead(
            args.max_ticks,
            deal_gas_overhead_raw,
        ),
        dexdo_core::params::min_deploy_shells_with_overhead(
            args.max_ticks,
            deal_gas_overhead_raw,
        ),
    );
    // Run the stale/orphaned-note check BEFORE reading ECC balance. After a redeploy, old notes may be
    // absent/inactive/stale-code; reporting that as "0 SHELL" would mask the actionable re-mint reason.
    chain.assert_seller_note_current(&note).await?;
    // Fail-LOUD if the note's ECC[2] SHELL cannot cover the exact deploy deposit. Do not add guessed runtime
    // headroom here: section 6 requires any gas/SHELL threshold beyond the deploy amount to come from
    // contract constants/receipts, not a drifting reserve.
    let note_ecc = chain
        .client()
        .get_account(&note)
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "seller note {} disappeared after current-note preflight",
                dexdo_core::address::display(&note.with_workchain())
            )
        })?
        .ecc_balance(2);
    let note_spendable = chain.private_note_shell_balance(&note).await?;
    ensure_provision_deposit_covered(
        note_ecc,
        note_spendable,
        deposit_shells,
        args.price_per_tick,
    )?;
    let m = if supplied_deal_gas_overhead_raw.is_none() {
        chain
            .provision_market(
                &keys,
                &note,
                &frame_model,
                nonce,
                args.price_per_tick,
                args.max_ticks,
                per_deploy,
            )
            .await?
    } else {
        chain
            .provision_market_with_deal_gas_overhead(
                &keys,
                &note,
                &frame_model,
                nonce,
                args.price_per_tick,
                args.max_ticks,
                per_deploy,
                deal_gas_overhead_raw,
            )
            .await?
    };
    let json = m.to_json()?;
    std::fs::write(&args.output, &json)
        .map_err(|e| anyhow::anyhow!("write --output {}: {e}", args.output.display()))?;
    println!("provisioned market -> {}", args.output.display());
    println!("{json}");
    Ok(())
}

/// the seller lists only a confirmed name, and only flags the grammar can spell. Its own
/// file so nothing here is edited to make it pass.
#[cfg(test)]
#[path = "admin_1855_registry_gate_tests.rs"]
mod admin_1855_registry_gate_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{IdentityArgs, ModelRegistryValidationArgs};

    #[tokio::test]
    async fn auto_nonce_retries_collisions_while_explicit_nonce_is_unchanged() {
        let mut candidates = [7, 8, 9].into_iter();
        let auto = choose_provision_nonce(
            ProvisionNonceArg::Auto,
            || candidates.next().expect("scripted random candidate"),
            |candidate| async move { Ok(candidate == 9) },
        )
        .await
        .unwrap();
        assert_eq!(auto, 9);

        let explicit = choose_provision_nonce(
            ProvisionNonceArg::Explicit(42),
            || panic!("explicit nonce must not use randomness"),
            |_| async { panic!("explicit nonce must not probe random candidates") },
        )
        .await
        .unwrap();
        assert_eq!(explicit, 42);

        let error = choose_provision_nonce(
            ProvisionNonceArg::Auto,
            || 10,
            |_| async { anyhow::bail!("history lookup failed") },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("history lookup failed"));
    }

    /// - DISPATCH, not the guard.

    /// The resolver was never the problem: `resolve_registered_model_identity` has always been able
    /// to say that `qwen--qwen3.6--27b-e2check` resolves to nothing. The problem was that on the
    /// default seller path nothing asked it -- `resolve_model_registry_target` returns its target
    /// untouched when no `--model-registry-validation` config is given, so the seller reached
    /// `provision_market` having asked no question at all.

    /// So this reads the call order out of `run_provision` itself. A test that only exercised the
    /// decision function would have passed just as happily while the production path skipped it,
    /// which is precisely the failure being fixed.
    #[test]
    fn provision_asks_the_registry_before_it_spends_anything() {
        let source = include_str!("admin.rs");
        let body = source
            .split_once("fn run_provision_with_deal_gas_overhead")
            .expect("provision implementation present")
            .1
            .split_once("#[cfg(test)]")
            .expect("provision implementation ends before its tests")
            .0;
        let gate = body
            .find("ensure_model_resolves(")
            .expect("provision must resolve the model name");
        for spend in [
            // the money: every deploy the provision pays for, and the note reads that precede them
            "provision_market(",
            "deposit_per_deploy(",
            "ensure_provision_deposit_covered(",
        ] {
            let at = body
                .find(spend)
                .unwrap_or_else(|| panic!("run_provision still calls {spend}"));
            assert!(
                gate < at,
                "`{spend}` runs at {at} but the registry resolution only at {gate};  is that a \
                 market is paid for before anyone asks whether a buyer could resolve its name"
            );
        }
    }

    #[test]
    fn provision_passes_explicit_endpoint_to_every_registry_read() {
        let source = include_str!("admin.rs");
        let body = source
            .split_once("fn run_provision_with_deal_gas_overhead")
            .expect("provision implementation present")
            .1
            .split_once("#[cfg(test)]")
            .expect("provision implementation ends before its tests")
            .0;

        for call in [
            "preload_default_model_registry_with_endpoint(",
            "preload_model_registry_policy_with_endpoint(",
            "ensure_model_resolves(",
            "resolve_model_registry_target_with_endpoint(",
            "enforce_model_registry_policy_with_endpoint(",
        ] {
            let call = body
                .split_once(call)
                .unwrap_or_else(|| panic!("provision must call {call}"))
                .1;
            let arguments = &call[..call.len().min(400)];
            assert!(
                arguments.contains("None"),
                "{call} must receive the explicit provision endpoint"
            );
        }
    }

    /// THE DEFECT: a name that RESOLVES but is spelled differently is deployed under the
    /// operator's bytes.

    /// `qwen--qwen3.8--27b--fp8` resolves: the candidate walk reaches `Qwen3.8-27B`, which the
    /// catalogue carries. The resolver hands that spelling back and it was thrown away, so the book
    /// was deployed at `sha256("qwen--qwen3.8--27b--fp8")` -- a market nobody taking the name from
    /// the registry can ever reach. Measured on mainnet, 31 August 2026:

    /// ```text
    /// model_hash("qwen--qwen3.8--27b--fp8") = 0x5667f009...
    /// model_hash("Qwen3.8-27B") = 0xff1123cb...
    /// ```


    /// correct spelling. It does not substitute -- on a money path, replacing what the operator
    /// typed reads as the client being wrong rather than as help.
    #[test]
    fn a_name_the_registry_spells_differently_is_refused_and_the_spelling_is_named() {
        let error = super::model_resolution_result(
            super::ModelResolutionCaller::Provision,
            "qwen--qwen3.8--27b--fp8",
            false,
            Ok("Qwen3.8-27B".to_string()),
        )
        .expect_err("a name the registry spells differently must be refused, not deployed raw");
        let message = format!("{error:#}");
        assert!(
            message.contains("Qwen3.8-27B"),
            "the refusal must name the spelling to write instead: {message}"
        );
        assert!(
            message.contains("qwen--qwen3.8--27b--fp8"),
            "and the spelling that was given, or the operator cannot tell what to change: {message}"
        );
        assert!(
            message.contains("refuses before deploying anything"),
            "the refusal must say it happened before the spend: {message}"
        );
    }

    /// The plain mismatch -- no flags -- is refused and told what to write.

    /// Split from the test above after review: every seller input there carries capability flags,
    /// so `split("--").skip(3)` is non-empty and BOTH of them exercise the flagged arm. The arm that
    /// produces "Write `X`" was reachable by no test at all -- deleting its whole message left the
    /// suite green. This drives it with a two-part name, where the tail is empty by construction.
    #[test]
    fn a_plain_mismatch_is_refused_and_told_the_spelling_to_write() {
        let error = super::model_resolution_result(
            super::ModelResolutionCaller::Provision,
            "qwen3-32b",
            false,
            Ok("Qwen3-32B".to_string()),
        )
        .expect_err("a case-only difference is still a different name to the chain");
        let message = format!("{error:#}");
        assert!(
            message.contains("Write `Qwen3-32B`"),
            "the plain refusal must hand over the spelling to write: {message}"
        );
        assert!(
            message.contains("qwen3-32b"),
            "and the one that was given: {message}"
        );
    }

    /// A flagged name is not told to write a name that drops its flags.

    /// The refusal's advice is "write `{registry_model}`", and for a name carrying capability flags
    /// that advice names a DIFFERENT market: `qwen--qwen3--32b--w8k--tools` resolves to `Qwen3-32B`
    /// because `model_id_alias` drops the flags on the way to the candidate list, so writing
    /// `Qwen3-32B` lists an unflagged book and silently loses `w8k` and `tools`.

    /// This is not the flags question itself -- whether a flagged market can exist at all under the
    /// registry rule is the owner's, and `Qwen3-32B--w8k--tools` does not resolve either, because
    /// `parse_canonical_model_id` reads it as producer `Qwen3-32B`, model `w8k`, version `tools`.
    /// What is decidable here is narrower and true under every answer to it: an operator must not be
    /// handed a name that means something else as though it were their name spelled correctly.
    #[test]
    fn a_flagged_name_is_not_told_to_write_one_without_its_flags() {
        let error = super::model_resolution_result(
            super::ModelResolutionCaller::Provision,
            "qwen--qwen3--32b--w8k--tools",
            false,
            Ok("Qwen3-32B".to_string()),
        )
        .expect_err("a flagged name whose base resolves elsewhere is still a refusal");
        let message = format!("{error:#}");
        assert!(
            message.contains("w8k") && message.contains("tools"),
            "the refusal must name the flags the suggestion would drop: {message}"
        );
        assert!(
            !message.contains("Write `Qwen3-32B`"),
            "and must not hand a name that lists a different, unflagged market as if it were the \
             same one spelled right: {message}"
        );
    }

    /// And the flag does NOT cover it.

    /// `--allow-unverified-model` means "the registry does not confirm this model". A name the
    /// registry confirmed under another spelling is a CONFIRMED model and an operator's typo, so
    /// letting the flag through would make it the way to leave a wrong spelling on chain -- which is
    /// the outcome it was never introduced for.
    #[test]
    fn the_unverified_opt_out_does_not_cover_a_misspelling() {
        let error = super::model_resolution_result(
            super::ModelResolutionCaller::DeployMarket,
            "qwen--qwen3.8--27b--fp8",
            true,
            Ok("Qwen3.8-27B".to_string()),
        )
        .expect_err("the opt-out is about an unconfirmed model, not about a misspelled one");
        assert!(format!("{error:#}").contains("Qwen3.8-27B"), "{error:#}");
    }

    /// The exact registered spelling passes through untouched, and is what comes back.

    /// Without this the fix above is satisfied by refusing everything, and the returned value is
    /// what the caller keys the book by.
    #[test]
    fn the_registered_spelling_passes_and_is_returned() {
        let resolved = super::model_resolution_result(
            super::ModelResolutionCaller::Provision,
            "Qwen3.8-27B",
            false,
            Ok("Qwen3.8-27B".to_string()),
        )
        .expect("the registry's own spelling is the one name that must never be refused");
        assert_eq!(resolved.as_deref(), Some("Qwen3.8-27B"));
    }

    /// The refusal must be the buyer's answer, not a second opinion -- same resolver, so the operator
    /// reads the same candidate list at provision time that a buyer would read later.
    #[test]
    fn provision_refuses_an_unresolvable_name_and_names_the_cost() {
        let error = super::model_resolution_result(
            super::ModelResolutionCaller::Provision,
            "qwen--qwen3.6--27b-e2check",
            false,
            Err(dexdo::registry::RegistryAnswered::error(
                "buyer content identity registry check failed: claimed model \
                 qwen--qwen3.6--27b-e2check does not resolve to a registered ModelRegistry \
                 0:0d0d identity; tried [\"qwen--qwen3.6--27b-e2check\"]",
            )),
        )
        .expect_err("an unresolvable name must be refused, not warned about");
        let message = format!("{error:#}");
        assert!(message.contains("does not resolve"), "{message}");
        assert!(
            message.contains("refuses before deploying anything"),
            "the refusal must say it happened before the spend: {message}"
        );
        assert!(
            message.contains("--allow-unverified-model"),
            "an operator who means it needs to be told the opt-out: {message}"
        );
    }

    /// and the refusal names the command that was actually run.

    /// `deploy-market` shares this resolver now. With the command hard-coded, an operator who typed
    /// `dexdo deploy-market` was told that `provision` had refused and was advised to pass a flag
    /// "to provision it anyway" -- a refusal describing a command they did not run, which is the
    /// class of defect is about. It must also not overstate the cost: `deploy-market` pays
    /// for one order book, not for a RootModel and a TokenContract as well.
    #[test]
    fn the_refusal_names_the_command_that_ran_and_only_what_it_pays_for() {
        let refusal = |caller: super::ModelResolutionCaller| {
            format!(
                "{:#}",
                super::model_resolution_result(
                    caller,
                    "qwen--qwen3.6--27b-e2check",
                    false,
                    // A membership MISS, not a read failure. Carried by the TYPE the resolver
                    // attaches to a verdict, because that is what the two arms are told apart by.
                    Err(dexdo::registry::RegistryAnswered::error(
                        "does not resolve to a registered identity"
                    )),
                )
                .expect_err("an unresolvable name must be refused")
            )
        };

        let deploy_market = refusal(super::ModelResolutionCaller::DeployMarket);
        assert!(
            deploy_market.contains("deploy-market refuses")
                && !deploy_market.contains("provision"),
            "the operator ran deploy-market and must not be told provision refused: {deploy_market}"
        );
        assert!(
            !deploy_market.contains("RootModel"),
            "deploy-market deploys one order book; naming the provision spend overstates it: \
             {deploy_market}"
        );

        let provision = refusal(super::ModelResolutionCaller::Provision);
        assert!(provision.contains("provision refuses"), "{provision}");
        assert!(
            provision.contains("RootModel") && provision.contains("TokenContract"),
            "provision does pay for all three, and its refusal still says so: {provision}"
        );
    }

    /// a registry that could not be READ is not a verdict on the name.

    /// Removing the shape check made this resolution the only name gate on the default path, so a
    /// transient read failure now blocks the command outright -- and it was reported in the same
    /// words as a real membership miss: "Fix the --frame-model name, or pass
    /// --allow-unverified-model". For a rate-limited endpoint (the chain answers 403 above three
    /// requests a second) or a manifest whose ModelRegistry account is absent, that advises editing
    /// a correct name, or disabling a check that never ran.
    #[test]
    fn an_unreadable_registry_is_not_reported_as_a_bad_model_name() {
        let unreachable = |raw: &str| {
            format!(
                "{:#}",
                super::model_resolution_result(
                    super::ModelResolutionCaller::DeployMarket,
                    "Qwen3-32B",
                    false,
                    Err(anyhow::anyhow!("{raw}")),
                )
                .expect_err("an unreadable registry still stops the command")
            )
        };

        for raw in [
            "no ModelRegistry at 0:0d0d on network net-a: the account was not found",
            "request failed: 403 Forbidden",
            "read timed out after 30s",
        ] {
            let message = unreachable(raw);
            assert!(
                message.contains("could not be read") && message.contains("UNKNOWN"),
                "a read failure must say the registry could not be asked: {message}"
            );
            assert!(
                !message.contains("Fix the --frame-model name"),
                "the name is correct and must not be blamed for an unread registry: {message}"
            );
        }

        // And the other side of the same rule: a real miss still accuses the name, or this would
        // have turned every refusal into "try again later".

        // Built with `RegistryAnswered::error`, which is what the resolver attaches to a verdict.
        // An earlier draft used a bare `anyhow!` here and the miss was classified as an unread
        // registry -- the test fabricated the very distinction it was checking, and passed the
        // wrong way round until the marker was made constructible.
        let miss = format!(
            "{:#}",
            super::model_resolution_result(
                super::ModelResolutionCaller::DeployMarket,
                "qwen--qwen3.6--27b-e2check",
                false,
                Err(dexdo::registry::RegistryAnswered::error(
                    "claimed model does not resolve to a registered identity; tried [...]"
                )),
            )
            .expect_err("an unresolvable name must be refused")
        );
        assert!(
            miss.contains("Fix the --frame-model name"),
            "a membership miss is the name's fault and must say so: {miss}"
        );
    }

    /// A resolvable name is unaffected, and the opt-out still works -- a gate that refused everything,
    /// or that could not be opted out of, would trade one broken default for another.

    /// **The first half changed with, and the change is the owner's rule, not a weakening.**
    /// It used to pass `qwen--qwen3--32b` against a registry answering `Qwen/Qwen3-32B` and assert
    /// that provisioning proceeds. That is the case now refused: the two spell different bytes, and
    /// `model_hash = sha256(frame_model)` makes them different books, so proceeding is what put a
    /// market on mainnet that no buyer taking the name from the registry can reach. What the half
    /// asserts is unchanged -- a name the registry resolves is not refused -- and the input is now a
    /// name the registry resolves TO ITSELF, which is the only shape that was ever safe to deploy.
    #[test]
    fn a_resolvable_name_passes_and_the_opt_out_downgrades_to_a_warning() {
        let resolved = super::model_resolution_result(
            super::ModelResolutionCaller::Provision,
            "Qwen/Qwen3-32B",
            false,
            Ok("Qwen/Qwen3-32B".to_string()),
        )
        .expect("a resolvable name provisions");
        assert_eq!(resolved.as_deref(), Some("Qwen/Qwen3-32B"));

        let allowed = super::model_resolution_result(
            super::ModelResolutionCaller::Provision,
            "qwen--qwen3.6--27b-e2check",
            true,
            Err(dexdo::registry::RegistryAnswered::error("does not resolve")),
        )
        .expect("--allow-unverified-model provisions anyway");
        assert_eq!(allowed, None);
    }

    /// The resolved registry name is an ANSWER, not a rename -- and since that is true for a
    /// second reason, not the one written here first.

    /// It used to read "substituting it would move the derived `model_hash` and the order book with
    /// it -- the canonicalisation question own -- so `run_provision` must keep deploying
    /// under the name the operator gave". is gone and its substitution requirement is
    /// cancelled. What replaced it is stronger: the gate
    /// returns `Ok` ONLY when the two names are byte-equal, so there is no rename left to forbid.

    /// The assertion is kept because it still costs nothing and still says something true -- the
    /// resolution is a gate -- and because an assignment reappearing here would be the first sign of
    /// substitution creeping back in under a different justification.
    #[test]
    fn provision_keeps_the_operator_s_name_after_resolving_it() {
        let source = include_str!("admin.rs");
        let body = source
            .split_once("fn run_provision_with_deal_gas_overhead")
            .expect("provision implementation present")
            .1
            .split_once("#[cfg(test)]")
            .expect("provision implementation ends before its tests")
            .0;
        let gate = body
            .find("ensure_model_resolves(")
            .expect("resolution call present");
        let first_spend = body
            .find("deposit_per_deploy(")
            .expect("provision still computes its first spend");
        assert!(
            gate < first_spend,
            "the registry resolution must remain before the first spend"
        );
        let gate_line = body
            .lines()
            .find(|line| line.contains("ensure_model_resolves("))
            .expect("resolution call present");
        assert!(
            !gate_line.contains('='),
            "the resolution is a gate, not an assignment: binding its result here is how a rename \
             sneaks in -- `{gate_line}`"
        );
    }

    #[tokio::test]
    async fn provision_rejects_bad_price_before_identity_or_network_side_effects() {
        let args = ProvisionArgs {
            identity: IdentityArgs {
                note_key: None,
                note_index: 0,
                note_addr: None,
            },
            registry: ModelRegistryValidationArgs::default(),
            frame_model: "not-even-a-canonical-model".to_string(),
            allow_unverified_model: false,
            nonce: None,
            price_per_tick: dexdo_core::PRICE_STEP - 1,
            max_ticks: 1,
            deposit_shells: None,
            output: "must-not-be-written.json".into(),
            policy: None,
        };

        let error = run_provision(args)
            .await
            .expect_err("invalid price must fail at the command boundary");
        let message = error.to_string();
        // The refusal names the price in the unit the argument takes, and says what a price is.
        assert!(
            message.contains("whole number of SHELL a tick"),
            "{message}"
        );
        assert!(
            message.contains(&dexdo_core::shell_amount(dexdo_core::PRICE_STEP - 1)),
            "{message}"
        );
        assert!(!message.contains("--note-addr"), "{message}");
        assert!(!message.contains("missing-contracts-manifest"), "{message}");
    }
}

/// `dexdo deploy-market`: deploy the per-model `InferenceOrderBook` (the shared market for a model) if it is
/// not yet on-chain -- note-funded, the explicit "list this model" step a seller runs before posting
/// offers. The book address is deterministic from `model_hash`, so this is idempotent (already-deployed ->
/// no-op). Same lazy deploy the seller's `post_offer` does, surfaced as a first-class operate command.

/// The note is named by `--note-addr` and by nothing else, and there is no key flag at all: the
/// owner key is found by that address in the pool of the data directory this run was given. A note
/// this instance did not deploy is therefore out of reach here on purpose -- two ways of naming one
/// note is a question with no right answer, and the run that asked it signed one note's deploy with
/// another note's key.
pub(crate) async fn run_market_deploy(args: MarketDeployArgs) -> Result<()> {
    use dexdo_core::{model_hash_for, KeyPair, RealChainBackend, TICK_SIZE};
    // The manifest path comes from the environment now; the flag it used to come from is
    // gone. It still has to be text before it can be passed on as one.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let manifest = manifest_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds a path that is not printable text: {}",
            dexdo_core::params::MANIFEST_PATH_VAR,
            manifest_path.display()
        )
    })?;
    // Fail-closed on a stale binary / live-network skew BEFORE the on-chain deploy -- same gate `provision`/
    // `seller` run. Without it, deploy-market would silently deploy an order book on outdated contract code
    // against a re-deployed network (a live run caught exactly this: live PrivateNote ahead of the binary pin).
    let registry_policy =
        load_enabled_model_registry_policy(RegistryRole::Seller, &args.registry, &manifest_path)?;
    // as in `run_provision` above: the registry is the authority on a model name, and the
    // client keeps no grammar of its own to refuse one with. `--frame-model` is used as the exact
    // bytes it was given -- which is what `model_hash = sha256(frame_model)` has always meant.
    let requested_model = require_model_name(
        &args.frame_model,
        "--frame-model",
        "Pass a name `dexdo markets` lists.",
    )?;
    preload_model_registry_policy(
        RegistryRole::Seller,
        registry_policy.as_ref(),
        &manifest_path,
    )
    .await?;
    chain_doctor_preflight(&manifest_path, None).await?;
    // ASK THE BUYER'S QUESTION BEFORE THE BOOK IS PAID FOR -- the same reasoning, and the
    // same call, as put in front of `provision`.

    // Removing the `producer--model--version` shape check left this command with NO test of the
    // model name on its default path: role-scoped registry validation is off unless configured, so
    // `resolve_model_registry_target` returns its argument untouched and `enforce_model_registry_policy`
    // is skipped entirely. `dexdo deploy-market` would then deploy an order book at `sha256(<typo>)`
    // and pay for it out of the note, for a market no buyer could ever resolve.

    // The shape check was not a membership test and could not be one; this is. It is also the same
    // question the buyer asks, so a name that passes here is a name a buyer can reach.
    ensure_model_resolves(
        ModelResolutionCaller::DeployMarket,
        &manifest_path,
        None,
        &requested_model,
        args.allow_unverified_model,
    )
    .await?;
    // Which note funds this deploy, asked LAST -- after every refusal this run's own arguments have
    // already earned: an unreadable manifest path, a nameless model, a stale binary, a model name
    // no buyer could resolve. A question put in front of those costs the operator a menu, a balance
    // read per note (up to 5s each) and then the refusal anyway.

    // Offered from the pool on a terminal; anywhere else the refusal names `--note-addr`. This
    // command used to refuse outright instead, while the shared refusal copy (`crate::cli::refusal`,
    // keyed on "--note-addr" + "required") told the operator to "run the same command on a terminal
    // -- it offers the notes the pool records". That advice described `run_provision` and was
    // printed here, where no menu existed; following it produced the same refusal a second time,
    // and the operator went looking for another way to name the note -- `--note-key`, or a `--pool`
    // flag this command does not have, neither of which selects one.
    let note_addr = match args.note_addr.clone() {
        Some(address) => address,
        None => crate::cli::note_pick::ask_which_note(&manifest_path, None).await?,
    };
    let target = resolve_model_registry_target(
        RegistryRole::Seller,
        registry_policy.as_ref(),
        &manifest_path,
        &requested_model,
        BookTarget {
            frame_model: requested_model.clone(),
            model_hash: model_hash_for(&requested_model),
            order_book: None,
            root_model: None,
            note_addr: Some(note_addr.clone()),
        },
    )
    .await?;
    let frame_model = target.frame_model;
    let model_hash = target.model_hash;
    // Read after the manifest, for the reason given in `run_provision_with_deal_gas_overhead`.

    // `None` for the key, ALWAYS: this command takes no `--note-key`. The note is named by
    // its address and the pool of this run's data directory answers with the key recorded under
    // that address. A key flag here would be a second way to name a note, and the two can disagree.

    // Named `deploy-market`, not a backend description: that string is the first word of this refusal and
    // an operator reads it as the thing that refused. A backend description is not a command anybody ran,
    // and it is the same class as the `--note-addr` refusal this branch replaced.
    let seed = crate::cli::support::note_owner_secret_from_pool_only(
        &note_addr,
        "deploy-market",
        "the note's owner key",
    )?;
    let chain = RealChainBackend::connect(manifest)?;
    // this command has no `--note-key`, so its refusal must not name one. The key came from
    // the pool entry recorded under `--note-addr`, and that is what is malformed if this fails.
    let keys = KeyPair::from_secret_hex(seed.trim()).map_err(|e| {
        anyhow::anyhow!(
            "deploy-market: the owner key the pool records for note {} is not a valid SDK secret \
             hex: {e:?}",
            dexdo_core::address::display(&note_addr)
        )
    })?;
    let note = dexdo_core::address::parse_chain_address(&note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tick_size = TICK_SIZE;
    let ob = chain
        .inference_orderbook_address(&note, &model_hash, tick_size)
        .await?;
    let expected_order_book = ob.with_workchain();
    let book_active = chain.inference_orderbook_stats(&ob).await?.is_some();
    if let Some(policy) = registry_policy.as_ref() {
        enforce_model_registry_policy(
            RegistryRole::Seller,
            policy,
            &manifest_path,
            &frame_model,
            &expected_order_book,
            book_active,
            BuyerMissingBookPolicy::Reject,
        )
        .await?;
    }
    if book_active {
        println!(
            "inference market already deployed for {} -- order book {}",
            frame_model,
            dexdo_core::address::display(&ob.with_workchain())
        );
        return Ok(());
    }
    println!(
        "deploying inference market (order book) for {} ...",
        frame_model
    );
    chain
        .deploy_inference_orderbook(&note, &keys, &model_hash, &frame_model, tick_size)
        .await?;
    // Wait for activation so a follow-up `post_offer` doesn't race the deploy (the book getter returns once active).
    for _ in 0..MARKET_DEPLOY_ACTIVATION_MAX_READS {
        if chain.inference_orderbook_stats(&ob).await?.is_some() {
            break;
        }
        tokio::time::sleep(MARKET_DEPLOY_ACTIVATION_POLL_INTERVAL).await;
    }
    println!(
        "deployed inference market for {} -- order book {}",
        frame_model,
        dexdo_core::address::display(&ob.with_workchain())
    );
    Ok(())
}

/// `deploy-market` names the note the way its own refusal says it does.

/// What it reads is `include_str!` of this source, so it needs nothing from the build configuration beyond
/// `test`. It used to say it was deliberately outside a cargo feature; the features are gone
/// from this workspace, so there is no
/// tier to be outside of any more.

/// Measured before the fix: `dexdo deploy-market`, given a model and no note, run on a terminal
/// with a pool holding one funded note, printed

/// This run has no note to spend from, and nothing was sent.
/// Pass --note-addr, or run the same command on a terminal -- it offers the notes the pool
/// records, with what each one holds.

/// while running ON a terminal. The second sentence is `crate::cli::refusal`'s, keyed on any message
/// carrying "--note-addr" and "required", and it describes `run_provision`. `run_market_deploy`
/// refused before it ever looked at a pool.
#[cfg(test)]
mod deploy_market_note_identity_1784_tests {
    fn run_market_deploy_body() -> String {
        command_body("pub(crate) async fn run_market_deploy(args: MarketDeployArgs)")
    }

    /// The note comes from the pool offer, not from an outright refusal.
    #[test]
    fn deploy_market_offers_the_pool_before_it_refuses_for_a_note() {
        let body = run_market_deploy_body();
        assert!(
            body.contains("ask_which_note("),
            "deploy-market must offer the notes the pool records, which is what its own refusal \
             tells the operator it does; body was:\n{body}"
        );
        assert!(
            !body.contains("--note-addr (active inference note) is required"),
            "the outright refusal is back, and the refusal copy promising a menu is a lie again"
        );
    }

    /// The owner key is FOUND, not asked for: the note is named by its address and the pool of the
    /// data directory answers with the key. `--note-key` remains an escape for a note kept outside
    /// a pool; it must not become the way a note is selected.
    #[test]
    fn deploy_market_finds_the_owner_key_by_the_note_address() {
        let body = run_market_deploy_body();
        // The KEY-LESS lookup specifically. `note_owner_secret_for` takes a `--note-key` argument
        // and would let this command grow a second way to name its note; the function this looks
        // for has no key parameter at all, which is what makes that impossible rather than merely
        // unintended.
        let resolves = body
            .find("note_owner_secret_from_pool_only(")
            .expect("deploy-market must resolve the owner key through the key-less pool lookup");
        assert!(
            !body.contains("note_owner_secret_for("),
            "the key-taking lookup is back on a command that has no key flag"
        );
        let selects = body
            .find("ask_which_note(")
            .expect("deploy-market must settle the note first");
        assert!(
            selects < resolves,
            "the note has to be settled before its key is looked up: a key resolved first would \
             belong to a note nobody chose"
        );
    }

    /// And the question is asked AFTER the refusals the arguments already earned.

    /// `provision`, `seller` and `buyer` all place this question after their own argument checks,
    /// each with the same comment saying why. Asked first, an operator picks a note, waits out a
    /// balance read per note, and is then refused for an unreadable `--contracts` -- a question
    /// they should never have been shown.
    #[test]
    fn deploy_market_refuses_on_its_own_arguments_before_it_asks_for_a_note() {
        let body = run_market_deploy_body();
        let asks = body
            .find("ask_which_note(")
            .expect("deploy-market must offer the pool");
        for earlier in [
            // replaced `--contracts` with the manifest path from the environment; the check
            // this stands for is the same one -- the path has to be text before it is passed on.
            "holds a path that is not printable text",
            "load_enabled_model_registry_policy(",
            "require_model_name(",
            // The two that cost the operator a wait if they run after the menu: the stale-binary
            // gate and the buyer's own registry question.
            "chain_doctor_preflight(",
            "ensure_model_resolves(",
        ] {
            let at = body
                .find(earlier)
                .unwrap_or_else(|| panic!("run_market_deploy still checks {earlier}"));
            assert!(
                at < asks,
                "`{earlier}` is checked at {at}, after the note question at {asks}"
            );
        }
    }

    /// and `provision` follows the same rule, because the comment beside its question says
    /// it does.

    /// It did not. The question was the first thing the command ran, so `dexdo provision` with an
    /// empty `--frame-model` built the menu, read a balance per note -- up to 5s each, half a
    /// minute on a nine-note pool -- took the answer, and only then refused for the name. The two
    /// commands share one rule and now share a guard for it: a claim about a sibling that only the
    /// sibling's own test could catch is a claim nothing checks.
    #[test]
    fn provision_refuses_on_its_own_arguments_before_it_asks_for_a_note() {
        let body = command_body("pub(crate) async fn run_provision_with_deal_gas_overhead");
        let asks = body
            .find("ask_which_note(")
            .expect("provision must offer the pool");
        for earlier in [
            "validate_price_step(",
            "load_seller_runtime_policy(",
            // replaced `--contracts` with the manifest path from the environment; the check
            // this stands for is the same one -- the path has to be text before it is passed on.
            "holds a path that is not printable text",
            "require_model_name(",
        ] {
            let at = body
                .find(earlier)
                .unwrap_or_else(|| panic!("run_provision still checks {earlier}"));
            assert!(
                at < asks,
                "`{earlier}` is checked at {at}, after the note question at {asks}"
            );
        }
    }

    /// neither operate command carries a model-name grammar of its own.

    /// Both spend money -- `deploy-market` deploys an order book, `provision` deploys the book, the
    /// RootModel and a per-deal TokenContract -- and both used to refuse a name for its SHAPE before
    /// asking the on-chain catalog anything. The shape was `producer--model--version`; the 4.0.36
    /// catalog does not use it, so names the catalog holds could not be typed. The registry is the
    /// authority on a model name, and it is the only one.

    #[test]
    fn the_operate_commands_keep_no_model_name_grammar_of_their_own() {
        for command in [
            "pub(crate) async fn run_provision_with_deal_gas_overhead",
            "pub(crate) async fn run_market_deploy(args: MarketDeployArgs)",
        ] {
            let body = command_body(command);
            assert!(
                !body.contains("validate_canonical_model_id("),
                "`{command}` refuses a model name by its shape again; the catalog decides which \
                 names exist, and it does not spell them `producer--model--version`"
            );
        }
    }

    /// and the check that REPLACED the shape runs before anything is paid for.

    /// Removing a gate is only safe if what stands in its place is stricter, and here it has to be
    /// EARLIER as well: everything `deploy-market` does after this line is a deploy paid out of the
    /// note. This is the property, applied to the command did not cover.
    #[test]
    fn deploy_market_asks_the_registry_before_it_deploys_anything() {
        let body = command_body("pub(crate) async fn run_market_deploy(args: MarketDeployArgs)");
        let gate = body
            .find("ensure_model_resolves(")
            .expect("deploy-market must resolve the model name the way a buyer would");
        let spend = "deploy_inference_orderbook(";
        let at = body
            .find(spend)
            .unwrap_or_else(|| panic!("run_market_deploy still calls {spend}"));
        assert!(
            gate < at,
            "`{spend}` runs at {at} and the registry resolution only at {gate}: a book would be \
             paid for before anyone asked whether a buyer could resolve its name"
        );
    }

    /// One command's body: from its signature to the next top-level item.

    /// Bounded by the next line-initial `#[cfg(` rather than by the next non-chain stub. Those
    /// stubs are far apart -- the one after `run_provision_with_deal_gas_overhead` sits past the
    /// chain test module -- so the coarser bound swallowed 425 lines including that module, and
    /// any test added there naming a forbidden call would have been reported as the COMMAND making
    /// it, which is a false statement about its own subject.

    /// COMMENT LINES ARE DROPPED, and that is not tidiness. Every guard here asks whether a command
    /// CALLS something, and a call that is commented out is not a call -- but its text is still
    /// there. Measured: commenting out the whole `ensure_model_resolves(...)` block left
    /// `deploy_market_asks_the_registry_before_it_deploys_anything` green, so the gate that stands
    /// between a typo and a paid-for order book could be removed without the suite noticing. A
    /// guard with a known way to fool it is worse than none: it is trusted.

    /// The body of a command, bounded by its own brace and with comments removed.

    /// Both properties were hand-rolled here and both were wrong. The end marker was the next
    /// `#[cfg(` -- a neighbouring stub, deletable by unrelated work, and removing the cargo
    /// features deleted all of them. The comment filter read `//` lines only, so `/*... */`
    /// walked through it and a call wrapped in one still read as a call.
    fn command_body(command: &str) -> String {
        crate::cli::source_probe::code_of(include_str!("admin.rs"), command)
    }

    /// The guard on the guard: a commented-out call must not read as a call.

    /// Without this the source lints in this module are satisfied by text alone, which is how a
    /// removed registry gate stayed green in a measurement of them.
    #[test]
    fn a_commented_out_call_is_not_a_call() {
        let body = command_body("pub(crate) async fn run_market_deploy(args: MarketDeployArgs)");
        assert!(
            body.contains("ensure_model_resolves("),
            "the real call is code and must survive comment stripping: {body}"
        );
        // And the stripping actually removes something in this body, or the check above proves
        // nothing about it: every one of these functions is commented, densely.
        assert!(
            !body.contains("// "),
            "comment lines must be gone from the scanned body"
        );
    }
}

/// the seller CLOSES a STOPped deal's per-deal `TokenContract` via `TokenContract::destroy()`
/// (`onlyOwnerPubkey(_sellerPubkey)`, gated `!_opened && !_disputed && !_offerPosted`) -> `selfdestruct` to the
/// deal's own stored `_sellerNote` (`contracts/airegistry/TokenContract.sol:1844`). **The payee is not an
/// argument (4.0.33, Task O):** `--note-addr` names the operator's note for the messages below, it does not
/// choose where the deal pays.
/// **DESTRUCTIVE:** it selfdestructs the TC; the held leftover burns cross-dapp (the raw `selfdestruct` return is
/// not credited back to the cross-dapp note). At the right-sized ~10/deploy funding ( -- MIN_BALANCE gates
/// nothing) that leftover is ~a few vmshell (negligible), so the old fail-closed `--acknowledge-burn` for ~110 is
/// overkill -- it is optional now (kept for back-compat).
pub(crate) async fn run_destroy(args: DestroyArgs) -> Result<()> {
    // The manifest path comes from the environment now. The flag it used to
    // come from is gone, and with it the case where an operator typed something
    // unprintable -- what is left is a path this process was handed, which still has
    // to be text before it can be passed on as one.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let manifest = manifest_path.to_str().ok_or_else(|| {
        anyhow::anyhow!(
            "{} holds a path that is not printable text: {}",
            dexdo_core::params::MANIFEST_PATH_VAR,
            manifest_path.display()
        )
    })?;
    let chain = dexdo_core::RealChainBackend::connect(manifest)?;
    run_destroy_with_chain(args, &chain).await
}

/// The reads and the submit `destroy` needs, behind a seam so the command's own decisions are
/// exercised without a node. Mirrors `RecoverChain`/`ReclaimChain` in `cli::recover`.
#[async_trait::async_trait]
trait DestroyChain: Sync {
    /// The deal's coherent lifecycle snapshot. `None` is the *account* fact -- the `TokenContract` is not
    /// active, i.e. it already selfdestructed -- and never an empty field of a live contract.
    async fn state(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::DealChainState>>;
    async fn destroy(
        &self,
        tc: &dexdo_core::Address,
        note: &dexdo_core::Address,
        keys: &dexdo_core::KeyPair,
    ) -> Result<()>;
}

#[async_trait::async_trait]
impl DestroyChain for dexdo_core::RealChainBackend {
    async fn state(&self, tc: &dexdo_core::Address) -> Result<Option<dexdo_core::DealChainState>> {
        Ok(self
            .token_contract_deal_snapshot(tc)
            .await?
            .map(|snapshot| snapshot.state))
    }

    async fn destroy(
        &self,
        tc: &dexdo_core::Address,
        note: &dexdo_core::Address,
        keys: &dexdo_core::KeyPair,
    ) -> Result<()> {
        self.destroy_token_contract(tc, note, keys).await.map(drop)
    }
}

async fn run_destroy_with_chain(args: DestroyArgs, chain: &dyn DestroyChain) -> Result<()> {
    use dexdo_core::{Address, KeyPair};
    let _ = args.acknowledge_burn; // Accepted and ignored for existing-script compatibility; does not gate destroy.
    eprintln!(
        "dexdo destroy: selfdestructs the TokenContract; the held leftover (~a few vmshell at the right-sized \
         ~10/deploy funding, ) burns cross-dapp (not credited back to the note) -- negligible."
    );
    let note_addr = crate::cli::support::require_note_addr(
        &args.identity,
        "destroy",
        "the seller note this deal belongs to",
    )?;

    // The TC comes from --token-contract OR --market (single source of truth, fail-loud).
    let (tc_str, _frame, _nonce) =
        resolve_market_fields(args.market.as_deref(), args.token_contract.as_deref(), None)?;
    let seed = crate::cli::support::note_owner_secret_for(
        args.identity.note_key.as_deref(),
        &note_addr,
        None,
        "destroy",
        "the seller note's owner key",
    )?;
    let keys = KeyPair::from_secret_hex(seed.trim())
        .map_err(|e| anyhow::anyhow!("note owner key (SDK secret hex): {e:?}"))?;
    let note = dexdo_core::address::parse_chain_address(&note_addr)
        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
    let tc =
        Address::parse(&tc_str).map_err(|e| anyhow::anyhow!("token_contract {tc_str}: {e}"))?;
    let tc_display = dexdo_core::address::display_self_dapp(&tc.with_workchain());
    let note_display = dexdo_core::address::display(&note.with_workchain());
    // cleanup is idempotent. An inactive account IS the finished job, so a repeat run reports it and
    // exits 0 instead of submitting a `destroy` that only reaches `getSeller`-returned-nothing. The account
    // fact is read here -- `token_contract_seller_pubkey` collapses "TC gone" and "empty pubkey" into the
    // same `None`, so the seller-key gate downstream cannot tell an already-closed deal from a broken one.
    // Same shape as the seller branch of `dexdo close` (`cli::close`, `already_closed`).
    if chain.state(&tc).await?.is_none() {
        println!("destroy noop: TokenContract {tc_display} is inactive/closed");
        return Ok(());
    }
    eprintln!(
        "destroy {tc_display}: selfdestructs the TokenContract; under right-sized funding the remaining few vmshell \
         burn cross-dapp (not credited back to the note {note_display}). Seller-signed; requires the deal STOPped \
         (!_opened && !_disputed)."
    );
    chain.destroy(&tc, &note, &keys).await?;
    println!(
        "destroy submitted -> TokenContract {tc_display} selfdestructs; remaining cross-dapp gas is not credited to note {note_display}"
    );
    Ok(())
}

#[cfg(test)]
mod destroy_tests {
    use super::{run_destroy_with_chain, DestroyChain};
    use crate::cli::args::{DestroyArgs, IdentityArgs};
    use anyhow::Result;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TC: &str = "0:9999999999999999999999999999999999999999999999999999999999999999";
    const NOTE: &str = "0:1111111111111111111111111111111111111111111111111111111111111111";

    /// One deal, modelled the way the 4.0.33 contract behaves for the two facts `destroy` reads.

    /// `state = None` is a **destroyed account**: no code, so every getter returns nothing --
    /// `getState` and `getSeller` alike. The submit reproduces the exact production failure
    /// reported, by driving the same shared gate production drives: `token_contract_seller_pubkey`
    /// hands `check_seller_pubkey` a `None` it cannot distinguish from a live contract with an
    /// empty pubkey, and the operator gets a hard error for a job that is already done.
    struct FakeDeal {
        state: Option<dexdo_core::DealChainState>,
        submits: AtomicUsize,
    }

    impl FakeDeal {
        fn destroyed() -> Self {
            Self {
                state: None,
                submits: AtomicUsize::new(0),
            }
        }

        fn stopped() -> Self {
            Self {
                state: Some(dexdo_core::DealChainState {
                    funded: false,
                    opened: false,
                    probe_accepted: false,
                    disputed: false,
                    deposit: 0,
                    finalized_owed: 0,
                    tokens_final: 0,
                    tokens_pending: 0,
                    probe_tick: 0,
                    funded_time: None,
                    probe_time: 0,
                    last_claim_time: 0,
                    dispute_time: 0,
                }),
                submits: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl DestroyChain for FakeDeal {
        async fn state(
            &self,
            _tc: &dexdo_core::Address,
        ) -> Result<Option<dexdo_core::DealChainState>> {
            Ok(self.state)
        }

        async fn destroy(
            &self,
            _tc: &dexdo_core::Address,
            _note: &dexdo_core::Address,
            keys: &dexdo_core::KeyPair,
        ) -> Result<()> {
            self.submits.fetch_add(1, Ordering::SeqCst);
            // `RealChainBackend::destroy_token_contract` gates on the seller pubkey read BEFORE it
            // submits; on a destroyed account that read is `None`, and the shared gate -- the same
            // one production calls -- is the error the operator saw.
            let seller_key =
                dexdo_core::KeyPair::from_secret_hex(SELLER_SECRET).expect("modelled seller key");
            let seller = self
                .state
                .as_ref()
                .map(|_| seller_key.public_hex().to_string());
            dexdo_core::check_seller_pubkey("destroy", seller.as_deref(), keys.public_hex())
                .map_err(anyhow::Error::msg)?;
            Ok(())
        }
    }

    /// The seller's own key: the modelled `onlyOwnerPubkey(_sellerPubkey)` accepts it on a live
    /// deal, so the positive control really submits rather than failing the gate for another reason.
    const SELLER_SECRET: &str = "2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a";

    fn destroy_args(key_file: &std::path::Path) -> DestroyArgs {
        DestroyArgs {
            identity: IdentityArgs {
                note_key: Some(key_file.to_path_buf()),
                note_index: 0,
                note_addr: Some(NOTE.to_string()),
            },
            token_contract: Some(TC.to_string()),
            market: None,
            acknowledge_burn: false,
        }
    }

    fn seller_key_file(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let path = dir.path().join("note.secret.hex");
        crate::cli::support::write_owner_only_key_fixture(&path, SELLER_SECRET);
        path
    }

    /// cleanup must be idempotent. The operator re-runs `dexdo destroy` on a deal that already
    /// selfdestructed (the normal case after a settled purchase) and must be told the job is done --
    /// exit 0, no submit -- instead of `destroy: TokenContract exposes no seller pubkey`.
    #[tokio::test]
    async fn repeated_destroy_on_a_selfdestructed_deal_is_a_noop_not_a_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key_file = seller_key_file(&dir);
        let chain = FakeDeal::destroyed();

        run_destroy_with_chain(destroy_args(&key_file), &chain)
            .await
            .expect("an already destroyed TokenContract is a finished job, not an error");

        assert_eq!(
            chain.submits.load(Ordering::SeqCst),
            0,
            "a destroyed account must not be sent another destroy"
        );
    }

    /// The idempotency guard must not swallow the real work: a live STOPped deal still submits.
    #[tokio::test]
    async fn a_live_stopped_deal_still_submits_destroy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let key_file = seller_key_file(&dir);
        let chain = FakeDeal::stopped();

        run_destroy_with_chain(destroy_args(&key_file), &chain)
            .await
            .expect("a live STOPped deal is destroyable");

        assert_eq!(
            chain.submits.load(Ordering::SeqCst),
            1,
            "the live deal must still be destroyed exactly once"
        );
    }
}
