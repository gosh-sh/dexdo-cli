//! `dexdo` CLI helpers(backends, resolvers, deposit sizing, render), split out of `main.rs` (PR3,
//! move-only). Behavior-identical to the pre-split functions.

use crate::cli::args::*;
use anyhow::{bail, Result};
use dexdo_core::{
    deal_anomalies, per_model_breakdown, ChainBackend, DealAnomaly, DealRole, DobParams, LocalNote,
    MockChainBackend, ModelBreakdown, Note, NoteTree, ProtocolConsts, TreeSnapshot,
};
use std::path::PathBuf;
use std::sync::Arc;

/// Load the identity's **note tree** from `--note-key`. dexdo only **reads** the key,
/// never writes or rotates it. No path -> an ephemeral tree(degenerate to a single note) with
/// a warning(mock-demo). An invalid/inaccessible path is an explicit failure, not a silent `generate()`.
pub(crate) fn load_note_tree(note_key: Option<&std::path::Path>) -> Result<NoteTree> {
    match note_key {
        Some(path) => {
            let hex = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("read --note-key {}: {e}", path.display()))?;
            NoteTree::from_secret_hex(&hex)
                .map_err(|e| anyhow::anyhow!("parse --note-key {}: {e}", path.display()))
        }
        None => {
            tracing::warn!(
                "ephemeral note (no --note-key): identity will NOT persist between runs -- \
                 mock-demo only. Production path: set --note-key <path>."
            );
            Ok(NoteTree::new(LocalNote::generate()))
        }
    }
}

/// Load the specific identity(sub)note(tree + index from `--note-index`) that
/// `seller`/`buyer` operates on. Index outside the tree -> explicit failure.
pub(crate) fn load_note_identity(identity: &IdentityArgs) -> Result<LocalNote> {
    let tree = load_note_tree(identity.note_key.as_deref())?;
    tree.node(identity.note_index).ok_or_else(|| {
        anyhow::anyhow!(
            "--note-index {} outside the tree (an ephemeral note has only index 0)",
            identity.note_index
        )
    })
}

/// Chain backend + note, selected by `--mock-chain`/the `shellnet` feature. Behind the common
/// `ChainBackend`/`Note` trait the `seller`/`buyer` flow does not depend on the choice -- only construction changes.
pub(crate) type ChainAndNote = (Arc<dyn ChainBackend>, Arc<dyn Note>);

/// Mock backend + a loaded(or ephemeral) `LocalNote` -- the standard mock path.
pub(crate) fn mock_chain_and_note(
    endpoints_file: PathBuf,
    identity: &IdentityArgs,
) -> Result<ChainAndNote> {
    let chain: Arc<dyn ChainBackend> = Arc::new(MockChainBackend::new(
        endpoints_file,
        ProtocolConsts::canonical(),
        DobParams::canonical(),
    ));
    let note: Arc<dyn Note> = Arc::new(load_note_identity(identity)?);
    Ok((chain, note))
}

/// Read the key's hex secret from a file. The contents are **not logged**(secret).
#[cfg(feature = "shellnet")]
pub(crate) fn read_secret_hex(path: &std::path::Path, what: &str) -> Result<String> {
    let s = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read {what} {}: {e}", path.display()))?;
    let s = s.trim().to_string();
    if s.is_empty() {
        bail!("{what} {} is empty", path.display());
    }
    Ok(s)
}

