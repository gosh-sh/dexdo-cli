//! `dexdo` CLI argument structs(clap-derived), split out of `main.rs`(PR3, move-only). The parser surface
//! for `seller`/`buyer`/`monitor`/`provision`/`destroy`/`recover`. Behavior-identical to the pre-split defs.

use anyhow::{bail, Result};
use clap::{ArgGroup, Args, Subcommand, ValueEnum};
use http::uri::Authority;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

fn parse_market_data_limit(s: &str) -> Result<u32, String> {
    parse_bounded_u32(s, 1, 200, "--limit")
}

fn parse_market_data_depth_limit(s: &str) -> Result<u32, String> {
    parse_bounded_u32(s, 1, 1000, "--limit")
}

fn parse_positive_u64(s: &str) -> Result<u64, String> {
    let value = s
        .parse::<u64>()
        .map_err(|e| format!("expected positive integer: {e}"))?;
    if value == 0 {
        return Err("expected positive integer, got 0".to_string());
    }
    Ok(value)
}

fn parse_bounded_u32(s: &str, min: u32, max: u32, name: &str) -> Result<u32, String> {
    let value = s
        .parse::<u32>()
        .map_err(|e| format!("{name}: expected integer in {min}..={max}: {e}"))?;
    if !(min..=max).contains(&value) {
        return Err(format!(
            "{name}: expected integer in {min}..={max}, got {value}"
        ));
    }
    Ok(value)
}

pub(crate) const DEFAULT_CHAIN_READ_TIMEOUT_SECS: u64 = 30;

/// Bounded timeout for direct shellnet getter reads. This is intentionally separate from market-data's
/// indexer HTTP timeout: `market-data` is the fast indexer path and must not be wrapped here.
#[derive(Args, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChainReadTimeoutArgs {
    /// Bound direct shellnet chain reads and return a retryable error instead of hanging.
    #[arg(
        long,
        default_value_t = DEFAULT_CHAIN_READ_TIMEOUT_SECS,
        value_parser = parse_positive_u64,
        value_name = "SECS"
    )]
    pub(crate) read_timeout_secs: u64,
}

/// First-class mock flags -- common to both roles.
#[derive(Args, Clone)]
pub(crate) struct MockFlags {
    /// Mock model: fake tokens instead of a real upstream.
    #[arg(long)]
    pub(crate) mock_model: bool,
    /// Mock chain: `MockChainBackend` instead of real shellnet.
    #[arg(long)]
    pub(crate) mock_chain: bool,
}

impl MockFlags {
    /// Buyer mock-demo: on a mock chain the upstream is also mock -- there is no real stream, so
    /// we require `--mock-model`. The chain is selected separately (`--mock-chain` -> mock, otherwise real
    /// shellnet behind the feature) -- the former `bail!` about "" is removed(the real backend is available).
    pub(crate) fn require_mock_model(&self) -> Result<()> {
        if !self.mock_model {
            bail!("the buyer mock chain also requires --mock-model (,); the real path is without --mock-chain");
        }
        Ok(())
    }
}

/// Note identity -- common to all subcommands(`seller`/`buyer`/`monitor`).
/// Path to the root key; dexdo only **reads** it
/// and derives the note in memory. Same key -> same note -> continuity between runs.
#[derive(Args, Clone)]
pub(crate) struct IdentityArgs {
    /// Path to a file with the 32-byte hex secret of the note's root key -- the **persistent** identity
    /// . The root key is derivable: identity = the key AND the whole
    /// tree of(sub)notes under it. Without the flag -- an ephemeral note (a warning; mock-demo only,
    /// without continuity). An invalid/inaccessible path is an explicit failure, not a silent `generate()`.
    #[arg(long)]
    pub(crate) note_key: Option<PathBuf>,
    /// Index of the tree(sub)note this subcommand operates on: one root
    /// key -> many notes, a specific deal/order lives on a specific sub-note. Reproducible
    /// (same key+index -> same note). The monitor aggregates over the whole tree(`--tree-width`).
    #[arg(long, default_value_t = 0)]
    pub(crate) note_index: u32,
    /// **Real shellnet:** on-chain address of the actor's already **provisioned** `PrivateNote` (minted by
    /// `mint_pn_pool`). dexdo does NOT create the note -- it only reads/signs it
    /// with the owner key from `--note-key`(for the real path this is the SDK secret hex, not an HD seed). On the mock path
    /// (`--mock-chain`) it is ignored.
    #[arg(long)]
    pub(crate) note_addr: Option<String>,
}

/// Issue: explicit client-side ModelRegistry validation config.
#[derive(Args, Clone, Default)]
pub(crate) struct ModelRegistryValidationArgs {
    /// Strict JSON config with explicit seller/buyer check_model_registry booleans.
    #[arg(long)]
    pub(crate) model_registry_validation: Option<PathBuf>,
    /// Explicit ModelRegistry address override; requires --model-registry-validation.
    #[arg(long)]
    pub(crate) model_registry_address: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GatewayAdvertiseAddr(Authority);

impl GatewayAdvertiseAddr {
    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for GatewayAdvertiseAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for GatewayAdvertiseAddr {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let authority = s
            .parse::<Authority>()
            .map_err(|e| format!("--gateway-advertise: expected host:port: {e}"))?;
        if authority.host().is_empty() || authority.port_u16().is_none() {
            return Err("--gateway-advertise: expected host:port".to_string());
        }
        if authority.as_str().contains('@') {
            return Err("--gateway-advertise: expected host:port without userinfo".to_string());
        }
        Ok(Self(authority))
    }
}

#[derive(Args)]
pub(crate) struct SellerArgs {
    #[command(flatten)]
    pub(crate) mock: MockFlags,
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    /// Local bind/listen address for accepting buyer connections; seller-side equivalent of buyer --local-listen.
    #[arg(long, default_value = "127.0.0.1:8443")]
    pub(crate) gateway_listen: SocketAddr,
    /// Public gateway host:port written to the buyer handover. Defaults to --gateway-listen.
    #[arg(long, value_name = "HOST:PORT")]
    pub(crate) gateway_advertise: Option<GatewayAdvertiseAddr>,
    /// Endpoints file -- the handover seam. By default, in the platform data directory
    /// (Linux `~/.local/share/dexdo`, macOS App Support, Windows `%APPDATA%`); `--endpoints-file`
    /// overrides it(D6, portability).
    #[arg(long)]
    pub(crate) endpoints_file: Option<PathBuf>,
    /// Local deal-handle directory. Defaults to the platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
    /// Deal token_contract address. Optional if `--market` is given(loaded from the manifest).
    #[arg(long)]
    pub(crate) token_contract: Option<String>,
    /// Issue: load the deal `token_contract` from a `dexdo provision` market manifest instead of
    /// passing `--token-contract` by hand. The seller still serves the model named by `--model`.
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// Review: deal nonce for the per-deal `TokenContract`(matches `dexdo provision --nonce`).
    /// Required with an explicit `--token-contract` on real shellnet -- the IOB rejects an offer whose
    /// `tokenContract` does not derive from `(sellerPubkey, nonce)`. With `--market` it is taken from
    /// the manifest, so do not pass both.
    #[arg(long)]
    pub(crate) nonce: Option<u64>,
    /// Tick price P in SHELL.
    #[arg(long, default_value_t = 1000)]
    pub(crate) price_per_tick: u64,
    /// How many fake tokens to serve in `--mock-model` mode. Real upstreams follow the request's
    /// `max_tokens`, capped by the market's `max_ticks * TICK_SIZE`.
    #[arg(long, default_value_t = 8)]
    pub(crate) mock_token_count: u64,
    /// Model name from the config: a key or `frame_model`. Required on real shellnet
    /// even with `--mock-model`, because it selects the on-chain `modelHash`; optional only for the
    /// `--mock-chain --mock-model` demo.
    #[arg(long)]
    pub(crate) model: Option<String>,
    /// Path to the models config. Defaults to `models.json` in the working directory.
    #[arg(long, default_value = "models.json")]
    pub(crate) models: PathBuf,
    /// **Real shellnet:** manifest of the deployed contracts(SuperRoot/DappConfig addresses). The release
    /// places it next to the binary; default -- `contracts/deployed.shellnet.json` in the working directory. The
    /// probe-commission is posted by the note itself -- no operator wallet.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
    /// **Real shellnet:** maximum raw ECC[2] units the seller may post for `fundProbeCommission`.
    /// The client reads `getProbe().probeCommission` and posts exactly that raw amount; this limit fails
    /// closed if the contract requires more. The default is 1_000_000 raw units, i.e. 0.001 SHELL.
    #[arg(long, default_value_t = 1_000_000)]
    pub(crate) probe_shell: u128,
    /// Persistent failure policy JSON. Defaults to XDG config (`~/.config/dexdo/policy.json`, Windows
    /// `%APPDATA%\dexdo\policy.json`). Real seller startup fails closed if missing or incomplete.
    #[arg(long)]
    pub(crate) policy: Option<PathBuf>,
}

impl SellerArgs {
    pub(crate) fn gateway_advertise_addr(&self) -> String {
        self.gateway_advertise
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| self.gateway_listen.to_string())
    }
}