/// Real seller backend + the seller's `RealNote`: from the `--note-key` seed + `--note-addr` (the
/// mint-specific address of the provisioned note) and `model_hash` from `--model`. Directive: the note
/// self-funds its seller side from its own ECC[2] -- no operator wallet. Provisioning is a separate script.
#[cfg(feature = "shellnet")]
pub(crate) fn seller_real_backend(
    args: &SellerArgs,
    market_frame_model: Option<&str>,
    market_nonce: Option<u64>,
) -> Result<ChainAndNote> {
    let name = args
        .model
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("real shellnet: set --model <name from config> (needed for model_hash)")
        })?;
    let frame_model = dexdo::seller::ModelsConfig::load(&args.models)?
        .get(name)?
        .frame_model
        .clone();
    // The offer's on-chain model name/hash MUST be canonical `producer--model--version`(indexer-parseable);
    // an OpenAI slug belongs in `served_model`. Fail loud before posting an un-indexable offer.
    dexdo_core::validate_canonical_model_id(&frame_model).map_err(|e| anyhow::anyhow!(e))?;
    check_market_model_match(market_frame_model, &frame_model, name)?;
    let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!("real shellnet: --note-addr (provisioned note address) is required")
    })?;
    let note_key =
        args.identity.note_key.as_deref().ok_or_else(|| {
            anyhow::anyhow!("real shellnet: --note-key (note root seed) is required")
        })?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    // Review: the deal nonce binds the offer to the canonical per-deal TokenContract. The IOB
    // rejects any offer whose `tokenContract` does not derive from `(sellerPubkey, nonce)`, so the
    // real seller MUST have it -- from `--market`(manifest) or the explicit `--nonce` flag.
    let nonce = market_nonce.ok_or_else(|| {
        anyhow::anyhow!(
            "real shellnet: pass --nonce <n> (or --market <manifest>) -- the deal nonce binds the \
             offer to the canonical TokenContract (IOB rejects a mismatched tokenContract)"
        )
    })?;
    let (backend, rn) = dexdo_core::RealSellerBackend::from_provisioned(
        manifest,
        &note_addr,
        &read_secret_hex(note_key, "--note-key")?,
        &frame_model,
        nonce,
        args.probe_shell,
    )?;
    let chain: Arc<dyn ChainBackend> = Arc::new(backend);
    let note: Arc<dyn Note> = Arc::new(rn);
    Ok((chain, note))
}

#[cfg(not(feature = "shellnet"))]
pub(crate) fn seller_real_backend(
    _args: &SellerArgs,
    _market_frame_model: Option<&str>,
    _market_nonce: Option<u64>,
) -> Result<ChainAndNote> {
    bail!(
        "real shellnet backend unavailable: build with `--features shellnet` or pass --mock-chain"
    )
}

/// Real buyer backend + the buyer's `RealNote`: from a provisioned note(`--note-key`/`--note-addr`)
/// and `model_hash` from `--frame-model`. The price limit is `--max-price-per-tick`(>= ask); the escrow must
/// cover `ticks x limit x(1 + 2.5 % book fee)` (issue -- otherwise the escrow is orphaned in the book;
/// `from_provisioned` checks the invariant ahead of time via `check_buy_deposit_headroom`).
#[cfg(feature = "shellnet")]
pub(crate) fn buyer_real_backend(args: &BuyerArgs, frame_model: &str) -> Result<ChainAndNote> {
    let note_addr = args.identity.note_addr.clone().ok_or_else(|| {
        anyhow::anyhow!("real shellnet: --note-addr (provisioned note address) is required")
    })?;
    let note_key = args.identity.note_key.as_deref().ok_or_else(|| {
        anyhow::anyhow!("real shellnet: --note-key (owner key of the provisioned note) is required")
    })?;
    let manifest = args
        .contracts
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("--contracts: non-printable path"))?;
    let max_price_per_tick = args.max_price_per_tick;
    let (backend, rn) = dexdo_core::RealBuyerBackend::from_provisioned(
        manifest,
        &note_addr,
        &read_secret_hex(note_key, "--note-key")?,
        frame_model,
        max_price_per_tick,
        args.ticks,
        // default to EXACTLY the required escrow(no over-funding); an explicit value is checked
        // == required by `check_buy_deposit_headroom` in `from_provisioned`.
        args.escrow
            .unwrap_or_else(|| dexdo_core::required_escrow_for_buy(args.ticks, max_price_per_tick)),
    )?;
    let chain: Arc<dyn ChainBackend> = Arc::new(backend);
    let note: Arc<dyn Note> = Arc::new(rn);
    Ok((chain, note))
}

#[cfg(not(feature = "shellnet"))]
pub(crate) fn buyer_real_backend(_args: &BuyerArgs, _frame_model: &str) -> Result<ChainAndNote> {
    bail!(
        "real shellnet backend unavailable: build with `--features shellnet` or pass --mock-chain"
    )
}

/// Default platform path for the endpoints file(D6): the application data directory
/// (Linux `~/.local/share/dexdo`, macOS `~/Library/Application Support/ai.gosh.dexdo`,
/// Windows `%APPDATA%\gosh\dexdo\data`). `PathBuf::join` yields the correct separator on each OS.
pub(crate) fn default_endpoints_path() -> Result<PathBuf> {
    let proj = directories::ProjectDirs::from("ai", "gosh", "dexdo").ok_or_else(|| {
        anyhow::anyhow!("could not determine the platform data directory; set --endpoints-file")
    })?;
    Ok(proj.data_dir().join("endpoints.json"))
}