#[derive(Args, Clone)]
pub(crate) struct BuyerArgs {
    #[command(flatten)]
    pub(crate) mock: MockFlags,
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    /// Endpoints file -- where to read the enc-endpoint from. Default -- the platform data directory
    /// (see `seller`); `--endpoints-file` overrides it(D6).
    #[arg(long)]
    pub(crate) endpoints_file: Option<PathBuf>,
    /// Local deal-handle directory. Defaults to the platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
    /// Deal token_contract address. Optional if `--market` is given(loaded from the manifest).
    #[arg(long)]
    pub(crate) token_contract: Option<String>,
    /// **Resume/connect** to an ALREADY-matched deal without placing a new buy -- read the on-chain handover and
    /// stream/serve. Use it after a `buyer` timed out awaiting the seller's `open_stream`, once the seller has
    /// opened it: the escrow is already committed, so a fresh buy would double-pay. Works BOTH model-only
    /// (default: the deal `TokenContract` is recovered from THIS note's own `InferenceFilledConfirmed` event --
    /// no hand-pasted address) and with an explicit `--token-contract`/`--market`.
    #[arg(long)]
    pub(crate) resume: bool,
    /// Issue: load `token_contract` + `frame_model` from a `dexdo provision` market manifest
    /// instead of passing them by hand.
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// One-shot buyer receive cap. In local consumer-API mode, per-request `max_tokens` is honored and capped by
    /// the purchased deal budget(`--ticks x TICK_SIZE`) instead of this debug/one-shot cap.
    #[arg(long, default_value_t = 8)]
    pub(crate) max_tokens: u64,
    /// Local bind/listen address for the OpenAI-compatible consumer endpoint; buyer-side equivalent of seller --gateway-listen.
    /// If set -- the client brings up an HTTP interface instead of a one-shot stream+STOP.
    #[arg(long)]
    pub(crate) local_listen: Option<SocketAddr>,
    /// Continuity mode for --local-listen. proactive keeps a warm next deal ready after idle close,
    /// low remaining budget, or no current deal; it may pre-buy while idle and spend the documented probe/idle
    /// cost. on-demand buys only after active/recent consumer traffic; it avoids idle spend, but the first
    /// request after idle may wait for a fresh deal.
    #[arg(long, value_enum, default_value_t = ContinuityModeArg::Proactive, value_name = "MODE")]
    pub(crate) continuity_mode: ContinuityModeArg,
    /// Emit machine-readable JSONL lifecycle events on stdout.
    #[arg(long)]
    pub(crate) json: bool,
    /// Additionally bring up an Anthropic-compatible `/v1/messages` via transcoding(B20).
    #[arg(long)]
    pub(crate) anthropic_compat: bool,
    /// Model id of the configured frame/market(B2/B19) -- the only served model. Alias: --model.
    /// **Required**(review Y2): it used to silently default to `dexdo-mock`, which on a real deal made
    /// B7 verify substitution against the wrong model(a silent no-op / false positive). Now the operator
    /// sets it explicitly(for mock-demo -- `--frame-model dexdo-mock`). Optional if `--market` is given
    /// (loaded from the manifest). Horizon: derive it from the deal's per-model `InferenceOrderBook`.
    #[arg(long, visible_alias = "model")]
    pub(crate) frame_model: Option<String>,
    /// accept a model whose family has **no content-identity check** (no B8 fingerprint and no B7
    /// reference/key) on NAME-only evidence. Without it the buyer refuses to open the consumer API for such a
    /// model -- a seller could serve a cheaper model under the correct name undetected. When a fingerprint OR a
    /// reference key IS available the content gate runs regardless of this flag(it cannot be opted out of).
    #[arg(long)]
    pub(crate) allow_unverified_model: bool,
    /// path to the models config(JSON) providing the **per-model verification data** (B5 vocab, B8
    /// fingerprints, B7-full reference via base_url/served_model/api_key_env). Defaults to `models.json` in the
    /// working directory. A model absent from this config has no verification data -> the buyer fails closed
    /// (unless `--allow-unverified-model`).
    #[arg(long, default_value = "models.json")]
    pub(crate) models: PathBuf,
    /// How many ticks the buyer purchases. Not used on the mock path.
    #[arg(long, default_value_t = 8)]
    pub(crate) ticks: u128,
    /// **Real shellnet:** the per-tick price LIMIT(`maxPricePerTick`) -- crosses with ask <= limit.
    /// Must be >= ask. Book deposit check: `escrow >= ticks x maxPricePerTick x (1 + 2.5 %
    /// fee)` -- the fee is charged ON TOP of the limit, so `escrow = limit x ticks`(without headroom) does
    /// not pass; keep the limit noticeably below `escrow /(ticks x 1.025)`.
    #[arg(long, default_value_t = 1_000_000)]
    pub(crate) max_price_per_tick: u128,
    /// **Real shellnet:** escrow(the note's ECC SHELL) for `placeInferenceBuy`. Omit it to use EXACTLY
    /// the required `ticks x max_price_per_tick x(1 + 2.5 % book fee)`. An
    /// explicit value must equal `required`: under-funding orphans the escrow; over-funding strands the
    /// surplus when the buy rests and is filled as a maker. Without `--mock-chain`.
    #[arg(long)]
    pub(crate) escrow: Option<u128>,
    /// **Real shellnet:** manifest of the deployed contracts(see `seller --contracts`).
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
    /// Persistent failure policy JSON. Defaults to XDG config (`~/.config/dexdo/policy.json`, Windows
    /// `%APPDATA%\dexdo\policy.json`). Real buyer startup fails closed if missing or incomplete.
    #[arg(long)]
    pub(crate) policy: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ContinuityModeArg {
    Proactive,
    OnDemand,
}

impl ContinuityModeArg {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Proactive => "proactive",
            Self::OnDemand => "on-demand",
        }
    }

    pub(crate) fn as_planner_mode(self) -> dexdo::buyer::continuity::ContinuityMode {
        match self {
            Self::Proactive => dexdo::buyer::continuity::ContinuityMode::Proactive,
            Self::OnDemand => dexdo::buyer::continuity::ContinuityMode::OnDemand,
        }
    }
}