/// Resolve the endpoints file path: an explicit `--endpoints-file` takes priority, otherwise the platform
/// default. The parent directory is created(the mock writes `*.chainstate.json` alongside it).
pub(crate) fn resolve_endpoints_file(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let path = match explicit {
        Some(p) => p,
        None => default_endpoints_path()?,
    };
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("create directory {}: {e}", parent.display()))?;
        }
    }
    Ok(path)
}

/// Issue: load + integrity-check a `dexdo provision` market manifest(`--market`). A corrupt or
/// hand-edited manifest(empty fields, `model_hash` not matching `frame_model`) is rejected, not silently
/// trusted by a real-money CLI.
pub(crate) fn load_market(path: &std::path::Path) -> Result<dexdo_core::MarketManifest> {
    let s = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read --market {}: {e}", path.display()))?;
    let m = dexdo_core::MarketManifest::from_json(&s)
        .map_err(|e| anyhow::anyhow!("parse --market {}: {e}", path.display()))?;
    m.validate()
        .map_err(|e| anyhow::anyhow!("--market {}: {e}", path.display()))?;
    Ok(m)
}

/// on `dexdo seller --market`, the seller note(`--note-addr`) MUST be the one the market was provisioned
/// for. The per-deal `TokenContract` is derived from `(sellerPubkey, nonce)`; posting an offer from a different
/// note/key than the manifest's `seller_note` makes the `InferenceOrderBook` reject the ask (canonical-TC
/// mismatch) -- it never rests, the seller never matches, and the buyer times out. Fail closed BEFORE posting.
/// Pure(offline-testable): compares the manifest `seller_note` to `--note-addr`, both wallet-normalized.
pub(crate) fn assert_market_seller_note(manifest_seller_note: &str, note_addr: &str) -> Result<()> {
    let norm =
        |s: &str| dexdo_core::normalize_wallet_address(s).unwrap_or_else(|_| s.trim().to_string());
    if norm(manifest_seller_note) != norm(note_addr) {
        bail!(
            "--market manifest seller_note {manifest_seller_note} != --note-addr {note_addr}: the seller note \
             must be the one the market was provisioned for. The per-deal TokenContract is derived from \
             (sellerPubkey, nonce), so an offer from a different note/key is rejected by the InferenceOrderBook \
             (canonical-TC mismatch) -- the ask never rests and the buyer never matches (). Use the \
             provisioned note, or re-provision a market for this note."
        );
    }
    Ok(())
}

#[cfg(test)]
mod seller_note_tests {
    use super::*;

    /// `dexdo seller --market` must fail closed if the manifest's `seller_note` isn't this seller's
    /// `--note-addr` -- a mismatched note posts a non-canonical TC the IOB won't rest, so the seller never
    /// matches and the buyer times out. The same note passes.
    #[test]
    fn market_seller_note_mismatch_fails_closed() {
        assert!(assert_market_seller_note("0:abc123", "0:abc123").is_ok());
        let err = assert_market_seller_note("0:aaaa", "0:bbbb")
            .unwrap_err()
            .to_string();
        assert!(err.contains(""), "{err}");
        assert!(err.contains("seller note"), "{err}");
    }
}

/// Resolve `(token_contract, frame_model, nonce)` for seller/buyer from `--market`(if set) or the
/// explicit flags: a produced provisioning record feeds the CLI without hand-editing.
/// `frame_model` is returned as `Option` -- the seller passes `None` (it validates the manifest model
/// against `--model`). `nonce` is the deal nonce from the manifest -- `Some` only on the
/// `--market` path; on the explicit `--token-contract` path it is `None` (the seller supplies it via
/// `--nonce`, the buyer ignores it).
/// **Fail-loud(real-money CLI):** `--market` is the single source of truth -- combining it with an
/// explicit `--token-contract`/`--frame-model` is rejected rather than silently taking one of them.
pub(crate) fn resolve_market_fields(
    market: Option<&std::path::Path>,
    token_contract: Option<&str>,
    frame_model: Option<&str>,
) -> Result<(String, Option<String>, Option<u64>)> {
    if let Some(p) = market {
        if token_contract.is_some() {
            bail!("--market and --token-contract are mutually exclusive -- pass only one");
        }
        if frame_model.is_some() {
            bail!("--market and --frame-model are mutually exclusive -- pass only one");
        }
        let m = load_market(p)?;
        Ok((m.token_contract, Some(m.frame_model), Some(m.nonce)))
    } else {
        let tc = token_contract
            .ok_or_else(|| anyhow::anyhow!("provide --token-contract or --market <manifest>"))?;
        Ok((tc.to_string(), frame_model.map(str::to_string), None))
    }
}