/// Monitor arguments -- identity + selected chain; read-only.
#[derive(Args)]
pub(crate) struct MonitorArgs {
    #[command(flatten)]
    pub(crate) mock: MockFlags,
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// Endpoints/mock-chain-state file(the same one as `seller`/`buyer`). Default -- the platform path.
    #[arg(long)]
    pub(crate) endpoints_file: Option<PathBuf>,
    /// How many tree(sub)notes to poll (, R14: the monitor aggregates over ALL notes under
    /// the key, not just the root). The window `index = 0..width` is reproducible from the key. `--note-index`
    /// is ignored by the monitor(it sees the whole tree, not a single sub-note).
    #[arg(long, default_value_t = 8)]
    pub(crate) tree_width: u32,
    /// **Real shellnet:** the operator's market manifest(s) from `dexdo provision` -- the monitor
    /// reads each market's `TokenContract` by-fact state on-chain (a `RealNote` is a single key, not an HD tree,
    /// so the real monitor reads the markets it is given, not a `--tree-width` window). Repeat for many markets.
    #[arg(long)]
    pub(crate) market: Vec<PathBuf>,
    /// Deployed-contracts manifest(SuperRoot/DappConfig) for the real-chain connection.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

/// Doctor / health check: read-only shellnet version and manifest freshness guard.
#[derive(Args)]
pub(crate) struct DoctorArgs {
    /// Network to check. Only `shellnet` is currently supported.
    #[arg(long, default_value = "shellnet")]
    pub(crate) network: String,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
    /// Optional `dexdo provision` market manifest. When supplied, doctor also verifies the market IOB/TC are
    /// active and carry the expected code hash.
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// Persistent failure policy JSON to inspect. Defaults to the platform config path.
    #[arg(long)]
    pub(crate) policy: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct PolicyArgs {
    #[command(subcommand)]
    pub(crate) command: PolicyCommand,
}

#[derive(Subcommand)]
pub(crate) enum PolicyCommand {
    /// Create or complete a policy.json template without overwriting existing answers.
    Init(PolicyInitArgs),
    /// Print the current policy.json.
    Show(PolicyPathArgs),
    /// Open policy.json in $VISUAL or $EDITOR, scaffolding it first if missing.
    Edit(PolicyPathArgs),
}

#[derive(Args)]
pub(crate) struct PolicyInitArgs {
    /// Which role section to scaffold.
    #[arg(long, value_enum, default_value_t = PolicyRoleArg::Both)]
    pub(crate) role: PolicyRoleArg,
    /// Policy file path. Defaults to the platform config path.
    #[arg(long)]
    pub(crate) path: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct PolicyPathArgs {
    /// Policy file path. Defaults to the platform config path.
    #[arg(long)]
    pub(crate) path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum PolicyRoleArg {
    Buyer,
    Seller,
    Both,
}

/// Provision arguments: bring up a per-deal market from the seller note alone.
#[derive(Args)]
pub(crate) struct ProvisionArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    /// Frame model id the market serves(determines the on-chain `modelHash`).
    #[arg(long)]
    pub(crate) frame_model: String,
    /// Deployed-contracts manifest(SuperRoot/DappConfig addresses).
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
    /// Deal nonce -- disambiguates multiple `TokenContract`s under one `RootModel`.: REQUIRED and must be
    /// UNIQUE per deal -- the per-deal `TokenContract` derives from `(sellerPubkey, nonce)`, so a reused/default
    /// nonce collides(overwrites a prior deal's TC). No unsafe `0` default; pass a distinct value per deal.
    #[arg(long)]
    pub(crate) nonce: Option<u64>,
    /// Tick price P(SHELL) for the deal `TokenContract`.
    #[arg(long, default_value_t = 1000)]
    pub(crate) price_per_tick: u128,
    /// Max ticks the deal `TokenContract` bounds to.
    #[arg(long, default_value_t = 1024)]
    pub(crate) max_ticks: u128,
    /// Note ECC[2] allocation in whole SHELL, not raw nano/vmshell(1 SHELL = 1e9 raw).
    /// Split about `deposit/2` per deploy after `fundDeployShell`(RootModel + TokenContract).
    /// Unused deploy remainder burns at `destroy`; raise this value if a live TC needs more runtime gas.
    /// Fail-closed: below-floor / overflow is rejected, not clamped. Absent on an interactive terminal prompts.
    #[arg(long)]
    pub(crate) deposit_shells: Option<u128>,
    /// Output path for the produced market manifest(OB/RootModel + the deployed TC address). `dexdo
    /// seller`/`buyer` load it via `--market`.
    #[arg(long, default_value = "market.json")]
    pub(crate) output: PathBuf,
}

/// Args for `dexdo destroy`: the seller CLOSES a STOPped deal's `TokenContract` (DESTRUCTIVE -- the held
/// ~a-few-vmshell leftover burns cross-dapp; negligible at the right-sized ~10/deploy funding,).
#[derive(Args)]
pub(crate) struct DestroyArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// The deal `TokenContract` to destroy(or pass `--market`).
    #[arg(long)]
    pub(crate) token_contract: Option<String>,
    /// A `dexdo provision` market manifest carrying the `token_contract`(alternative to `--token-contract`).
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// **Optional acknowledgement.** `destroy` `selfdestruct`s the TC; the held leftover burns cross-dapp
    /// (not credited back to the note). At the right-sized ~10/deploy funding that leftover is ~a few vmshell, so
    /// this flag is no longer required(a fail-closed gate for ~110 was overkill) -- kept for back-compat.
    #[arg(long)]
    pub(crate) acknowledge_burn: bool,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

/// Args for `dexdo recover`: the BUYER STOPs an orphaned OPEN deal from its note WITHOUT placing a
/// new buy, so a stuck deal(the buyer process died) can be closed and the seller can then `destroy` it.
#[derive(Args)]
pub(crate) struct RecoverArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// The OPEN deal `TokenContract` to STOP(or pass `--market`).
    #[arg(long)]
    pub(crate) token_contract: Option<String>,
    /// A `dexdo provision` market manifest carrying the `token_contract`(alternative to `--token-contract`).
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// DEXDO_PN_POOL fallback carrying the note owner key and last matched TokenContract. Defaults to env DEXDO_PN_POOL.
    #[arg(long)]
    pub(crate) pool: Option<PathBuf>,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

/// Args for `dexdo dispute`: the BUYER opens an on-chain dispute on an OPEN deal -- `streamDispute` ->
/// `TC.dispute()` LOCKS both notes until `releaseDispute`/arbitration. The anti-scam lever for an
/// observed substitution/fraud -- strictly stronger than `recover`'s STOP(which still pays for ticks).
#[derive(Args)]
pub(crate) struct DisputeArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// The OPEN deal `TokenContract` to dispute(or pass `--market`).
    #[arg(long)]
    pub(crate) token_contract: Option<String>,
    /// A `dexdo provision` market manifest carrying the `token_contract`(alternative to `--token-contract`).
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// DEXDO_PN_POOL fallback carrying the note owner key and last matched TokenContract. Defaults to env DEXDO_PN_POOL.
    #[arg(long)]
    pub(crate) pool: Option<PathBuf>,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

/// Args for `dexdo reclaim`: the BUYER reclaims escrow on seller no-show. OPEN abandoned deals use
/// `streamReclaim` -> `TC.reclaimOnTimeout()` after `STREAM_TIMEOUT`; funded-but-never-opened deals use
/// `streamCleanup` -> `TC.cleanupUnopened()` after `MATCH_OPEN_TIMEOUT`. Fails closed locally on ownership + timer.
#[derive(Args)]
pub(crate) struct ReclaimArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// The funded/OPEN deal `TokenContract` to reclaim(or pass `--market`).
    #[arg(long)]
    pub(crate) token_contract: Option<String>,
    /// A `dexdo provision` market manifest carrying the `token_contract`(alternative to `--token-contract`).
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// DEXDO_PN_POOL fallback carrying the note owner key and last matched TokenContract. Defaults to env DEXDO_PN_POOL.
    #[arg(long)]
    pub(crate) pool: Option<PathBuf>,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

/// Args for `dexdo release-dispute`: the SELLER concedes an on-chain dispute on its deal TC,
/// unlocking both notes and returning the contested tick/deposit to the buyer.
#[derive(Args)]
pub(crate) struct ReleaseDisputeArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// The disputed deal `TokenContract` to release(or pass `--market`).
    #[arg(long)]
    pub(crate) token_contract: Option<String>,
    /// A `dexdo provision` market manifest carrying the `token_contract`(alternative to `--token-contract`).
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

/// Args for `dexdo withdraw-shell`: the SELLER withdraws finalized `_finalizedOwed` SHELL from a deal TC.
#[derive(Args)]
pub(crate) struct WithdrawShellArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// The deal `TokenContract` to withdraw from(or pass `--market`).
    #[arg(long)]
    pub(crate) token_contract: Option<String>,
    /// A `dexdo provision` market manifest carrying the `token_contract`(alternative to `--token-contract`).
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// Amount to withdraw in the contract's SHELL unit. If omitted, withdraws all current `finalizedOwed`.
    #[arg(long)]
    pub(crate) amount: Option<u128>,
    /// Recipient PrivateNote/wallet address. Defaults to `--note-addr`.
    #[arg(long)]
    pub(crate) recipient: Option<String>,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

/// Deploy the per-model `InferenceOrderBook`(the shared market for a model) if it is not yet on-chain --
/// note-funded, a separate operate step the seller runs before posting offers. The book address is
/// deterministic from the model's `model_hash`, so the deploy is idempotent(already-deployed -> no-op).
#[derive(Args)]
pub(crate) struct MarketDeployArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    /// Frame model id whose order book to deploy(its `model_hash` derives the book address).
    #[arg(long)]
    pub(crate) frame_model: String,
    /// Deployed-contracts manifest(SuperRoot/DappConfig addresses).
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

/// Read-only market discovery: list configured/per-manifest inference order books.
#[derive(Args)]
pub(crate) struct MarketsArgs {
    /// Emit the stable `dexdo.markets.v1` JSON object.
    #[arg(long)]
    pub(crate) json: bool,
    #[command(flatten)]
    pub(crate) read_timeout: ChainReadTimeoutArgs,
    /// Use the local mock chain state next to --endpoints-file.
    #[arg(long)]
    pub(crate) mock_chain: bool,
    /// Mock-chain endpoints/state file used when --mock-chain is set.
    #[arg(long)]
    pub(crate) endpoints_file: Option<PathBuf>,
    /// Frame model id for mock-chain discovery output.
    #[arg(long, default_value = "dexdo-mock")]
    pub(crate) frame_model: String,
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    /// Optional provisioned market manifest(s). If absent, markets are derived from --models via --note-addr.
    #[arg(long)]
    pub(crate) market: Vec<PathBuf>,
    /// Models config used when --market is absent.
    #[arg(long, default_value = "models.json")]
    pub(crate) models: PathBuf,
    /// Any active inference PrivateNote address used to derive per-model order-book addresses when --market is absent.
    #[arg(long)]
    pub(crate) note_addr: Option<String>,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

/// One model's order book as the human-readable box table: the same view the buyer shows before a
/// buy -- #/price-per-tick/max-ticks + full tokenContract addresses. Read-only, keyed by the canonical model.
#[derive(Args)]
pub(crate) struct MarketArgs {
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    #[command(flatten)]
    pub(crate) read_timeout: ChainReadTimeoutArgs,
    /// Canonical model id(`producer--model--version`, e.g. `qwen--qwen3--32b`) whose book to render.
    pub(crate) model: String,
    /// Any active inference PrivateNote address used to derive the per-model order-book address.
    #[arg(long)]
    pub(crate) note_addr: Option<String>,
    /// Optional provisioned market manifest. If given, the book is read from it instead of `--note-addr`.
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// Models config(used to accept a model KEY as well as a raw canonical frame_model).
    #[arg(long, default_value = "models.json")]
    pub(crate) models: PathBuf,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

/// Read buyer-executable asks for one model book at a concrete tick count and price ceiling.
#[derive(Args)]
pub(crate) struct ExecutableBookArgs {
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    #[command(flatten)]
    pub(crate) read_timeout: ChainReadTimeoutArgs,
    /// Canonical model id(`producer--model--version`, e.g. `qwen--qwen3--32b`) whose book to inspect.
    pub(crate) model: String,
    /// Any active inference PrivateNote address used to derive the per-model order-book address.
    #[arg(long)]
    pub(crate) note_addr: Option<String>,
    /// Optional provisioned market manifest. If given, the book is read from it instead of `--note-addr`.
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// Models config(used to accept a model KEY as well as a raw canonical frame_model).
    #[arg(long, default_value = "models.json")]
    pub(crate) models: PathBuf,
    /// Desired buyer ticks for the executable row filter.
    #[arg(long, default_value_t = 8)]
    pub(crate) ticks: u128,
    /// Buyer price ceiling used by `dexdo buyer --max-price-per-tick`.
    #[arg(long, default_value_t = 1_000_000)]
    pub(crate) max_price_per_tick: u128,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

/// Executable quote over current order-book depth.
#[derive(Args)]
pub(crate) struct QuoteArgs {
    /// Emit the stable `dexdo.quote.v1` JSON object.
    #[arg(long)]
    pub(crate) json: bool,
    #[command(flatten)]
    pub(crate) read_timeout: ChainReadTimeoutArgs,
    /// Use the local mock chain state next to --endpoints-file.
    #[arg(long)]
    pub(crate) mock_chain: bool,
    /// Mock-chain endpoints/state file used when --mock-chain is set.
    #[arg(long)]
    pub(crate) endpoints_file: Option<PathBuf>,
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    /// Optional provisioned market manifest. If absent, --model + --note-addr derive the book.
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// Model key or frame_model from --models. Required when --market is absent.
    #[arg(long)]
    pub(crate) model: Option<String>,
    /// Models config used with --model.
    #[arg(long, default_value = "models.json")]
    pub(crate) models: PathBuf,
    /// Any active inference PrivateNote address used to derive the order-book address when --market is absent.
    #[arg(long)]
    pub(crate) note_addr: Option<String>,
    /// Desired ticks. Mutually exclusive with --budget.
    #[arg(long)]
    pub(crate) ticks: Option<u128>,
    /// Fee-inclusive SHELL budget. Mutually exclusive with --ticks.
    #[arg(long)]
    pub(crate) budget: Option<u128>,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

/// Read-only Dodex inference market-data indexer discovery.
#[derive(Args)]
pub(crate) struct MarketDataArgs {
    /// Dodex indexer base URL. Env fallback: DEXDO_INDEXER_URL. Default: http://dodex-dev.ackinacki.org:8080.
    #[arg(long, global = true)]
    pub(crate) indexer_url: Option<String>,
    /// Output format for scripts/operators.
    #[arg(long, value_enum, default_value_t = MarketDataOutput::Table, global = true)]
    pub(crate) output: MarketDataOutput,
    /// HTTP request timeout in milliseconds.
    #[arg(long, default_value_t = 10_000, global = true)]
    pub(crate) timeout_ms: u64,
    #[command(subcommand)]
    pub(crate) command: MarketDataCommand,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum MarketDataOutput {
    Table,
    Json,
}

#[derive(Subcommand)]
pub(crate) enum MarketDataCommand {
    /// List inference model order books from the read-only indexer.
    List {
        /// Filter by model producer.
        #[arg(long)]
        producer: Option<String>,
        /// Comma-separated status filter, e.g. TRADING.
        #[arg(long)]
        status: Option<String>,
        /// Opaque pagination cursor returned by a previous list call.
        #[arg(long)]
        cursor: Option<String>,
        /// Page size, 1..=200.
        #[arg(long, value_parser = parse_market_data_limit)]
        limit: Option<u32>,
    },
    /// Show one inference model order book by address.
    Show {
        /// InferenceOrderBook address, `0:<64 hex>`.
        inference_order_book_address: String,
    },
    /// Read depth for one inference model order book.
    Depth {
        /// InferenceOrderBook address, `0:<64 hex>`.
        inference_order_book_address: String,
        /// Levels per side, 1..=1000.
        #[arg(long, value_parser = parse_market_data_depth_limit)]
        limit: Option<u32>,
    },
}

/// Own order lifecycle: list/show/cancel/cancel-all for one note.
#[derive(Args)]
pub(crate) struct OrdersArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    #[command(flatten)]
    pub(crate) read_timeout: ChainReadTimeoutArgs,
    /// Optional provisioned market manifest. If absent, --model + --note-addr derive the book.
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// Model key or frame_model from --models. Required when --market is absent.
    #[arg(long)]
    pub(crate) model: Option<String>,
    /// Models config used with --model.
    #[arg(long, default_value = "models.json")]
    pub(crate) models: PathBuf,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
    #[command(subcommand)]
    pub(crate) command: OrdersCommand,
}

#[derive(Subcommand)]
pub(crate) enum OrdersCommand {
    /// List this note's resting orders in the model book.
    List,
    /// Show one of this note's resting orders.
    Show {
        /// On-chain order id.
        order_id: u128,
    },
    /// Cancel one of this note's resting orders.
    Cancel {
        /// On-chain order id.
        order_id: u128,
    },
    /// Cancel all of this note's resting orders in the model book.
    CancelAll,
}

/// Inference subscription lifecycle: recurring buy orders in one model book.
#[derive(Args)]
pub(crate) struct SubscriptionArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    #[command(flatten)]
    pub(crate) read_timeout: ChainReadTimeoutArgs,
    /// Optional provisioned market manifest. If absent, --model + --note-addr derive the book.
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// Model key or frame_model from --models. Required when --market is absent.
    #[arg(long)]
    pub(crate) model: Option<String>,
    /// Models config used with --model.
    #[arg(long, default_value = "models.json")]
    pub(crate) models: PathBuf,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
    #[command(subcommand)]
    pub(crate) command: SubscriptionCommand,
}

#[derive(Subcommand)]
pub(crate) enum SubscriptionCommand {
    /// Place a recurring inference buy subscription from this note.
    Place(SubscriptionPlaceArgs),
    /// Show one subscription's current on-chain state.
    Status {
        /// Resting subscription order id.
        order_id: u128,
    },
    /// Cancel a live subscription order owned by this note.
    Cancel {
        /// Resting subscription order id.
        order_id: u128,
    },
}

#[derive(Args)]
#[command(group(
    ArgGroup::new("subscription_size")
        .required(true)
        .multiple(false)
        .args(["ticks", "budget"])
))]
pub(crate) struct SubscriptionPlaceArgs {
    /// Actor PrivateNote owner key. May be passed before `place` or here after `place`.
    #[arg(long)]
    pub(crate) note_key: Option<PathBuf>,
    /// Per-tick price ceiling. Required because this command moves SHELL.
    #[arg(long)]
    pub(crate) max_price_per_tick: u128,
    /// Desired subscription ticks. Mutually exclusive with --budget.
    #[arg(long)]
    pub(crate) ticks: Option<u128>,
    /// Fee-inclusive SHELL budget. The CLI converts it to whole ticks and sends exact escrow only.
    #[arg(long)]
    pub(crate) budget: Option<u128>,
    /// Client renewal hint stored by the book. Renewal still requires a future re-place.
    #[arg(long)]
    pub(crate) auto_renew: bool,
}

/// Role override for raw token-contract status/close when no local deal handle exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum DealRoleArg {
    Buyer,
    Seller,
}

/// Local deal handle list.
#[derive(Args)]
pub(crate) struct DealsArgs {
    /// Directory containing local deal handle JSON files. Defaults to the platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
}

/// Secret-free trading history from local deal handles.
#[derive(Args)]
pub(crate) struct HistoryArgs {
    /// Directory containing local deal handle JSON files. Defaults to the platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
    /// Only show deals for this actor PrivateNote address.
    #[arg(long)]
    pub(crate) note: Option<String>,
    /// Only show deals for this frame model or model_hash.
    #[arg(long)]
    pub(crate) model: Option<String>,
}

/// Loopback-only read dashboard for local open stream handles.
#[derive(Args)]
pub(crate) struct DashboardArgs {
    /// Loopback HTTP listen address.
    #[arg(long, default_value = "127.0.0.1:8765")]
    pub(crate) listen: SocketAddr,
    /// Directory containing local deal handle JSON files. Defaults to the platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
}

/// Read current on-chain state for a local deal handle or a raw TokenContract address.
#[derive(Args)]
pub(crate) struct StatusArgs {
    /// Emit the stable `dexdo.status.v1` JSON object.
    #[arg(long)]
    pub(crate) json: bool,
    /// Use the local mock chain state next to --endpoints-file.
    #[arg(long)]
    pub(crate) mock_chain: bool,
    /// Mock-chain endpoints/state file used when --mock-chain is set.
    #[arg(long)]
    pub(crate) endpoints_file: Option<PathBuf>,
    /// Local handle id/path or raw TokenContract address.
    pub(crate) deal: String,
    /// Directory containing local deal handle JSON files. Defaults to the platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
    /// Deployed-contracts manifest. Local handles use their saved manifest path unless this overrides it.
    #[arg(long)]
    pub(crate) contracts: Option<PathBuf>,
}

/// Role-aware close/recovery wrapper for a local deal handle or raw TokenContract address.
#[derive(Args)]
pub(crate) struct CloseArgs {
    /// Emit the stable `dexdo.close.v1` JSON object.
    #[arg(long)]
    pub(crate) json: bool,
    /// Use the local mock chain state next to --endpoints-file.
    #[arg(long)]
    pub(crate) mock_chain: bool,
    /// Mock-chain endpoints/state file used when --mock-chain is set.
    #[arg(long)]
    pub(crate) endpoints_file: Option<PathBuf>,
    /// Local handle id/path or raw TokenContract address.
    pub(crate) deal: String,
    /// Directory containing local deal handle JSON files. Defaults to the platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
    /// Role for a raw TokenContract address. Ignored for local handles, which carry their own role.
    #[arg(long)]
    pub(crate) role: Option<DealRoleArg>,
    /// Actor PrivateNote address for a raw TokenContract address. Ignored for local handles unless supplied as a
    /// mismatch guard.
    #[arg(long)]
    pub(crate) note_addr: Option<String>,
    /// Actor PrivateNote owner key. Required only when `close` needs to submit a signed transaction.
    #[arg(long)]
    pub(crate) note_key: Option<PathBuf>,
    /// Deployed-contracts manifest. Local handles use their saved manifest path unless this overrides it.
    #[arg(long)]
    pub(crate) contracts: Option<PathBuf>,
}

/// Secret-free audit export for one deal.
#[derive(Args)]
pub(crate) struct ExportArgs {
    /// Local handle id/path or raw TokenContract address.
    #[arg(long)]
    pub(crate) deal: String,
    /// Output format.
    #[arg(long, value_enum, default_value_t = ExportFormatArg::Json)]
    pub(crate) format: ExportFormatArg,
    /// Directory containing local deal handle JSON files. Defaults to the platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
    /// Deployed-contracts manifest. Local handles use their saved manifest path unless this overrides it.
    #[arg(long)]
    pub(crate) contracts: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ExportFormatArg {
    Json,
    Md,
}

/// Args for `dexdo note`: manage the actor's shellnet PrivateNotes.
#[derive(Args)]
pub(crate) struct NoteArgs {
    #[command(subcommand)]
    pub(crate) command: NoteCommand,
}

#[derive(Subcommand)]
pub(crate) enum NoteCommand {
    /// Balance: read a PrivateNote's public on-chain balances by address only. No key, no signing.
    Balance(NoteBalanceArgs),
    /// Deploy: a wallet-funded `PrivateNote` on shellnet in-process through `gosh.ackinacki`, folded
    /// into a `DEXDO_PN_POOL` pool the `seller`/`buyer` consume.
    Deploy(NoteDeployArgs),
    /// Recover/finalize a wallet-funded `PrivateNote` deploy from a crash-safe recovery state file.
    Recover(NoteRecoverArgs),
    /// Withdraw: submit owner-signed `PrivateNote.withdrawTokens(destWalletAddr, dapp_id)` for a note's
    /// available token balances. This is not a claim that every native/ECC balance is fully retired without
    /// by-fact evidence on the current contract. Fails on-chain if the note is stream-locked.
    Withdraw(NoteWithdrawArgs),
}

/// Args for `dexdo note balance`: read-only PrivateNote balance by address.
#[derive(Args)]
pub(crate) struct NoteBalanceArgs {
    /// PrivateNote address to inspect, `0:<64 hex>`.
    #[arg(long)]
    pub(crate) note_addr: String,
    /// Deployed-contracts manifest(keeps shellnet connection behavior aligned with the other note commands).
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

/// Args for `dexdo note withdraw`: submit a note token-balance withdrawal to a wallet.
#[derive(Args)]
pub(crate) struct NoteWithdrawArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// Destination wallet address -- `half1::half2` or `0:<64 hex>`. Normalized to
    /// `0:<half2>` fail-loud on bad input.
    #[arg(long)]
    pub(crate) to: String,
    /// Deployed-contracts manifest(SuperRoot/DappConfig addresses).
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

/// Args for `dexdo note deploy`: deploys the wallet-funded `PrivateNote` in-process through
/// `gosh.ackinacki`, then adapts the result into the `DEXDO_PN_POOL` schema.
#[derive(Args)]
pub(crate) struct NoteDeployArgs {
    /// Deployed multisig WALLET address that funds the note(no giver).
    #[arg(long)]
    pub(crate) multisig_address: String,
    /// File with the multisig wallet's 32-byte secret hex(the funding key); the secret is never logged.
    #[arg(
        long,
        value_name = "PATH",
        required_unless_present = "multisig_seed_file",
        conflicts_with = "multisig_seed_file"
    )]
    pub(crate) multisig_key: Option<PathBuf>,
    /// File with the multisig wallet seed phrase. TVM-compatible derivation is used; the phrase is never logged.
    #[arg(
        long,
        value_name = "PATH",
        required_unless_present = "multisig_key",
        conflicts_with = "multisig_key"
    )]
    pub(crate) multisig_seed_file: Option<PathBuf>,
    /// PN deposit nominal.
    #[arg(long, default_value = "N100")]
    pub(crate) nominal: String,
    /// Deposit currency.
    #[arg(long, default_value = "nackl", value_parser = ["nackl", "shell", "usdc"])]
    pub(crate) token_type: String,
    /// Shellnet endpoint.
    #[arg(long, default_value = "shellnet.ackinacki.org")]
    pub(crate) endpoint: String,
    /// The `DEXDO_PN_POOL` JSON to append the deployed note to(created if absent).
    #[arg(long)]
    pub(crate) pool: PathBuf,
    /// Crash-safe deploy recovery state. Defaults to `<pool>.recovery.json`; carries the note owner secret.
    #[arg(long, value_name = "PATH")]
    pub(crate) recovery: Option<PathBuf>,
    /// Test/live-gate failpoint: persist complete recovery state, then stop before writing the final pool.
    #[arg(long, hide = true)]
    pub(crate) simulate_interrupt_after_spend_before_pool: bool,
    /// Test/live-gate failpoint: persist a deposit voucher submit checkpoint, submit the wallet transaction,
    /// then stop before waiting/proving/deploying.
    #[arg(long, hide = true)]
    pub(crate) simulate_interrupt_after_deposit_voucher_submit: bool,
    /// Test/live-gate failpoint: persist a deposit VoucherGenerated event, then stop before proving/deploying.
    #[arg(long, hide = true)]
    pub(crate) simulate_interrupt_after_deposit_voucher_event: bool,
    /// Test/live-gate failpoint: persist a SHELL gas voucher submit checkpoint, submit the wallet transaction,
    /// then stop before waiting/proving/funding.
    #[arg(long, hide = true)]
    pub(crate) simulate_interrupt_after_shell_voucher_submit: bool,
    /// Test/live-gate failpoint: submit deployPrivateNote, wait until the note address is active/discoverable,
    /// then stop before recording pn_address/deposit_identifier_hash in recovery.
    #[arg(long, hide = true)]
    pub(crate) simulate_interrupt_after_deploy_before_note_record: bool,
}