/// `dexdo provision` REQUIRES an explicit, deal-unique `--nonce`. The per-deal `TokenContract` derives
/// from `(sellerPubkey, nonce)`, so a reused/default nonce collides -- a second provisioned deal overwrites the
/// first deal's TC. The old `--nonce 0` default silently reused it; this fails loud and forces a distinct nonce
/// per deal. Pure.
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))] // used by run_provision(shellnet); test exercises it
pub(crate) fn require_provision_nonce(nonce: Option<u64>) -> Result<u64> {
    nonce.ok_or_else(|| {
        anyhow::anyhow!(
            "--nonce <n> is required and must be UNIQUE per deal: the per-deal TokenContract derives from \
             (sellerPubkey, nonce), so a reused/default nonce collides -- overwriting a prior deal's TC. Pass a \
             distinct --nonce for each provisioned deal (e.g. an incrementing counter)."
        )
    })
}

#[cfg(test)]
mod provision_nonce_tests {
    use super::require_provision_nonce;

    /// `provision` refuses an absent `--nonce`(the old unsafe `0` default -> collision across deals)
    /// and accepts an explicit deal-unique value.
    #[test]
    fn provision_nonce_required_and_explicit() {
        assert_eq!(require_provision_nonce(Some(7)).unwrap(), 7);
        let err = require_provision_nonce(None).unwrap_err().to_string();
        assert!(err.contains("UNIQUE per deal"), "{err}");
        assert!(err.contains("--nonce"), "{err}");
    }
}

/// Issue(review): the served `--model` must resolve to the model a `--market` manifest was
/// provisioned for, else the seller posts the manifest's `token_contract` into the wrong order book
/// while a buyer using the same manifest derives another model(fields drift). Fail closed on mismatch.
/// (Only the real-shellnet seller path calls it; kept non-gated so the offline regression exercises it.)
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn check_market_model_match(
    market_frame_model: Option<&str>,
    configured_frame_model: &str,
    model_name: &str,
) -> Result<()> {
    if let Some(mfm) = market_frame_model {
        if mfm != configured_frame_model {
            bail!(
                "--market manifest is for frame_model `{mfm}`, but --model `{model_name}` resolves to \
                 `{configured_frame_model}` -- refusing to serve the wrong model into the manifest's order book"
            );
        }
    }
    Ok(())
}

pub(crate) fn consumer_api_token_budget(ticks: u128) -> u64 {
    let tick_size = dexdo_core::DobParams::canonical().tick_size as u128;
    ticks.saturating_mul(tick_size).min(u64::MAX as u128) as u64
}

/// the one-shot `dexdo buyer` path(no `--local-listen`) opens the seller stream with NO canonical
/// request -- it is promptless by design(`connect_and_stream` sends `None`). A **real** seller upstream
/// cannot serve a prompt-less stream(`"real upstream requires a canonical request"`), and fabricating a
/// default prompt would run+bill a synthetic inference the buyer never asked for(money-safety). So one-shot
/// only drives a `--mock-model` seller; real-provider inference must go through `--local-listen` + the consumer
/// API, which supplies the prompt per request. Fail closed EARLY
/// (before the on-chain buy) with an actionable error instead of a deep gateway `InvalidArgument`.
pub(crate) fn oneshot_real_upstream_guard(
    local_listen_set: bool,
    mock_model: bool,
) -> Result<(), String> {
    if !local_listen_set && !mock_model {
        return Err(
            "real-provider inference requires `--local-listen <addr>` + a `/v1/chat/completions` request \
             (the consumer API supplies the prompt,/G); one-shot `dexdo buyer` (no prompt) only drives a \
             `--mock-model` seller. Add `--local-listen` and POST your prompt there, or pass `--mock-model` for \
             the mock path ()."
                .to_string(),
        );
    }
    Ok(())
}

/// 1 SHELL = 1e9 raw ECC[2] nano(the note-side unit; `--deposit-shells N` = N **SHELL**, not vmshell).
#[allow(dead_code)] // used by the shellnet `provision` path + the deposit-validation tests
pub(crate) const SHELL_UNIT: u128 = 1_000_000_000;
/// per-deploy **SHELL allocation** floor(note-side), sized to the deploy's **vmshell** gas need --
/// **derived from contract constants**. **fund-10(per @SeHor05): `MIN_BALANCE`
/// gates nothing** -- `ensureBalance()`'s `mintshellq` is a no-op for a self-dapp TC(no DappConfig), so the old
/// +100 `MIN_BALANCE` term was the note **over-funding** the deploy, not a floor. A self-dapp deploy only needs the
/// cross-dapp `REGISTER_FORWARD_VALUE`(5) + ~0.07 vmshell compute + a thin margin = **~10 SHELL/deploy**;
/// `fundDeployShell` converts SHELL->vmshell ~1:1(flag:16). `REGISTER_FORWARD_VALUE` is a
/// `contracts/dex/modifiers/modifiers.sol` constant. The held leftover burns at `destroy` (`selfdestruct(payout)`
/// to the cross-dapp note is not credited -- by-fact x2) but at ~10/deploy is now ~a few vmshell(negligible).
/// **NB(by-fact):** on the CURRENT contract a live deal's TC runtime cross-dapp sends(5 vmshell each) drain a
/// ~10-funded TC, so a live deal needs a higher `--deposit-shells` until @SeHor05's send-`value:`->0.01 cut.
#[allow(dead_code)]
pub(crate) const MIN_DEPLOY_SHELLS: u128 = 5 /* REGISTER_FORWARD_VALUE */ + 5 /* compute margin (~0.07 burn + headroom) */;
/// default note deposit(**SHELL/ECC[2]**, note-side) -- **fund-10, right-sized**, not padded.
/// `deposit/2` funds the RootModel + per-deal `TokenContract` deploys, so the default gives `20/2 = 10` SHELL each
/// (-> ~10 vmshell/deploy after flag:16) -- the `MIN_DEPLOY_SHELLS` floor. The held leftover burns at `destroy` but is
/// ~a few vmshell(negligible). **NB:** a live deal on the current contract needs a higher `--deposit-shells` (the
/// TC runtime sends drain ~10) until @SeHor05's send-`value:`->0.01 cut -- the AmicableSplit behaviour itself is
/// proven offline(`positive_path_amicable_split`).
#[allow(dead_code)]
pub(crate) const DEFAULT_DEPOSIT_SHELLS: u128 = 20;
/// Contract constants mirrored from `contracts/airegistry/modifiers/modifiers.sol`.
#[allow(dead_code)]
pub(crate) const SELLER_PROBE_COMMISSION_BPS: u128 = 250;
#[allow(dead_code)]
pub(crate) const BPS_DENOMINATOR: u128 = 10_000;

/// resolve the per-deploy ECC[2] funding(raw) from the user's note deposit(SHELL) -- **fail-closed** for a
/// value that controls live on-chain spending. Errors on `u128` overflow and on a **below-floor** deposit (a known
/// funded-uninit / fund-burn outcome on-chain), instead of silently clamping or proceeding into a live spend. For
/// this checkpoint the deposit is a **per-deploy allocation**(RootModel + one `TokenContract` = `deposit/2`), not
/// yet the full "N deals per note" budget model.
#[allow(dead_code)]
pub(crate) fn deposit_per_deploy(deposit_shells: u128) -> Result<u128> {
    let deposit_raw = deposit_shells.checked_mul(SHELL_UNIT).ok_or_else(|| {
        anyhow::anyhow!("--deposit-shells {deposit_shells}: overflows the u128 ECC[2] raw range")
    })?;
    let per_deploy = deposit_raw / 2; // RootModel + per-deal TokenContract
    if per_deploy < MIN_DEPLOY_SHELLS.saturating_mul(SHELL_UNIT) {
        anyhow::bail!(
            "--deposit-shells {deposit_shells} -> ~{} SHELL/deploy is below the {MIN_DEPLOY_SHELLS} SHELL/deploy \
             floor (each self-dapp deploy needs ~{MIN_DEPLOY_SHELLS} vmshell after flag:16 = REGISTER_FORWARD_VALUE \
             5 + ~0.07 compute + margin -- contract constants, not bisected; : MIN_BALANCE gates nothing, so the \
             old ~110 was the note over-funding). Below it the deploy under-funds (funded-uninit). \
             Raise --deposit-shells to >={} (default {DEFAULT_DEPOSIT_SHELLS}).",
            per_deploy / SHELL_UNIT,
            MIN_DEPLOY_SHELLS * 2
        );
    }
    Ok(per_deploy)
}