/// Args for `dexdo note recover`: finalize a deploy whose recovery state was persisted before/after spend.
#[derive(Args)]
pub(crate) struct NoteRecoverArgs {
    /// Crash-safe recovery state written by `dexdo note deploy`.
    #[arg(long, value_name = "PATH")]
    pub(crate) recovery: PathBuf,
    /// The `DEXDO_PN_POOL` JSON to append the recovered note to(created if absent).
    #[arg(long)]
    pub(crate) pool: PathBuf,
}

/// Args for `dexdo oracle`: OracleEventList/PMP lifecycle for range prediction markets backed by an
/// inference `InferenceOrderBook`.
#[derive(Args)]
pub(crate) struct OracleArgs {
    #[command(subcommand)]
    pub(crate) command: OracleCommand,
}

#[derive(Subcommand)]
pub(crate) enum OracleCommand {
    /// Deploy-if-absent Oracle + OracleEventList, add a range event, deploy/approve the PMP from a note, and
    /// write the resulting oracle-market manifest.
    Provision(Box<OracleProvisionArgs>),
    /// Read OracleEventList/PMP state from an oracle-market manifest.
    State(OracleStateArgs),
    /// Resolve a range PMP: OracleEventList.resolveRange -> OB requestWeeklyMedian -> OEL onWeeklyMedian -> PMP submitResolve.
    Resolve(OracleResolveArgs),
}