/// Seller probe commission from `TokenContract._probeCommission()`:
/// `pricePerTick * SELLER_PROBE_COMMISSION_BPS / BPS_DENOMINATOR`.
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn seller_probe_commission_for_price(price_per_tick: u128) -> Result<u128> {
    let product = price_per_tick
        .checked_mul(SELLER_PROBE_COMMISSION_BPS)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "price_per_tick {price_per_tick}: seller probe commission overflows u128"
            )
        })?;
    Ok(product / BPS_DENOMINATOR)
}

/// provision may fail early if the note cannot cover the exact deploy deposit plus the contract-derived
/// seller probe commission. This is not guessed runtime headroom: `TokenContract.open()` hard-requires
/// `fundProbeCommission()` first, and the amount is `SELLER_PROBE_COMMISSION_BPS` of `price_per_tick`.
#[cfg_attr(not(feature = "shellnet"), allow(dead_code))]
pub(crate) fn ensure_provision_deposit_covered(
    note_ecc_raw: u128,
    deposit_shells: u128,
    price_per_tick: u128,
) -> Result<()> {
    let deploy_need = deposit_shells.checked_mul(SHELL_UNIT).ok_or_else(|| {
        anyhow::anyhow!("--deposit-shells {deposit_shells}: overflows the u128 ECC[2] raw range")
    })?;
    let probe_need = seller_probe_commission_for_price(price_per_tick)?;
    let need = deploy_need.checked_add(probe_need).ok_or_else(|| {
        anyhow::anyhow!(
            "--deposit-shells {deposit_shells} plus seller probe commission {probe_need}: overflows the u128 ECC[2] raw range"
        )
    })?;
    if note_ecc_raw < need {
        anyhow::bail!(
            "provision: note ECC[2] SHELL = {note_ecc_raw} raw (~{} SHELL), but --deposit-shells \
             {deposit_shells} needs {deploy_need} raw (~{deposit_shells} SHELL) for RootModel + TokenContract \
             deploys plus {probe_need} raw seller probe commission at price_per_tick={price_per_tick}. \
             Lower --deposit-shells (default {DEFAULT_DEPOSIT_SHELLS}) or top up the note's physical ECC[2] SHELL.",
            note_ecc_raw / SHELL_UNIT,
        );
    }
    Ok(())
}

/// interactively ask the operator for the note deposit(SHELL). `Ok(None)` = empty line / non-interactive
/// stdin(caller uses [`DEFAULT_DEPOSIT_SHELLS`]); `Ok(Some)` = a valid amount; **`Err` = a non-empty unparseable
/// line** -- fail-closed: a typo must NOT silently fall back to the default for a live-spend input.
#[cfg(feature = "shellnet")]
pub(crate) fn prompt_deposit_shells() -> Result<Option<u128>> {
    use std::io::{IsTerminal as _, Write as _};
    if !std::io::stdin().is_terminal() {
        return Ok(None);
    }
    eprint!(
        "Note ECC[2] allocation in SHELL (1 SHELL = 1e9 raw; split about deposit/2 per deploy) [default {DEFAULT_DEPOSIT_SHELLS}]: "
    );
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let t = line.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let n = t
        .parse::<u128>()
        .map_err(|e| anyhow::anyhow!("note deposit '{t}': not a valid whole SHELL amount ({e})"))?;
    Ok(Some(n))
}

/// Human-readable view of the identity's **note tree** snapshot(R14): state across all sub-notes under
/// the key. "From whom" = the counterparty note's anonymous public key.
pub(crate) fn print_tree_snapshot(s: &TreeSnapshot) {
    print!("{}", render_tree_snapshot(s));
}

pub(crate) fn render_tree_snapshot(s: &TreeSnapshot) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    writeln!(
        &mut out,
        "identity note tree ({} sub-notes polled):",
        s.note_ids.len()
    )
    .unwrap();
    for id in &s.note_ids {
        writeln!(&mut out, "  * {id}").unwrap();
    }
    writeln!(&mut out, "tree exposure (at risk): {} SHELL", s.exposure).unwrap();
    writeln!(&mut out, "offers in book: {}", s.offers.len()).unwrap();
    for o in &s.offers {
        writeln!(
            &mut out,
            "  * {} -- {} SHELL/tick x {} ticks",
            o.token_contract, o.price_per_tick, o.max_ticks
        )
        .unwrap();
    }
    writeln!(&mut out, "deals: {}", s.deals.len()).unwrap();
    for d in &s.deals {
        let role = match d.role {
            DealRole::Buyer => "buyer",
            DealRole::Seller => "seller",
        };
        let cp = d.counterparty.as_deref().unwrap_or("--(no match)");
        let by_fact = match &d.snapshot {
            Some(snap) => format!(
                "by-fact: to seller {} / refund {} / locked(buyer {}, seller {}) / burn {}{}",
                snap.seller_received,
                snap.buyer_refunded,
                snap.buyer_locked,
                snap.seller_locked,
                snap.burned,
                if snap.closed { " * CLOSED" } else { "" }
            ),
            None => "stream not opened".to_string(),
        };
        writeln!(
            &mut out,
            "  * {} [{}] counterparty {} * {} SHELL/tick * {}",
            d.token_contract, role, cp, d.price_per_tick, by_fact
        )
        .unwrap();
        // Surface by-fact anomalies: an orphaned lock / a lock that survived a STOP / a buyer lock
        // past the two-tick invariant must be HIGHLIGHTED, not hidden behind a clean number.
        for a in deal_anomalies(d) {
            let msg = match a {
                DealAnomaly::LockedNoMatch { locked } => {
                    format!("orphaned lock -- {locked} SHELL locked with no matched counterparty ()")
                }
                DealAnomaly::LockedAfterClose { locked } => {
                    format!("settlement mismatch -- {locked} SHELL still locked after the deal closed ()")
                }
                DealAnomaly::BuyerLockExceedsTwoTicks { buyer_lead, ceiling } => format!(
                    "two-tick invariant -- buyer lead {buyer_lead} SHELL exceeds the {ceiling} ceiling ()"
                ),
            };
            writeln!(&mut out, "      ! ANOMALY: {msg}").unwrap();
        }
    }
    // Per-model by-fact accounting, per role: the same deals, grouped by served model and
    // counterparty, with tokens(finalized ticks) / SHELL settled / locked / burned.
    write_role_breakdown(
        &mut out,
        "seller",
        "recv",
        &per_model_breakdown(&s.deals, DealRole::Seller),
    );
    write_role_breakdown(
        &mut out,
        "buyer",
        "paid",
        &per_model_breakdown(&s.deals, DealRole::Buyer),
    );
    out
}

fn write_role_breakdown(
    out: &mut String,
    role_label: &str,
    money_label: &str,
    models: &[ModelBreakdown],
) {
    use std::fmt::Write as _;

    if models.is_empty() {
        return;
    }
    writeln!(out, "{role_label} accounting (by model):").unwrap();
    for m in models {
        writeln!(
            out,
            "  > model {} -- tokens {} * {} {} SHELL * locked {} * burned {}",
            m.model, m.tokens, money_label, m.money, m.locked, m.burned
        )
        .unwrap();
        for c in &m.counterparties {
            let cp = c.counterparty.as_deref().unwrap_or("--(no match)");
            writeln!(
                out,
                "      -> {} -- tokens {} * {} {} SHELL * locked {} * burned {}",
                cp, c.tokens, money_label, c.money, c.locked, c.burned
            )
            .unwrap();
        }
    }
}

#[cfg(test)]
mod monitor_render_tests {
    use super::render_tree_snapshot;
    use dexdo_core::{DealChainState, DealRole, DealView, StreamSnapshot, TreeSnapshot};

    fn state(funded: bool, opened: bool, disputed: bool, probe_accepted: bool) -> DealChainState {
        DealChainState {
            funded,
            opened,
            disputed,
            probe_accepted,
            funded_time: None,
            last_advance: 0,
        }
    }