#[derive(Args)]
pub(crate) struct OracleProvisionArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// Oracle owner key; also signs RootOracle/Oracle/OracleEventList calls. The secret is never logged.
    #[arg(long)]
    pub(crate) oracle_key: PathBuf,
    /// Deterministic RootOracle name for this Oracle.
    #[arg(long)]
    pub(crate) oracle_name: String,
    /// OracleEventList index under the Oracle.
    #[arg(long, default_value_t = 0)]
    pub(crate) event_list_index: u128,
    /// Human-readable OracleEventList description, used only when the list needs deployment.
    #[arg(long, default_value = "")]
    pub(crate) event_list_description: String,
    /// Existing `dexdo provision` market manifest; its InferenceOrderBook is the range-event price source.
    #[arg(long)]
    pub(crate) market: PathBuf,
    /// Range-event name.
    #[arg(long)]
    pub(crate) event_name: String,
    /// Resolution deadline. Must satisfy contract MIN_RESULT_GAP from current chain time.
    #[arg(long)]
    pub(crate) deadline: u64,
    /// Event description passed into PMP approval.
    #[arg(long, default_value = "")]
    pub(crate) describe: String,
    /// Range upper bound as uint256 decimal. Repeat once per boundary; outcomes must be bounds+1.
    #[arg(long = "bound")]
    pub(crate) bounds: Vec<String>,
    /// Outcome label. Repeat exactly bounds.len()+1 times, in dense outcome-id order.
    #[arg(long = "outcome")]
    pub(crate) outcome_names: Vec<String>,
    /// Initial clean stake per outcome. Repeat exactly once per outcome; each must satisfy the contract minimum.
    #[arg(long = "initial-stake")]
    pub(crate) initial_stakes: Vec<u128>,
    /// PMP collateral token type. `1` is NACKL, matching the live SDK PMP tests.
    #[arg(long, default_value_t = 1)]
    pub(crate) token_type: u32,
    /// Oracle fee paid by the PMP deployer note to this OracleEventList.
    #[arg(long, default_value_t = 0)]
    pub(crate) oracle_fee: u128,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
    /// Output path for the oracle-market manifest.
    #[arg(long, default_value = "oracle-market.json")]
    pub(crate) output: PathBuf,
}

#[derive(Args)]
pub(crate) struct OracleStateArgs {
    /// Oracle-market manifest produced by `dexdo oracle provision`.
    #[arg(long)]
    pub(crate) manifest: PathBuf,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}

#[derive(Args)]
pub(crate) struct OracleResolveArgs {
    /// Oracle-market manifest produced by `dexdo oracle provision`.
    #[arg(long)]
    pub(crate) manifest: PathBuf,
    /// Signing key for the public `resolveRange` external message. The oracle key is conventional.
    #[arg(long)]
    pub(crate) oracle_key: PathBuf,
    /// Deployed-contracts manifest.
    #[arg(long, default_value = "contracts/deployed.shellnet.json")]
    pub(crate) contracts: PathBuf,
}