    fn snapshot_from_state(
        state: DealChainState,
        seller_received: u64,
        buyer_locked: u64,
        seller_locked: u64,
    ) -> StreamSnapshot {
        StreamSnapshot {
            seller_locked,
            buyer_locked,
            buyer_lead: 0,
            seller_received,
            buyer_refunded: 0,
            burned: 0,
            closed: state.is_stopped(),
        }
    }

    fn rendered_market_monitor(token_contract: &str, snapshot: StreamSnapshot) -> String {
        let exposure = if snapshot.closed {
            0
        } else {
            snapshot.seller_locked
        };
        let tree = TreeSnapshot {
            note_ids: vec!["seller-note".to_string()],
            offers: Vec::new(),
            deals: vec![DealView {
                token_contract: token_contract.to_string(),
                role: DealRole::Seller,
                counterparty: Some("buyer-pubkey".to_string()),
                price_per_tick: 400,
                model: Some("qwen--qwen3--32b".to_string()),
                snapshot: Some(snapshot),
            }],
            exposure,
        };
        render_tree_snapshot(&tree)
    }

    #[test]
    fn funded_never_opened_market_snapshot_is_active_without_false_18() {
        let rendered = rendered_market_monitor(
            "tc-funded-never-opened",
            snapshot_from_state(state(true, false, false, false), 0, 3075, 10),
        );
        let expected = "\
identity note tree (1 sub-notes polled):
  * seller-note
tree exposure (at risk): 10 SHELL
offers in book: 0
deals: 1
  * tc-funded-never-opened [seller] counterparty buyer-pubkey * 400 SHELL/tick * by-fact: to seller 0 / refund 0 / locked(buyer 3075, seller 10) / burn 0
seller accounting (by model):
  > model qwen--qwen3--32b -- tokens 0 * recv 0 SHELL * locked 10 * burned 0
      -> buyer-pubkey -- tokens 0 * recv 0 SHELL * locked 10 * burned 0
";
        assert_eq!(rendered, expected);
        assert!(!rendered.contains("CLOSED"), "{rendered}");
        assert!(!rendered.contains("settlement mismatch"), "{rendered}");
    }

    #[test]
    fn opened_probe_market_snapshot_is_active_without_false_18() {
        let rendered = rendered_market_monitor(
            "tc-opened-probe",
            snapshot_from_state(state(true, true, false, false), 0, 4100, 10),
        );
        let expected = "\
identity note tree (1 sub-notes polled):
  * seller-note
tree exposure (at risk): 10 SHELL
offers in book: 0
deals: 1
  * tc-opened-probe [seller] counterparty buyer-pubkey * 400 SHELL/tick * by-fact: to seller 0 / refund 0 / locked(buyer 4100, seller 10) / burn 0
seller accounting (by model):
  > model qwen--qwen3--32b -- tokens 0 * recv 0 SHELL * locked 10 * burned 0
      -> buyer-pubkey -- tokens 0 * recv 0 SHELL * locked 10 * burned 0
";
        assert_eq!(rendered, expected);
        assert!(!rendered.contains("CLOSED"), "{rendered}");
        assert!(!rendered.contains("settlement mismatch"), "{rendered}");
    }

    #[test]
    fn stopped_market_snapshot_with_locked_escrow_still_flags_18() {
        let rendered = rendered_market_monitor(
            "tc-stopped-locked",
            snapshot_from_state(state(true, false, false, true), 810, 4100, 10),
        );
        let expected = "\
identity note tree (1 sub-notes polled):
  * seller-note
tree exposure (at risk): 0 SHELL
offers in book: 0
deals: 1
  * tc-stopped-locked [seller] counterparty buyer-pubkey * 400 SHELL/tick * by-fact: to seller 810 / refund 0 / locked(buyer 4100, seller 10) / burn 0 * CLOSED
      ! ANOMALY: settlement mismatch -- 4110 SHELL still locked after the deal closed ()
seller accounting (by model):
  > model qwen--qwen3--32b -- tokens 2 * recv 810 SHELL * locked 10 * burned 0
      -> buyer-pubkey -- tokens 2 * recv 810 SHELL * locked 10 * burned 0
";
        assert_eq!(rendered, expected);
    }
}
