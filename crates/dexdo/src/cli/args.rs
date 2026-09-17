//! `dexdo` CLI argument structs (clap-derived), split out of `main.rs` (PR3, move-only). The parser surface
//! for `seller`/`buyer`/`monitor`/`provision`/`destroy`/`recover`. Behavior-identical to the pre-split defs.

use anyhow::{bail, Result};
use clap::{Args, Subcommand, ValueEnum};
use dexdo_core::params::{
    CLI_POSITIVE_U64_MIN, DEFAULT_BUYER_MAX_TOKENS, DEFAULT_BUYER_TICKS,
    DEFAULT_CHAIN_READ_TIMEOUT_SECS, DEFAULT_CONTINUITY_MODE, DEFAULT_DASHBOARD_LISTEN,
    DEFAULT_EXECUTABLE_BOOK_TICKS, DEFAULT_EXPORT_FORMAT, DEFAULT_MARKETS_FRAME_MODEL,
    DEFAULT_MARKET_DATA_OUTPUT, DEFAULT_MARKET_DATA_TIMEOUT_MS,
    DEFAULT_MARKET_MANIFEST_OUTPUT_PATH, DEFAULT_MODELS_PATH, DEFAULT_MONITOR_TREE_WIDTH,
    DEFAULT_NOTE_INDEX, DEFAULT_ORACLE_EVENT_LIST_DESCRIPTION, DEFAULT_ORACLE_EVENT_LIST_INDEX,
    DEFAULT_ORACLE_FEE, DEFAULT_ORACLE_MARKET_OUTPUT_PATH, DEFAULT_ORACLE_PMP_DESCRIPTION,
    DEFAULT_POLICY_ROLE, DEFAULT_PROVISION_MAX_TICKS, DEFAULT_SELLER_GATEWAY_LISTEN,
    DEFAULT_SELLER_MOCK_TOKEN_COUNT, MARKET_DATA_DEPTH_LIMIT_MAX, MARKET_DATA_DEPTH_LIMIT_MIN,
    MARKET_DATA_LIST_LIMIT_MAX, MARKET_DATA_LIST_LIMIT_MIN, SHELL_CURRENCY_ID,
    SHELL_CURRENCY_LABEL,
};
use http::uri::Authority;
use std::fmt;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

fn parse_market_data_limit(s: &str) -> Result<u32, String> {
    parse_bounded_u32(
        s,
        MARKET_DATA_LIST_LIMIT_MIN,
        MARKET_DATA_LIST_LIMIT_MAX,
        "--limit",
    )
}

fn parse_market_data_depth_limit(s: &str) -> Result<u32, String> {
    parse_bounded_u32(
        s,
        MARKET_DATA_DEPTH_LIMIT_MIN,
        MARKET_DATA_DEPTH_LIMIT_MAX,
        "--limit",
    )
}

fn parse_positive_u64(s: &str) -> Result<u64, String> {
    let value = s
        .parse::<u64>()
        .map_err(|e| format!("expected positive integer: {e}"))?;
    if value < CLI_POSITIVE_U64_MIN {
        return Err("expected positive integer, got 0".to_string());
    }
    Ok(value)
}

fn parse_shell_currency_id(s: &str) -> Result<u32, String> {
    let value = s.parse::<u32>().map_err(|e| {
        format!("--token-type: expected SHELL currency id {SHELL_CURRENCY_ID}: {e}")
    })?;
    if value != SHELL_CURRENCY_ID {
        return Err(format!(
            "--token-type: dexdo markets support only SHELL currency id {SHELL_CURRENCY_ID}"
        ));
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

/// Bounded timeout for direct chain getter reads. This is intentionally separate from market-data's
/// indexer HTTP timeout: `market-data` is the fast indexer path and must not be wrapped here.
#[derive(Args, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChainReadTimeoutArgs {
    /// Bound the direct chain reads a command makes, ALL OF THEM TOGETHER, and return a
    /// retryable error instead of hanging.

    /// This is a budget for the command, not for each read. It used to be per read, so a
    /// command making five of them could take five times what was asked for; asks
    /// that a read-only command answer "within the bound", which a per-read bound never promised.
    /// Only time spent inside a chain read counts: waiting for a reconciliation, for a person, or
    /// for a write does not spend it.
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
    /// Mock model: fake tokens instead of a real upstream. Allowed with
    /// `--mock-chain` and on real shellnet, but refused on production mainnet.
    #[arg(long)]
    pub(crate) mock_model: bool,
    /// Mock chain: `MockChainBackend` instead of a real chain.
    #[arg(long)]
    pub(crate) mock_chain: bool,
}

impl MockFlags {
    /// deterministic fake output must never enter the production market for real SHELL.
    /// The real-shellnet stage remains intentional and supported.
    pub(crate) fn reject_mainnet_mock_model(&self, network: &str) -> Result<()> {
        if self.mock_model && !self.mock_chain && network == "mainnet" {
            bail!(
                "--mock-model cannot run against mainnet: it serves deterministic fake output \
                 into a real market for real SHELL; remove --mock-model, or point \
                 DEXDO_MANIFEST at shellnet for the mock-model + real-chain test stage"
            );
        }
        Ok(())
    }

    /// Buyer mock-demo: on a mock chain the upstream is also mock -- there is no real stream, so
    /// we require `--mock-model`. The chain is selected separately (`--mock-chain` -> mock, otherwise real
    /// the chain backend behind the feature) -- the former `bail!` about "" is removed (the real backend is available).
    pub(crate) fn require_mock_model(&self) -> Result<()> {
        if !self.mock_model {
            bail!("the buyer mock chain also requires --mock-model (,); the real path is without --mock-chain");
        }
        Ok(())
    }
}

/// Note identity -- common to all subcommands (`seller`/`buyer`/`monitor`).
/// Path to the root key; dexdo only **reads** it
/// and derives the note in memory. Same key -> same note -> continuity between runs.
#[derive(Args, Clone)]
pub(crate) struct IdentityArgs {
    /// Path to a file with the 32-byte hex secret of the note's root key -- the **persistent** identity
    /// . The root key is derivable: identity = the key AND the whole
    /// tree of (sub)notes under it. Without the flag -- an ephemeral note (a warning; mock-demo only,
    /// without continuity). An invalid/inaccessible path is an explicit failure, not a silent `generate()`.
    #[arg(long)]
    pub(crate) note_key: Option<PathBuf>,
    /// Index of the tree (sub)note this subcommand operates on: one root
    /// key -> many notes, a specific deal/order lives on a specific sub-note. Reproducible
    /// (same key+index -> same note). The monitor aggregates over the whole tree (`--tree-width`).
    #[arg(long, default_value_t = DEFAULT_NOTE_INDEX)]
    pub(crate) note_index: u32,
    /// **Real chain:** on-chain address of the actor's already **provisioned** `PrivateNote` (minted by
    /// `mint_pn_pool`). dexdo does NOT create the note -- it only reads/signs it
    /// with the owner key from `--note-key` (for the real path this is the SDK secret hex, not an HD seed). On the mock path
    /// (`--mock-chain`) it is ignored.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) note_addr: Option<String>,
}

/// Note identity for the pool-recovery commands (`reclaim`/`recover`/`dispute`). These act on a deal
/// already recorded on chain and in the pool, so they take the note **address** and its owner key and
/// nothing else -- there is no sub-note to derive and no `--note-index` to honour.
#[derive(Args)]
pub(crate) struct RecoveryIdentityArgs {
    /// Path to a file with the note's owner secret hex (the SDK secret of the on-chain `PrivateNote`).
    /// Optional: the recorded key next to the entry in `DEXDO_PN_POOL` is used when it is absent.
    #[arg(long)]
    pub(crate) note_key: Option<PathBuf>,
    /// On-chain address of the actor's already provisioned `PrivateNote`. Optional: without it the
    /// recorded pool metadata alone selects the deal(s).
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) note_addr: Option<String>,
}

/// Issue: explicit client-side ModelRegistry validation config.
#[derive(Args, Clone, Default)]
pub(crate) struct ModelRegistryValidationArgs {
    /// Strict JSON config with explicit seller/buyer check_model_registry booleans.
    #[arg(long)]
    pub(crate) model_registry_validation: Option<PathBuf>,
    /// Explicit ModelRegistry address override; requires --model-registry-validation.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
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
    #[arg(long, default_value = DEFAULT_SELLER_GATEWAY_LISTEN)]
    pub(crate) gateway_listen: SocketAddr,
    /// Public gateway host:port written to the buyer handover. Defaults to --gateway-listen, and on
    /// a real chain the resulting address must be reachable by a REMOTE buyer: a bind-all
    /// (`0.0.0.0`/`::`), loopback, RFC1918/ULA, link-local or CGNAT address is rejected before any
    /// offer is posted.
    #[arg(long, value_name = "HOST:PORT")]
    pub(crate) gateway_advertise: Option<GatewayAdvertiseAddr>,
    /// Issue: allow a non-routable `--gateway-advertise` (bind-all/loopback/private/link-local/CGNAT)
    /// for same-host or LAN testing. Buyers off this host will NOT be able to connect. `--mock-chain`
    /// demos never post to a real order book and are exempt without this flag.
    #[arg(long)]
    pub(crate) allow_private_advertise: bool,
    /// **Accepted and ignored.** Kept so existing scripts that pass it keep running; it does not
    /// gate the `advertised_gateway` self-probe or change seller behavior. Every self-probe failure
    /// is already fatal.
    #[arg(long)]
    pub(crate) require_advertise_probe: bool,
    /// Endpoints file -- the handover seam. Defaults to `<data-dir>/endpoints.json` when
    /// `--data-dir` is set, otherwise to the legacy platform data directory. This explicit flag
    /// overrides either default (D6, portability).
    #[arg(long)]
    pub(crate) endpoints_file: Option<PathBuf>,
    /// Local deal-handle directory. Defaults to `<data-dir>/deals` when selected, otherwise
    /// to the legacy platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
    /// Deal token_contract address. Optional if `--market` is given (loaded from the manifest).
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) token_contract: Option<String>,
    /// Issue: load the deal `token_contract` from a `dexdo provision` market manifest instead of
    /// passing `--token-contract` by hand. The seller still serves the model named by `--model`.
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// Review: deal nonce for the per-deal `TokenContract` (matches `--nonce` on `dexdo provision`).
    /// Required with an explicit `--token-contract` on a real chain -- the IOB rejects an offer whose
    /// `tokenContract` does not derive from `(sellerPubkey, nonce)`. With `--market` it is taken from
    /// the manifest, so do not pass both.
    #[arg(long)]
    pub(crate) nonce: Option<u64>,
    /// Offer this seller only to subscription BUYs. Ordinary sellers leave this off.
    #[arg(long)]
    pub(crate) subscription: bool,
    /// Tick price P in whole SHELL: `3` is three SHELL a tick. One SHELL is the book's price step,
    /// and a price that is not a whole multiple of it the book refuses.
    #[arg(long, default_value = "1", value_parser = crate::cli::args::parse_price_shell_u64)]
    pub(crate) price_per_tick: u64,
    /// How many fake tokens to serve in `--mock-model` mode. Real upstreams follow the request's
    /// `max_tokens`, capped by the market's `max_ticks * TICK_SIZE`.
    #[arg(long, default_value_t = DEFAULT_SELLER_MOCK_TOKEN_COUNT)]
    pub(crate) mock_token_count: u64,
    /// Model name from the config: a key or `frame_model`. Required on a real chain
    /// even with `--mock-model`, because it selects the on-chain `modelHash`; optional only for the
    /// `--mock-chain --mock-model` demo.
    #[arg(long)]
    pub(crate) model: Option<String>,
    /// List a model the on-chain `ModelRegistry` does not confirm, and go on when the registry cannot
    /// be read. Without it a real-chain seller refuses before it posts anything, the way
    /// `provision` and `deploy-market` already do -- this is the SAME flag those two carry, not a
    /// second one. It does not cover a spelling the registry holds differently: that is a confirmed
    /// model and a typo, and it is refused with the registered spelling named
    /// . No indexer lists a market raised under it.
    #[arg(long)]
    pub(crate) allow_unverified_model: bool,
    /// Path to the models config. Defaults to `<data-dir>/models.json` when
    /// selected, otherwise to `models.json` in the working directory.
    #[arg(long, default_value = DEFAULT_MODELS_PATH)]
    pub(crate) models: PathBuf,
    /// Persistent failure policy JSON. Defaults to `<data-dir>/policy.json` when selected, otherwise
    /// to the legacy XDG/Windows config path. Real seller startup fails closed if missing or incomplete.
    #[arg(long)]
    pub(crate) policy: Option<PathBuf>,
    /// Recovery escape hatch for a retained publication marker.  The value must
    /// exactly name the selected TokenContract and is accepted only with the
    /// separate confirmation flag; service/timer units must never set it.
    #[arg(long, value_name = "TOKEN_CONTRACT")]
    pub(crate) recover_publication: Option<String>,
    /// Acknowledge that `--recover-publication` creates an immutable audit and
    /// permits exactly one fresh explicit seller post after exact-negative proof.
    #[arg(long, requires = "recover_publication")]
    pub(crate) confirm_recover_publication: bool,
}

impl SellerArgs {
    pub(crate) fn gateway_advertise_addr(&self) -> String {
        self.gateway_advertise
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| self.gateway_listen.to_string())
    }

    /// Issue: the advertised address a remote buyer will dial, validated fail-closed BEFORE any
    /// chain work or order placement. `--mock-chain` has no real order book and no remote buyer, so
    /// the local demo addresses stay usable there.
    pub(crate) fn checked_gateway_advertise_addr(&self) -> Result<String> {
        let advertised = self.gateway_advertise_addr();
        if !self.mock.mock_chain {
            if self
                .gateway_advertise
                .as_ref()
                .and_then(|address| address.0.port_u16())
                == Some(0)
            {
                return Err(dexdo_core::DexdoError::new(
                    dexdo_core::error_codes::E_ADVERTISE_NOT_PUBLIC,
                    format!(
                        "--gateway-advertise {advertised} uses port 0, which no remote buyer can dial"
                    ),
                )
                .with_hint(dexdo_core::error_codes::E_ADVERTISE_NOT_PUBLIC.fix())
                .into());
            }
            dexdo::seller::advertise::validate_advertise(
                &advertised,
                self.gateway_advertise.is_none(),
                self.allow_private_advertise,
            )?;
        }
        Ok(advertised)
    }

    /// Issue: the advertise inherited from `--gateway-listen <host>:0` still carries the
    /// placeholder port `0`, because it is resolved at the command boundary, before the gateway has
    /// bound anything. Port `0` means "ask the OS for a free port" -- it is not an address, and no
    /// buyer (nor the self-probe) can ever dial it. Once the listener exists, its real port is the
    /// address this seller is actually reachable on, so adopt it.

    /// Only an INHERITED `:0` is rewritten. An explicit `--gateway-advertise` is the operator's own
    /// word about what the world can reach, and is never rewritten behind their back -- nor is a
    /// listen address that named a real port.
    pub(crate) fn bound_gateway_advertise(&self, advertised: String, bound: SocketAddr) -> String {
        if self.gateway_advertise.is_some() || bound.port() == 0 {
            return advertised;
        }
        match advertised.parse::<SocketAddr>() {
            Ok(requested) if requested.port() == 0 && requested.ip() == bound.ip() => {
                bound.to_string()
            }
            _ => advertised,
        }
    }

    /// Issue: `--require-advertise-probe` restores the pre- hard fail on any self-probe error.
    pub(crate) fn advertise_probe_policy(&self) -> dexdo::seller::liveness::AdvertiseProbePolicy {
        if self.require_advertise_probe {
            dexdo::seller::liveness::AdvertiseProbePolicy::Required
        } else {
            dexdo::seller::liveness::AdvertiseProbePolicy::TolerateTunneledTransportFailure
        }
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
    /// Endpoints file -- where to read the enc-endpoint from. Defaults to
    /// `<data-dir>/endpoints.json` when selected, otherwise to the legacy platform data directory;
    /// `--endpoints-file` overrides either default (D6).
    #[arg(long)]
    pub(crate) endpoints_file: Option<PathBuf>,
    /// Local deal-handle directory. Defaults to `<data-dir>/deals` when selected, otherwise
    /// to the legacy platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
    /// Deal token_contract address. Optional if `--market` is given (loaded from the manifest).
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) token_contract: Option<String>,
    /// **Resume/connect** to an ALREADY-matched deal without placing a new buy -- read the on-chain handover and
    /// stream/serve. Use it after a `buyer` timed out awaiting the seller's `open_stream`, once the seller has
    /// opened it: the escrow is already committed, so a fresh buy would double-pay. Works BOTH model-only
    /// (default: the deal `TokenContract` is recovered from THIS note's own `InferenceFilledConfirmed` event --
    /// no hand-pasted address) and with an explicit `--token-contract`/`--market`.
    #[arg(long)]
    pub(crate) resume: bool,
    /// **Planned restart**. The exit half of `--resume`: on a graceful shutdown of a
    /// `--local-listen` buyer, leave this ordinary deal OPEN on chain instead of submitting the
    /// terminal STOP, so the next process picks it back up with `--resume` -- same seller, same
    /// clearing price, same remaining capacity -- instead of paying the book fee for a second BUY at
    /// whatever the price is by then. A durable subscription is preserved on exit already; this is
    /// the same `SessionLifetimePolicy::Preserve`, asked for explicitly.

    /// OFF by default, and deliberately: an UNPLANNED exit -- a crash, a panic, a SIGKILL -- must keep
    /// today's settle-on-exit behavior, so a process that dies without an operator behind it does not
    /// strand a deal nobody is coming back for. Preserving is the reversible side (the deal is still
    /// there to settle later, with `dexdo close` or a subsequent run without this flag); a STOP ends
    /// in `_payOwedAndDie()` and there is no deal left to resume.

    /// Requires `--local-listen`: a one-shot buyer has nothing to restart into, so leaving its deal
    /// open would only be a way to receive a stream and skip the terminal that pays for it.
    #[arg(long)]
    pub(crate) preserve_deal_on_exit: bool,
    /// Let the whole buy wait in the model book when no seller ask is resting yet. Without this option
    /// the whole buy either fills immediately from one seller or is rejected and refunded.
    #[arg(long)]
    pub(crate) wait_for_seller: bool,
    /// Issue: load `token_contract` + `frame_model` from a `dexdo provision` market manifest
    /// instead of passing them by hand.
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// One-shot buyer receive cap. In local consumer-API mode, per-request `max_tokens` is honored and capped by
    /// the purchased deal budget (`--ticks x TICK_SIZE`) instead of this debug/one-shot cap.
    #[arg(long, default_value_t = DEFAULT_BUYER_MAX_TOKENS)]
    pub(crate) max_tokens: u64,
    /// Local bind/listen address for the OpenAI-compatible consumer endpoint; buyer-side equivalent
    /// of seller --gateway-listen.

    /// A real buy defaults it to 127.0.0.1:8080, because it has nowhere else to send its prompts and
    /// the operator was typing the same loopback address every time to say "yes, the ordinary one".
    /// The default is applied by the buyer rather than here: on `--mock-chain` the absence of an
    /// endpoint is itself the answer -- it selects the one-shot path, which only drives a
    /// `--mock-model` seller -- and a parser default would leave nothing able to say it.
    #[arg(long)]
    pub(crate) local_listen: Option<SocketAddr>,
    /// Continuity mode for --local-listen. proactive keeps a warm next deal ready after idle close,
    /// low remaining budget, or no current deal; it may pre-buy while idle and spend the documented probe/idle
    /// cost. on-demand buys only after active/recent consumer traffic; it avoids idle spend, but the first
    /// request after idle may wait for a fresh deal.
    #[arg(long, value_enum, default_value = DEFAULT_CONTINUITY_MODE, value_name = "MODE")]
    pub(crate) continuity_mode: ContinuityModeArg,
    /// Emit machine-readable JSONL lifecycle events on stdout.
    #[arg(long)]
    pub(crate) json: bool,
    /// Additionally bring up an Anthropic-compatible `/v1/messages` via transcoding (B20).
    #[arg(long)]
    pub(crate) anthropic_compat: bool,
    /// Model id of the configured frame/market (B2/B19) -- the only served model. Alias: --model.
    /// **Required** (review Y2): it used to silently default to `dexdo-mock`, which on a real deal made
    /// B7 verify substitution against the wrong model (a silent no-op / false positive). Now the operator
    /// sets it explicitly (for mock-demo -- `--frame-model dexdo-mock`). Optional if `--market` is given
    /// (loaded from the manifest). Horizon: derive it from the deal's per-model `InferenceOrderBook`.
    #[arg(long, visible_alias = "model")]
    pub(crate) frame_model: Option<String>,
    /// Accept a model without an executable exact-model buyer reference on NAME-only evidence. Existing B8/B7
    /// checks still run when their data is available; this flag only opts out of the reference-availability
    /// preflight.
    #[arg(long)]
    pub(crate) allow_unverified_model: bool,
    /// path to the models config (JSON) providing the **per-model verification data** (B5 vocab, B8
    /// fingerprints, B7-full reference via base_url/served_model/api_key_env). Defaults to
    /// `<data-dir>/models.json` when selected, otherwise to `models.json` in the working directory. A model
    /// absent from this config has no verification data -> the buyer fails closed (unless
    /// `--allow-unverified-model`).
    #[arg(long, default_value = DEFAULT_MODELS_PATH)]
    pub(crate) models: PathBuf,
    /// How many ticks the buyer purchases. Not used on the mock path.
    #[arg(long, default_value_t = DEFAULT_BUYER_TICKS)]
    pub(crate) ticks: u128,
    /// **Real chain:** the per-tick price LIMIT (`maxPricePerTick`) in whole SHELL -- `3`
    /// is three SHELL a tick -- crossing with an ask at or below it.
    /// Must be >= ask. Book deposit check: `escrow >= ticks x maxPricePerTick x (1 + 2.5 %
    /// fee)` -- the fee is charged ON TOP of the limit, so `escrow = limit x ticks` (without headroom) does
    /// not pass; keep the limit noticeably below `escrow / (ticks x 1.025)`.
    #[arg(long, default_value = "1", value_parser = crate::cli::args::parse_price_shell)]
    pub(crate) max_price_per_tick: u128,
    /// **Real chain:** escrow (the note's `getDetails().balance[2]` SHELL) for `placeInferenceBuy`. Omit it to use EXACTLY
    /// the required `ticks x max_price_per_tick x (1 + 2.5 % book fee)`. An
    /// explicit value must equal `required`: under-funding orphans the escrow; over-funding strands the
    /// surplus when the buy rests and is filled as a maker. Without `--mock-chain`.
    #[arg(long, value_parser = crate::cli::args::parse_shell_amount)]
    pub(crate) escrow: Option<u128>,
    /// Persistent failure policy JSON. Defaults to `<data-dir>/policy.json` when selected, otherwise
    /// to the legacy XDG/Windows config path. Real buyer startup fails closed if missing or incomplete.
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
    /// Endpoints/mock-chain-state file (the same one as `seller`/`buyer`). Default -- the platform path.
    #[arg(long)]
    pub(crate) endpoints_file: Option<PathBuf>,
    /// How many tree (sub)notes to poll (, R14: the monitor aggregates over ALL notes under
    /// the key, not just the root). The window `index = 0..width` is reproducible from the key. `--note-index`
    /// is ignored by the monitor (it sees the whole tree, not a single sub-note).
    #[arg(long, default_value_t = DEFAULT_MONITOR_TREE_WIDTH)]
    pub(crate) tree_width: u32,
    /// **Real chain:** the operator's market manifest(s) from `dexdo provision` -- the monitor
    /// reads each market's `TokenContract` by-fact state on-chain (a `RealNote` is a single key, not an HD tree,
    /// so the real monitor reads the markets it is given, not a `--tree-width` window). Repeat for many markets.
    #[arg(long)]
    pub(crate) market: Vec<PathBuf>,
}

/// Doctor / health check: read-only chain version and manifest freshness guard.
#[derive(Args)]
pub(crate) struct DoctorArgs {
    /// Emit one machine-readable health report instead of human output.
    #[arg(long)]
    pub(crate) json: bool,
    /// Optional `dexdo provision` market manifest. When supplied, doctor also verifies the market IOB/TC are
    /// active and carry the expected code hash.
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// Persistent failure policy JSON to inspect. Defaults to the platform config path.
    #[arg(long)]
    pub(crate) policy: Option<PathBuf>,
}

/// Buyer-side, money-free seller gateway reachability check.
#[derive(Args)]
pub(crate) struct GatewayCheckArgs {
    /// Decrypted seller gateway endpoint from the handover (`https://host:port`).
    #[arg(long)]
    pub(crate) endpoint: String,
    /// SHA-256 TLS certificate fingerprint from the same decrypted handover.
    #[arg(long)]
    pub(crate) tls_fingerprint: String,
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
    /// Validate a seller policy using the same local capability check as seller/provision.
    Validate(PolicyValidateArgs),
}

#[derive(Args)]
pub(crate) struct PolicyInitArgs {
    /// Which role section to scaffold.
    #[arg(long, value_enum, default_value = DEFAULT_POLICY_ROLE)]
    pub(crate) role: PolicyRoleArg,
    /// Policy file path. Defaults to `<data-dir>/policy.json` when selected, otherwise to the legacy
    /// platform config path.
    #[arg(long)]
    pub(crate) path: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct PolicyPathArgs {
    /// Policy file path. Defaults to `<data-dir>/policy.json` when selected, otherwise to the legacy
    /// platform config path.
    #[arg(long)]
    pub(crate) path: Option<PathBuf>,
}

#[derive(Args)]
pub(crate) struct PolicyValidateArgs {
    /// Runtime role whose policy must be validated.
    #[arg(long, value_enum)]
    pub(crate) role: PolicyValidateRoleArg,
    /// Policy file path. Defaults to `<data-dir>/policy.json` when selected, otherwise to the legacy
    /// platform config path.
    #[arg(long)]
    pub(crate) path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum PolicyRoleArg {
    Buyer,
    Seller,
    Both,
}

/// Whose rules are being checked. Both roles have a runtime policy and both are refused at startup
/// when theirs is incomplete, so both can be checked before that happens -- `policy init` has always
/// been able to scaffold either.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum PolicyValidateRoleArg {
    Buyer,
    Seller,
}

/// How `provision` obtains the per-deal nonce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProvisionNonceArg {
    Auto,
    Explicit(u64),
}

impl FromStr for ProvisionNonceArg {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        value.parse::<u64>().map(Self::Explicit).map_err(|_| {
            format!("invalid nonce {value:?}: expected `auto` or an unsigned 64-bit integer")
        })
    }
}

/// Provision arguments: bring up a per-deal market from the seller note alone.
#[derive(Args)]
pub(crate) struct ProvisionArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    /// Frame model id the market serves (determines the on-chain `modelHash`).
    #[arg(long)]
    pub(crate) frame_model: String,
    /// provision a market for a model name that does not resolve in the ModelRegistry. Off by
    /// default, matching the buyer: the buyer's content-identity resolution is mandatory, so a name
    /// that fails it can never be bought and the order book, RootModel and TokenContract deployed for
    /// it are paid for and can never trade. Same flag, same meaning, same opt-out as `dexdo buyer`.
    #[arg(long)]
    pub(crate) allow_unverified_model: bool,
    /// Deal nonce (required). For a normal new market, use `auto` (recommended): it chooses and
    /// verifies a random 64-bit value. Use an explicit unsigned integer only for reproducible or
    /// operator-controlled provisioning. The per-deal `TokenContract` derives from
    /// `(sellerPubkey, nonce)`, so every selected value must be unique for that seller.
    #[arg(long)]
    pub(crate) nonce: Option<ProvisionNonceArg>,
    /// Tick price P in whole SHELL: `3` is three SHELL a tick. One SHELL is the book's price step,
    /// and a price that is not a whole multiple of it the book refuses.
    #[arg(long, default_value = "1", value_parser = crate::cli::args::parse_price_shell)]
    pub(crate) price_per_tick: u128,
    /// Max ticks the deal `TokenContract` bounds to.
    #[arg(long, default_value_t = DEFAULT_PROVISION_MAX_TICKS)]
    pub(crate) max_ticks: u128,
    /// Note ECC[2] allocation in whole SHELL, not raw nano/vmshell (1 SHELL = 1e9 raw).
    /// The per-deal `TokenContract`'s ECC[2] RESERVE, sent with `PrivateNote.deployDeal` (4.0.36).
    /// Every entry into the deal burns its measured charge out of it, starting with the constructor;
    /// the `RootModel` is deployed by `SuperRoot`, which carries its own value.
    /// Only two calls put ECC[2] there: `deployDeal` seeds it, and `PrivateNote.fundDeployShell`
    /// tops it up afterwards -- reachable by the SELLER note alone. A buyer whose deal runs dry
    /// cannot refill it and loses `stop`/`dispute`/`cleanupUnopened`, so size it here.
    /// Unused remainder burns at `destroy`.
    /// Fail-closed: below-floor / overflow is rejected, not clamped. Absent on an interactive terminal prompts.
    #[arg(long)]
    pub(crate) deposit_shells: Option<u128>,
    /// Output path for the produced market manifest (OB/RootModel + the deployed TC address). `dexdo
    /// seller`/`buyer` load it via `--market`.
    #[arg(long, default_value = DEFAULT_MARKET_MANIFEST_OUTPUT_PATH)]
    pub(crate) output: PathBuf,
    /// Persistent seller failure policy JSON. Defaults to the same platform path as `dexdo seller`.
    /// Provisioning fails locally before reading secrets or connecting to the chain if it is unsupported.
    #[arg(long)]
    pub(crate) policy: Option<PathBuf>,
}

/// Args for `dexdo destroy`: the seller CLOSES a STOPped deal's `TokenContract` (DESTRUCTIVE -- the held
/// ~a-few-vmshell leftover burns cross-dapp; negligible at the right-sized ~10/deploy funding,).
#[derive(Args)]
pub(crate) struct DestroyArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// The deal `TokenContract` to destroy (or pass `--market`).
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) token_contract: Option<String>,
    /// A `dexdo provision` market manifest carrying the `token_contract` (alternative to `--token-contract`).
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// **Accepted and ignored.** Kept so existing scripts that pass it keep running; it does not gate
    /// `destroy` or change its behavior. `destroy` still `selfdestruct`s the TC, and the held leftover
    /// burns cross-dapp rather than being credited back to the note.
    #[arg(long)]
    pub(crate) acknowledge_burn: bool,
}

/// Args for `dexdo recover`: the BUYER STOPs an orphaned OPEN deal from its note WITHOUT placing a
/// new buy, so a stuck deal (the buyer process died) can be closed and the seller can then `destroy` it.
#[derive(Args)]
pub(crate) struct RecoverArgs {
    #[command(flatten)]
    pub(crate) identity: RecoveryIdentityArgs,
    /// The OPEN deal `TokenContract` to STOP (or pass `--market`).
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) token_contract: Option<String>,
    /// A `dexdo provision` market manifest carrying the `token_contract` (alternative to `--token-contract`).
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// DEXDO_PN_POOL fallback carrying the note owner key and last matched TokenContract. Defaults to env DEXDO_PN_POOL.
    #[arg(long)]
    pub(crate) pool: Option<PathBuf>,
}

/// Args for `dexdo dispute`: the BUYER opens an on-chain dispute on an OPEN deal -- `streamDispute` ->
/// `TC.dispute()` freezes this TC's contested amount and seller bond until resolution. The anti-scam lever for an
/// observed substitution/fraud -- strictly stronger than `recover`'s STOP (which still pays for ticks).
#[derive(Args)]
pub(crate) struct DisputeArgs {
    #[command(flatten)]
    pub(crate) identity: RecoveryIdentityArgs,
    /// The OPEN deal `TokenContract` to dispute (or pass `--market`).
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) token_contract: Option<String>,
    /// A `dexdo provision` market manifest carrying the `token_contract` (alternative to `--token-contract`).
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// DEXDO_PN_POOL fallback carrying the note owner key and last matched TokenContract. Defaults to env DEXDO_PN_POOL.
    #[arg(long)]
    pub(crate) pool: Option<PathBuf>,
}

/// Args for `dexdo reclaim`: the BUYER reclaims escrow on seller no-show. OPEN deals use
/// the explicit `close`/`recover` STOP path; funded-but-never-opened deals use `streamCleanup`
/// after `MATCH_OPEN_TIMEOUT`. Fails closed locally on ownership, state, and the cleanup timer.
/// driven from a pool alone it reclaims **every** recorded deal, one at a time.
#[derive(Args)]
pub(crate) struct ReclaimArgs {
    #[command(flatten)]
    pub(crate) identity: RecoveryIdentityArgs,
    /// The funded/OPEN deal `TokenContract` to reclaim (or pass `--market`).
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) token_contract: Option<String>,
    /// A `dexdo provision` market manifest carrying the `token_contract` (alternative to `--token-contract`).
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// DEXDO_PN_POOL carrying each note's owner key and the TokenContract recorded for it. Without
    /// `--note-addr`/`--token-contract` this drives EVERY recorded recovery entry as its own reclaim --
    /// one money move per still-reclaimable deal -- and refuses contradictory records. Defaults to env
    /// DEXDO_PN_POOL.
    #[arg(long)]
    pub(crate) pool: Option<PathBuf>,
}

/// Args for `dexdo release-dispute`: the SELLER concedes an on-chain dispute on its deal TC,
/// returning the contested amount to the buyer and the seller bond.
#[derive(Args)]
pub(crate) struct ReleaseDisputeArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// The disputed deal `TokenContract` to release (or pass `--market`).
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) token_contract: Option<String>,
    /// A `dexdo provision` market manifest carrying the `token_contract` (alternative to `--token-contract`).
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
}

/// Permissionless resolution of a disputed TokenContract after its deployed dispute window.
#[derive(Args)]
pub(crate) struct ResolveDisputeTimeoutArgs {
    /// The disputed deal `TokenContract` to resolve (or pass `--market`).
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) token_contract: Option<String>,
    /// A `dexdo provision` market manifest carrying the `token_contract`.
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
}

/// Args for `dexdo withdraw-shell`: the SELLER withdraws finalized `_finalizedOwed` SHELL from a deal TC.
#[derive(Args)]
pub(crate) struct WithdrawShellArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// The deal `TokenContract` to withdraw from (or pass `--market`).
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) token_contract: Option<String>,
    /// A `dexdo provision` market manifest carrying the `token_contract` (alternative to `--token-contract`).
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// Amount to withdraw, in SHELL. If omitted, withdraws all current `finalizedOwed`.
    #[arg(long, value_parser = crate::cli::args::parse_shell_amount)]
    pub(crate) amount: Option<u128>,
}

/// Read-only chain/indexer receipt for one inference deal TokenContract.
#[derive(Args)]
pub(crate) struct SettlementReceiptArgs {
    /// TokenContract whose chain getters and ext-out history are read.
    #[arg(value_name = "TOKEN_CONTRACT", value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) token_contract: String,
    /// Rewards season to include in the independent note-aggregate join instructions.
    #[arg(long, value_name = "N")]
    pub(crate) season: Option<u32>,
    /// Emit the stable `dexdo.settlement-receipt.v1` JSON object.
    #[arg(long, required = true)]
    pub(crate) json: bool,
    /// Exit non-zero unless this deal's money conservation is PROVEN. Off by default.

    /// What each verdict of `conservation.status` means for the exit code, stated here in full so
    /// that nobody has to infer it from the name:

    /// `conserved` -- what was funded in equals what was credited to the notes plus what was
    /// written off, checked against the notes as a second account. Exit code 0.

    /// `unbalanced` -- the identity does not hold, or the deal's own terminal split disagrees with
    /// what the notes were credited. Exit code non-zero.

    /// `incomplete` -- a term of the identity could not be read, so conservation was never
    /// evaluated. Exit code non-zero, because "could not check" is not "checked and fine".

    /// `unverified` -- the identity had only ONE account's word for both of its sides, so it closed
    /// by construction and states nothing about the money. Exit code non-zero: a number compared
    /// with itself is not evidence.

    /// No conservation block at all -- the chain read was unavailable, so this receipt carries no
    /// money identity to judge. Exit code non-zero, for the same reason.

    /// WITHOUT this flag the command never fails on any verdict, only on a read, parse or endpoint
    /// error. That is a deliberate decision, not an oversight: this receipt is a stable
    /// reporting object, and its callers need the JSON most when a deal is inconsistent.

    /// The receipt is printed in full BEFORE this flag decides the exit code, so a caller that asks
    /// for the gate still gets the data.

    /// On the failing path stdout therefore carries TWO JSON documents -- this receipt, then a
    /// `dexdo.error.v1` envelope -- so read stdout as a stream of values, not as one object.
    #[arg(long)]
    pub(crate) require_conserved: bool,
}

/// Deploy the per-model `InferenceOrderBook` (the shared market for a model) if it is not yet on-chain --
/// note-funded, a separate operate step the seller runs before posting offers. The book address is
/// deterministic from the model's `model_hash`, so the deploy is idempotent (already-deployed -> no-op).

/// **The note is named by its address and by nothing else.** This command does not take
/// `IdentityArgs`, and that is the point: that group carries `--note-key` and `--note-index`, and
/// neither selects a note. A key SIGNS for a note -- offering it as a second way to name one gave a
/// run where the operator picked pool note A from the menu and the key file signed for B, which the
/// contract's `onlyOwnerPubkey` rejects after the deploy gas is spent. `--note-index` is for the
/// HD-derived sub-note tree and this command has no tree to walk.

/// The owner key is FOUND, not passed: by the note's address, in the pool of the data directory
/// this run was given -- the same pool `dexdo note deploy` wrote it into.
#[derive(Args)]
pub(crate) struct MarketDeployArgs {
    /// On-chain address of the `PrivateNote` this deploy is funded from and signed by. The only way
    /// to name the note. Its owner key comes from the pool entry recorded under this address.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) note_addr: Option<String>,
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    /// Frame model id whose order book to deploy (its `model_hash` derives the book address).
    /// The exact name the on-chain ModelRegistry carries -- the client applies no grammar of its own
    /// to it, and the book address is `sha256` of these bytes.
    #[arg(long)]
    pub(crate) frame_model: String,
    /// deploy a book for a model name that does not resolve in the ModelRegistry. Off by
    /// default, and the same flag, meaning and opt-out `provision` and `buyer` carry. A name the
    /// buyer's mandatory resolution rejects gives a book no buyer can ever reach, paid for out of
    /// the note.
    #[arg(long)]
    pub(crate) allow_unverified_model: bool,
}

/// `dexdo model-registry`: read the on-chain registry out and write it down.

/// No `--models` and no `--note-addr`: this command asks the REGISTRY what it holds, not the
/// operator's file and not a note's order books. The registry address comes from the manifest
/// unless `--model-registry-address` names another one.
#[derive(Args, Clone)]
pub(crate) struct ModelRegistryArgs {
    /// Write the registry to this file, as `dexdo.model_registry.v1` JSON.
    #[arg(long)]
    pub(crate) output: Option<PathBuf>,
    /// Emit the stable `dexdo.model_registry.v1` object on stdout instead of the human summary.
    #[arg(long)]
    pub(crate) json: bool,
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    /// Bound on the chain read, per: this command downloads a whole account, and an
    /// unbounded read is how a flaky node turns a two-second command into an open-ended wait.
    #[command(flatten)]
    pub(crate) read_timeout: ChainReadTimeoutArgs,
}

/// Read-only market discovery: list configured/per-manifest inference order books.
#[derive(Args)]
pub(crate) struct MarketsArgs {
    /// Pointed read-only questions about one market that need no note.
    #[command(subcommand)]
    pub(crate) command: Option<MarketsCommand>,
    /// Emit the stable `dexdo.markets.v2` JSON object.
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
    #[arg(long, default_value = DEFAULT_MARKETS_FRAME_MODEL)]
    pub(crate) frame_model: String,
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    /// Optional provisioned market manifest(s). If absent, markets are derived from --models via --note-addr.
    #[arg(long)]
    pub(crate) market: Vec<PathBuf>,
    /// Models config used when --market is absent.
    #[arg(long, default_value = DEFAULT_MODELS_PATH)]
    pub(crate) models: PathBuf,
    /// Any active inference PrivateNote address used to derive per-model order-book addresses when --market is absent.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) note_addr: Option<String>,
}

#[derive(Subcommand)]
pub(crate) enum MarketsCommand {
    /// Name the canonical order-book address for ONE model. Read-only and note-free.
    Address(MarketsAddressArgs),
}

/// Name one model's canonical `InferenceOrderBook` address.

/// The listing form of `markets` derives book addresses by running the note's
/// `getInferenceOrderBookAddress` getter, so it refuses without `--note-addr` and refuses again if
/// that note is not live. Neither fact is about the market: the book address is `tvm.hash` of a
/// StateInit built from the pinned `InferenceOrderBook` code and the model hash, and the client
/// carries that code image itself. This asks the question on its own, so no note has to exist for
/// an operator to be told where a market lives.
#[derive(Args)]
pub(crate) struct MarketsAddressArgs {
    /// Model to name the order book for, in any spelling ModelRegistry accepts.
    #[arg(long)]
    pub(crate) model: String,
    #[command(flatten)]
    pub(crate) read_timeout: ChainReadTimeoutArgs,
    /// Emit the stable `dexdo.markets_address.v1` JSON object.
    #[arg(long)]
    pub(crate) json: bool,
}

/// One model's order book as the human-readable box table: the same view the buyer shows before a
/// buy -- #/price-per-tick/max-ticks + full tokenContract addresses. Read-only, keyed by the canonical model.
#[derive(Args)]
pub(crate) struct MarketArgs {
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    #[command(flatten)]
    pub(crate) read_timeout: ChainReadTimeoutArgs,
    /// Model id whose book to render. The client applies no grammar of its own to it.

    /// Resolved against the on-chain ModelRegistry, with no local file required. A
    /// `models.json` is consulted only as a source of your own nicknames: a name it does not know
    /// is the model itself, and where it maps a name elsewhere the registry decides.
    pub(crate) model: String,
    /// Any active inference PrivateNote address used to derive the per-model order-book address.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) note_addr: Option<String>,
    /// Optional provisioned market manifest. If given, the book is read from it instead of `--note-addr`.
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// Models config (used to accept a model KEY as well as a raw canonical frame_model).
    #[arg(long, default_value = DEFAULT_MODELS_PATH)]
    pub(crate) models: PathBuf,
}

/// Read buyer-executable asks for one model book at a concrete tick count and price ceiling.
#[derive(Args)]
pub(crate) struct ExecutableBookArgs {
    #[command(flatten)]
    pub(crate) registry: ModelRegistryValidationArgs,
    #[command(flatten)]
    pub(crate) read_timeout: ChainReadTimeoutArgs,
    /// Model id whose book to inspect. The client applies no grammar of its own to it.

    /// Resolved against the on-chain ModelRegistry, with no local file required. A
    /// `models.json` is consulted only as a source of your own nicknames: a name it does not know
    /// is the model itself, and where it maps a name elsewhere the registry decides.
    pub(crate) model: String,
    /// Any active inference PrivateNote address used to derive the per-model order-book address.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) note_addr: Option<String>,
    /// Optional provisioned market manifest. If given, the book is read from it instead of `--note-addr`.
    #[arg(long)]
    pub(crate) market: Option<PathBuf>,
    /// Models config (used to accept a model KEY as well as a raw canonical frame_model).
    #[arg(long, default_value = DEFAULT_MODELS_PATH)]
    pub(crate) models: PathBuf,
    /// Desired buyer ticks for the executable row filter.
    #[arg(long, default_value_t = DEFAULT_EXECUTABLE_BOOK_TICKS)]
    pub(crate) ticks: u128,
    /// Buyer price ceiling in whole SHELL, the same figure `--max-price-per-tick` takes on
    /// `dexdo buyer`.
    #[arg(long, default_value = "1", value_parser = crate::cli::args::parse_price_shell)]
    pub(crate) max_price_per_tick: u128,
}

/// Executable quote over current order-book depth.
#[derive(Args)]
pub(crate) struct QuoteArgs {
    /// Emit the stable `dexdo.quote.v2` JSON object.
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
    #[arg(long, default_value = DEFAULT_MODELS_PATH)]
    pub(crate) models: PathBuf,
    /// Any active inference PrivateNote address used to derive the order-book address when --market is absent.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) note_addr: Option<String>,
    /// Desired ticks. Mutually exclusive with --budget.
    #[arg(long)]
    pub(crate) ticks: Option<u128>,
    /// Fee-inclusive budget, in SHELL. Mutually exclusive with --ticks.
    #[arg(long, value_parser = crate::cli::args::parse_shell_amount)]
    pub(crate) budget: Option<u128>,
}

/// Read-only Dodex inference market-data indexer discovery.
#[derive(Args)]
pub(crate) struct MarketDataArgs {
    /// Dodex indexer base URL. Env fallback: DEXDO_INDEXER_URL. Otherwise the manifest's own
    /// `indexer` field, then the indexer of the network the manifest declares.
    #[arg(long, global = true)]
    pub(crate) indexer_url: Option<String>,
    /// Output format for scripts/operators.
    #[arg(long, value_enum, default_value = DEFAULT_MARKET_DATA_OUTPUT, global = true)]
    pub(crate) output: MarketDataOutput,
    /// HTTP request timeout in milliseconds.
    #[arg(long, default_value_t = DEFAULT_MARKET_DATA_TIMEOUT_MS, global = true)]
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
        /// InferenceOrderBook address, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`.
        #[arg(value_parser = dexdo_core::address::arg_to_chain_param)]
        inference_order_book_address: String,
    },
    /// Read depth for one inference model order book.
    Depth {
        /// InferenceOrderBook address, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`.
        #[arg(value_parser = dexdo_core::address::arg_to_chain_param)]
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
    /// The model's on-chain id, `0x` + 64 hex, when neither the name nor a market file is at hand.

    /// the route for an owner holding ONLY the note and its key. `note outstanding` recovers
    /// a resting order as `modelHash` + `orderId` from the note's own inbound history, because
    /// `modelHash` is the one identifier that survives there -- the name does not, and `sha256(name)`
    /// does not run backwards. Everything past the input already works on the hash: the book is
    /// derived from it and `cancelInferenceOrder` takes it. Only the way in was missing.
    #[arg(long, conflicts_with_all = ["market", "model"], value_name = "0xHASH")]
    pub(crate) model_hash: Option<String>,
    /// Models config used with --model.
    #[arg(long, default_value = DEFAULT_MODELS_PATH)]
    pub(crate) models: PathBuf,
    #[command(subcommand)]
    pub(crate) command: OrdersCommand,
}

#[derive(Subcommand)]
pub(crate) enum OrdersCommand {
    /// List this note's resting orders in the model book.
    List,
    /// Show the durable BUY submit journal and close it only on a chain-proven terminal outcome.
    Journal,
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
    /// Sweep one of this note's resting orders that is already past its own deadline.

    /// The book's `expireOrder` is permissionless and idempotent, so this signs nothing and is
    /// safe to repeat; the refund it releases is reported only once it has been observed.
    Expire {
        /// On-chain order id.
        order_id: u128,
    },
    /// Cancel all of this note's resting orders in the model book.
    CancelAll,
    /// Find the deals the book says this note was matched into, when the client that was running
    /// them is gone.

    /// The resting-order views cannot answer this: a matched order has left the book, and the
    /// note's own fill mirror is best-effort, so a crash can leave a funded deal with no local
    /// record of who it was with. This walks the book's own `InferenceFilled` history for this
    /// note, then re-checks every deal it names against that TokenContract's `getParties` and
    /// `getState`. It reports candidates and refusals, never a recovery. Read-only; signs nothing.
    Fills,
}

/// Pre-match lifecycle for one single-seller take-or-pay subscription BUY.
#[derive(Args)]
pub(crate) struct SubscriptionArgs {
    /// Emit the stable `dexdo.subscription.v2` JSON object instead of the human receipt line.
    /// Global so it reads the same before or after the subcommand, like `--note-key` on `place`.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(flatten)]
    pub(crate) mock: MockFlags,
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// Local deal-handle directory. The matched subscription is persisted here before its fill
    /// cursor advances, so `dexdo buyer --resume` can reuse the same deterministic buyer handle.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
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
    #[arg(long, default_value = DEFAULT_MODELS_PATH)]
    pub(crate) models: PathBuf,
    /// Endpoints/mock-chain-state file used by the explicit mock path.
    #[arg(long)]
    pub(crate) endpoints_file: Option<PathBuf>,
    #[command(subcommand)]
    pub(crate) command: SubscriptionCommand,
}

#[derive(Subcommand)]
pub(crate) enum SubscriptionCommand {
    /// Place one ordinary AON+SUBSCRIPTION BUY with a finite deadline and exact escrow.
    Place(SubscriptionPlaceArgs),
    /// Show one owned subscription BUY while it rests, or its durable matched TC fact.
    Status {
        /// Ordinary InferenceOrderBook order id.
        order_id: u128,
        /// Book the due subscription week if one is due.: WITHOUT this flag `status` is
        /// read-only -- it signs nothing and spends nothing, and reports `settlement_due` so a
        /// poller can see a week is owed without submitting anything. Reading a subscription used
        /// to move the chain, so an orchestrator polling on a timer paid on every boundary it
        /// happened to cross.
        #[arg(long)]
        settle: bool,
    },
    /// Cancel one owned subscription BUY only while it is still resting.
    Cancel {
        /// Ordinary InferenceOrderBook order id.
        order_id: u128,
    },
}

#[derive(Args)]
pub(crate) struct SubscriptionPlaceArgs {
    /// Actor PrivateNote owner key. May be passed before `place` or here after `place`.
    #[arg(long)]
    pub(crate) note_key: Option<PathBuf>,
    /// Per-tick ceiling in whole SHELL: `3` is three SHELL a tick.
    #[arg(long, value_parser = crate::cli::args::parse_price_shell)]
    pub(crate) max_price_per_tick: u128,
    /// Whole 28-day volume. Must be within the protocol bound and divisible by four weeks.
    #[arg(long)]
    pub(crate) ticks: u128,
    /// Place against a model the on-chain `ModelRegistry` does not confirm, and go on when the
    /// registry cannot be read. Without it `place` refuses before any escrow moves, the way
    /// `buyer`, `seller`, `provision` and `deploy-market` already do -- this is the SAME flag those
    /// four carry, not a fifth one, and `place` was the one spending path that had no way to
    /// override the identical refusal. It does not cover a spelling the registry holds differently:
    /// that is a confirmed model and a typo, and it is refused with the registered spelling named
    /// .
    #[arg(long)]
    pub(crate) allow_unverified_model: bool,
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
    /// Directory containing local deal handle JSON files. Defaults to `<data-dir>/deals` when selected,
    /// otherwise to the legacy platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
    /// Report the answer as one JSON object on stdout instead of the human view.

    /// This command prints FACTS a runtime reads -- addresses, balances, state -- and 203 makes the
    /// flag obligatory for exactly that: "hosted runtimes cannot reliably supervise dexdo by
    /// parsing human logs". The human view stays free to be rewritten, wrapped and coloured because
    /// this one does not move.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Secret-free trading history from local deal handles.
#[derive(Args)]
pub(crate) struct HistoryArgs {
    /// Directory containing local deal handle JSON files. Defaults to `<data-dir>/deals` when selected,
    /// otherwise to the legacy platform app data directory.
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
    #[arg(long, default_value = DEFAULT_DASHBOARD_LISTEN)]
    pub(crate) listen: SocketAddr,
    /// Directory containing local deal handle JSON files. Defaults to `<data-dir>/deals` when selected,
    /// otherwise to the legacy platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
}

/// Read current on-chain state for a local deal handle or a raw TokenContract address.
#[derive(Args)]
pub(crate) struct StatusArgs {
    /// Emit the stable `dexdo.status.v3` JSON object.
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
    /// Directory containing local deal handle JSON files. Defaults to `<data-dir>/deals` when selected,
    /// otherwise to the legacy platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
}

/// Role-aware close/recovery wrapper for a local deal handle or raw TokenContract address.
/// An OPEN seller deal submits `sellerStop`; an already STOPped seller deal submits `destroy`; an
/// UNSOLD seller deal -- never funded, so never opened and never stopped -- submits `close`,
/// but only once its ask has left the book, because with an ask still resting `close()` records
/// intent and closes nothing.
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
    /// Directory containing local deal handle JSON files. Defaults to `<data-dir>/deals` when selected,
    /// otherwise to the legacy platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
    /// Role for a raw TokenContract address, or a mismatch guard for a local handle.
    #[arg(long)]
    pub(crate) role: Option<DealRoleArg>,
    /// Actor PrivateNote address for a raw TokenContract address, or a mismatch guard for a local handle.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) note_addr: Option<String>,
    /// Actor PrivateNote owner key. Required only when `close` needs to submit a signed transaction.
    #[arg(long)]
    pub(crate) note_key: Option<PathBuf>,
}

/// Secret-free audit export for one deal.
#[derive(Args)]
pub(crate) struct ExportArgs {
    /// Local handle id/path or raw TokenContract address.
    #[arg(long)]
    pub(crate) deal: String,
    /// Output format.
    #[arg(long, value_enum, default_value = DEFAULT_EXPORT_FORMAT)]
    pub(crate) format: ExportFormatArg,
    /// Directory containing local deal handle JSON files. Defaults to `<data-dir>/deals` when selected,
    /// otherwise to the legacy platform app data directory.
    #[arg(long)]
    pub(crate) deals_dir: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum ExportFormatArg {
    Json,
    Md,
}

/// Args for `dexdo wallet`: bind this instance to the funding (Hot) wallet it spends from.
#[derive(Args)]
pub(crate) struct WalletArgs {
    #[command(subcommand)]
    pub(crate) command: WalletCommand,
}

#[derive(Subcommand)]
pub(crate) enum WalletCommand {
    /// Bind a funding wallet for the first time, through an explicit provider. Refuses to touch an
    /// existing binding: replacing one is `dexdo wallet rebind`.
    Onboard(WalletBindArgs),
    /// Replace the active binding with one from an explicit provider. The binding it replaces is
    /// ARCHIVED, never deleted -- funds can still sit in the old Hot.
    Rebind(WalletBindArgs),
    /// Permanently remove one OLD archived binding and its per-binding secrets directory, but only
    /// after read-only chain balances prove that Hot holds zero native and zero of every ECC.
    RemoveArchived(WalletRemoveArchivedArgs),
    /// Which wallet this instance is bound to, per network, and what it holds. Read-only: it reads
    /// the recorded binding and the Hot's balance, and writes nothing. A balance it cannot read is
    /// named as unread rather than shown as a zero; `--no-balances` skips the chain entirely.

    /// `wallet` had three subcommands and every one of them WROTE -- bind, replace, remove. An
    /// operator who had just bound a wallet could not ask which one it was, and the client's own
    /// README once documented a `wallet status` that never existed, because the command is missing
    /// hard enough that its absence reads as a memory lapse rather than a gap. Every other area
    /// already has its reader: `note list`, `note balance`, `deals`, `markets`, `status`.
    Show(WalletShowArgs),
}

/// Args for the read-only view of the binding.
#[derive(Args)]
pub(crate) struct WalletShowArgs {
    /// Report the binding as JSON instead of the human view.
    #[arg(long)]
    pub(crate) json: bool,

    /// Report the binding without asking the chain what the Hot holds.

    /// The balance is the first thing an operator opens this command for, so it is read by
    /// default. This flag is for the cases where it cannot be: no network, a deliberately offline
    /// run, or a script that wants the recorded fields and nothing that can be slow.
    #[arg(long)]
    pub(crate) no_balances: bool,
}

#[derive(Args)]
pub(crate) struct WalletRemoveArchivedArgs {
    /// Exact id recorded by the archived binding. Active bindings are always refused.
    #[arg(long, value_name = "ID")]
    pub(crate) binding_id: String,
}

/// The provider is a SUBCOMMAND, not a flag, and it has no default.

/// Omitting it shows the numbered choice in an interactive terminal and is an ERROR anywhere else,
/// listing the valid subcommands. Working commands never take a provider at all: they use the
/// binding that was already recorded, because the three providers can hand over the same canonical
/// contract and nothing on chain distinguishes them afterwards.
#[derive(Args)]
pub(crate) struct WalletBindArgs {
    #[command(subcommand)]
    pub(crate) provider: Option<WalletProviderCommand>,

    /// Report the committed binding as one JSON object on stdout instead of the human result.

    /// opens by saying why this exists: "hosted runtimes cannot reliably supervise
    /// dexdo by parsing human logs. They need a stable" contract. Without it a caller that is not a
    /// person has two bad options -- scrape the styled result, or read `RUST_LOG` -- and the second
    /// is what pulled three absolute paths into the operator's result, because a test needed them
    /// and had nowhere else to look.

    /// It sits HERE, on the level `onboard` and `rebind` share, not on the `manual` subcommand,
    /// even though `manual` is the only fully headless provider. The result is printed by one
    /// function for all three providers; the flag is about WHO READS the output, not about how the
    /// wallet was obtained. `--non-interactive` is already shared for the same reason. The object
    /// is identical across providers -- only the moment it appears differs, because
    /// `ackinacki-wallet` and `gosh-ai` both need a human before there is anything to report.
    #[arg(long, global = true)]
    pub(crate) json: bool,
}

#[derive(Subcommand)]
pub(crate) enum WalletProviderCommand {
    /// Acki Nacki Wallet app: it deploys and provides the Vault/Hot pair, and funding is confirmed
    /// by the human inside the app.

    /// It carries the onboarding arguments because this provider is the one flow that is already
    /// implemented: an authenticated bee session needs an agent name and two owner-only local paths,
    /// none of which has a default. The other two providers take no payload yet.
    #[command(name = "ackinacki-wallet")]
    AckinackiWallet(WalletOnboardArgs),
    /// Gosh.ai: it provides a Hot only, handed over as one copied string. There is no Vault.
    #[command(name = "gosh-ai")]
    GoshAi(WalletGoshAiArgs),
    /// A Hot the operator already controls, connected by address plus a local secret file. The
    /// headless path: no terminal, no browser and no camera are required.

    /// It carries a payload for the same reason `ackinacki-wallet` does. PR1295 had to ship this as
    /// a separate `wallet onboard-manual` command because `onboard` then took required flags of its
    /// own and clap's derive demands them even under `subcommand_negates_reqs`. Those flags have
    /// since moved onto the provider subcommands, so `onboard` requires nothing and can host this
    /// one -- which is how the specification spells it.
    Manual(WalletOnboardManualArgs),
}

/// Args for the manual provider. Every one of them is optional on a terminal, where the address and
/// the secret file are asked for instead; an automated run supplies them and is refused, rather than
/// blocked on a prompt, when it does not.
#[derive(Args)]
pub(crate) struct WalletOnboardManualArgs {
    /// Cross-check only: the wallet address you expect. It is derived from the key, so passing it
    /// changes nothing except that a mismatch is refused instead of proceeding. Canonical
    /// `<dapp_id>::<account_id>` or legacy `0:<hex>`.
    #[arg(long, value_name = "ADDRESS")]
    pub(crate) multisig_address: Option<String>,
    /// File with the wallet's 32-byte secret hex. Referenced by the binding; never copied into it.
    #[arg(long, value_name = "PATH", conflicts_with = "multisig_seed_file")]
    pub(crate) multisig_private_key: Option<PathBuf>,
    /// File with the wallet's seed phrase. Referenced by the binding; never copied into it.
    #[arg(long, value_name = "PATH", conflicts_with = "multisig_private_key")]
    pub(crate) multisig_seed_file: Option<PathBuf>,
}

/// Args for `dexdo wallet onboard gosh-ai`.

/// Every field has a default, because the provider can also be chosen from the interactive menu,
/// where there is no command line to carry one. There are deliberately no `--address`, `--seed` or
/// `--phrase` flags: the Gosh.ai string contains a recovery phrase, and process arguments are
/// visible to every local process and land in the shell history, so it is taken only through a
/// hidden prompt.
#[derive(Args, Clone)]
pub(crate) struct WalletGoshAiArgs {
    /// How long to wait for the Hot to appear and go Active, e.g. `20m`. The specification fixes the
    /// default at ten minutes and makes this the only way to change it.
    #[arg(long, value_parser = crate::cli::args::parse_activation_timeout)]
    pub(crate) activation_timeout: Option<std::time::Duration>,
}

/// Parse a SHELL amount -- escrow, deposit, budget, a withdrawal -- as a person states it: `100`,
/// `6.15`, `0.000000001`. The grammar and the refusals are `dexdo_core::shell_amount_raw`, and the
/// text `dexdo_core::shell_amount` prints goes back in unchanged.

/// The same bound the price arguments carry, for the same reason. Every amount on a command line is
/// money a note has to hold or hand over, and the largest note that exists holds 1 000 000 SHELL
/// (`ALLOWED_NOMINALS` in `contracts/dex/modifiers/modifiers.sol`). A figure above that names money
/// no note could carry -- and it is what a command line written before amounts were stated in SHELL
/// looks like, where 5 SHELL was `5000000000`. Guarding the price and not the amount would have
/// left half the money path open to exactly the stale line the price guard refuses.
pub(crate) fn parse_shell_amount(raw: &str) -> Result<u128, String> {
    let amount = dexdo_core::shell_amount_raw(raw)?;
    let ceiling = dexdo_core::params::MAX_NOTE_NOMINAL_SHELL
        .checked_mul(dexdo_core::PRICE_STEP)
        .expect("the largest note nominal in raw ECC[2] fits u128");
    if amount > ceiling {
        return Err(format!(
            "amount {} SHELL is more than the largest note holds ({} SHELL), so no note could \
             carry it. An amount written before amounts were stated in SHELL states raw ECC[2] \
             units, where 5 SHELL is 5000000000: state it as {} instead",
            dexdo_core::shell_amount(amount),
            dexdo_core::params::MAX_NOTE_NOMINAL_SHELL,
            dexdo_core::shell_amount(amount / dexdo_core::PRICE_STEP)
        ));
    }
    Ok(amount)
}

/// Parse a per-tick price stated in whole SHELL into the raw ECC[2] units every price argument
/// carries onward.

/// One SHELL is the book's price step: `InferenceOrderBook` rejects a `pricePerTick` that is not a
/// whole multiple of it, so no other price can reach the chain and none is accepted here. What the
/// operator types is therefore the price itself -- `--price-per-tick 3` is three SHELL a tick --
/// and the billion that used to stand in that place is gone from the command line.

/// Refused, each by name: an empty value, a value that is not a whole decimal number, zero, and a
/// value whose raw form overflows. Zero is refused here rather than downstream because a price of
/// nothing is not a cheap offer -- the book rejects it, after the note has paid the gas to be told.
pub(crate) fn parse_price_shell(raw: &str) -> Result<u128, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err("price is empty; state a whole number of SHELL per tick, for example 3".into());
    }
    let shell: u128 = text.parse().map_err(|_| {
        format!(
            "price {text} is not a whole number of SHELL per tick; prices are quoted in whole \
             SHELL (the book's price step), so 3 means three SHELL a tick"
        )
    })?;
    if shell == 0 {
        return Err("price 0 SHELL per tick is invalid; state at least 1 SHELL".into());
    }
    // A price above the largest note that exists cannot be paid for: one tick at it costs more
    // than any note can hold, and the buyer's bond is two ticks on top of that. This is also what
    // catches a command line written before prices were quoted in SHELL -- every raw price from
    // one SHELL up (`1000000000` and above) lands here, and is refused with the reason rather than
    // carried into the money path to be caught by a later reserve check.
    if shell > dexdo_core::params::MAX_NOTE_NOMINAL_SHELL {
        return Err(format!(
            "price {shell} SHELL per tick is more than the largest note holds ({} SHELL), so no \
             note could pay for one tick at it, let alone the two-tick bond. A price written \
             before prices were quoted in whole SHELL states raw ECC[2] units, where 3 SHELL is \
             3000000000: state it as {} instead",
            dexdo_core::params::MAX_NOTE_NOMINAL_SHELL,
            shell / dexdo_core::PRICE_STEP.max(1)
        ));
    }
    dexdo_core::price_raw_from_shell(shell)
        .ok_or_else(|| format!("price {shell} SHELL per tick is beyond the range of a price"))
}

/// The same price for the one argument whose field is still 64-bit: the seller's own offer price
/// (`SellerConfig::price_per_tick` is `Shell`, a `u64`). The ceiling differs from the other four
/// price arguments, and it is named when it is hit rather than left to a silent conversion later.
pub(crate) fn parse_price_shell_u64(raw: &str) -> Result<u64, String> {
    let value = parse_price_shell(raw)?;
    u64::try_from(value).map_err(|_| {
        format!(
            "price {} SHELL per tick is beyond what a seller offer can hold; the largest it takes \
             is {} SHELL",
            value / dexdo_core::PRICE_STEP,
            u128::from(u64::MAX) / dexdo_core::PRICE_STEP
        )
    })
}

/// Parse a `--activation-timeout` such as `600`, `10m` or `1h`.
pub(crate) fn parse_activation_timeout(raw: &str) -> Result<std::time::Duration, String> {
    parse_duration_flag("--activation-timeout", raw)
}

/// Parse a `--funding-timeout` such as `600`, `10m` or `1h`.

/// Deliberately the same grammar `--activation-timeout` takes: two duration flags on one CLI that
/// accepted different spellings would be a trap for the operator who learned the first one.
pub(crate) fn parse_funding_timeout(raw: &str) -> Result<std::time::Duration, String> {
    parse_duration_flag("--funding-timeout", raw)
}

/// One duration grammar, named by the flag that is reporting the error.
fn parse_duration_flag(flag: &str, raw: &str) -> Result<std::time::Duration, String> {
    let raw = raw.trim();
    let (value, multiplier) = match raw.chars().last() {
        Some('s') => (&raw[..raw.len() - 1], 1u64),
        Some('m') => (&raw[..raw.len() - 1], 60),
        Some('h') => (&raw[..raw.len() - 1], 3_600),
        _ => (raw, 1),
    };
    let value: u64 = value
        .trim()
        .parse()
        .map_err(|_| format!("{flag} `{raw}` is not a duration such as 600, 10m or 1h"))?;
    let secs = value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("{flag} `{raw}` overflows"))?;
    if secs == 0 {
        return Err(format!("{flag} must be greater than zero"));
    }
    Ok(std::time::Duration::from_secs(secs))
}

/// Args for `dexdo wallet onboard ackinacki-wallet`.

/// Every field can be omitted, for the same reason `WalletGoshAiArgs` has defaults: the provider can
/// also be chosen from the interactive menu, where there is no command line to carry one. The two owner-only
/// paths default into the per-binding secrets directory the store reserves for the attempt -- the
/// same 0700 directory the gosh-ai provider writes its recovery phrase into -- so the operator never
/// has to invent a location for a private key, and a key generated by a menu-started onboarding is
/// reachable from the binding that names it.
#[derive(Args)]
pub(crate) struct WalletOnboardArgs {
    /// User-visible agent name sent to the wallet after the signed hello succeeds.

    /// Deliberately still REQUIRED on the subcommand: it is the label a human reads inside the
    /// wallet app when approving, so on a command line the operator gets to choose it. The
    /// interactive menu, which has no command line to carry one, supplies
    /// `params::WALLET_ONBOARD_DEFAULT_AGENT_NAME` instead of dead-ending.
    #[arg(long)]
    pub(crate) agent_name: String,
    /// Owner-only durable bee session state. Re-run with the same arguments to resume.
    /// The default resolves inside this attempt's binding directory; a path you pass is used exactly.
    #[arg(long, value_name = "PATH")]
    pub(crate) state: Option<PathBuf>,
    /// Owner-only file where a newly generated Hot secret key is stored.
    /// The default resolves inside this attempt's binding directory; a path you pass is used exactly.
    #[arg(long, value_name = "PATH")]
    pub(crate) hot_key: Option<PathBuf>,
    /// Generate and store a distinct Vault key. Without this flag, Hot's public key is requested for both.
    #[arg(long, value_name = "PATH")]
    pub(crate) vault_key: Option<PathBuf>,
    /// Save the connection QR code as an SVG file. The text link remains the default path.
    #[arg(long, value_name = "PATH")]
    pub(crate) qr_file: Option<PathBuf>,
    /// Accepted and ignored: the connection code is always drawn in the terminal now, at the
    /// smallest size that still scans. Kept so existing scripts keep running.
    #[arg(long)]
    pub(crate) terminal_qr: bool,
}

/// Args for `dexdo note`: manage the actor's PrivateNotes.
#[derive(Args)]
pub(crate) struct NoteArgs {
    #[command(subcommand)]
    pub(crate) command: NoteCommand,
}

#[derive(Subcommand)]
pub(crate) enum NoteCommand {
    /// Derive the canonical 1-of-1 operational wallet from `--note-key`, report the external
    /// funding it awaits, and deploy it only after that funding is present.
    Wallet(NoteWalletArgs),
    /// List the notes this instance has deployed, with what each one holds.
    /// Read-only: the pool file names them, the chain says what is in them.
    List(NoteListArgs),
    /// Balance: show `getDetails().balance` separately from the account's physical ECC[2] gas
    /// pocket for deploys and cross-dapp calls.
    /// Read-only by address; no key, no signing.
    Balance(NoteBalanceArgs),
    /// Read `getOutstanding` as a source of deal leads, verify each address through its own
    /// TokenContract, and state the note mirror's incompleteness and stale-close caveats.
    /// Read-only by address; no key, no signing.
    Outstanding(NoteOutstandingArgs),
    /// Deploy: a wallet-funded `PrivateNote` on the chain in-process through `gosh.ackinacki`, folded
    /// into a `DEXDO_PN_POOL` pool the `seller`/`buyer` consume.
    Deploy(NoteDeployArgs),
    /// Recover/finalize a wallet-funded `PrivateNote` deploy from a crash-safe recovery state file.
    Recover(NoteRecoverArgs),
    /// Top up an existing note's ECC[2] SHELL gas pocket from a funding wallet, up to an exact level.
    /// This is the account's coin pocket -- what deploys and deal gas are paid from -- and NOT the
    /// note's spendable trading record, which only `deployPrivateNote` can create.
    Topup(NoteTopupArgs),
    /// Transfer: move part of one note's spendable TRADING record into another note's, up to an
    /// exact level on the destination. This is the other balance from `note topup` -- the one a wallet
    /// cannot reach and only a deal, the book or another note can credit.
    Transfer(NoteTransferArgs),
    /// Withdraw: submit owner-signed `PrivateNote.withdrawTokens(destWalletAddr, dapp_id)` for a note's
    /// available token balances. This is not a claim that every native/ECC balance is fully retired without
    /// by-fact evidence on the current contract.
    Withdraw(NoteWithdrawArgs),
    /// Collect the physical ECC[2] that reached a note AFTER it withdrew.
    Sweep(NoteSweepArgs),
}

/// Args for `dexdo note wallet`: derive, inspect, and (once externally funded) deploy the
/// operational wallet used by `note deploy`.
#[derive(Args)]
pub(crate) struct NoteWalletArgs {
    /// File with the user's 32-byte note secret hex. The same seed controls the derived 1-of-1 wallet.
    #[arg(long, value_name = "PATH")]
    pub(crate) note_key: PathBuf,
    /// Intended note nominal: `N100`, `N1000`, `N10000`, `N100000`, or `N1000000`. This chooses
    /// the funding amount; the CLI never chooses a spend for the user.
    #[arg(long)]
    pub(crate) nominal: String,
}

/// Args for `dexdo note list`: which notes exist, and what they hold.

/// No `--note-addr`: the question is which notes there ARE. The pool is the client's own record of
/// every note it deployed, and it was readable only by a command already on its way to spend -- the
/// picker offered the rows and then asked which to take. An operator who wanted to look opened the
/// JSON by hand.
#[derive(Args)]
pub(crate) struct NoteListArgs {
    /// Name the notes without asking the chain what they hold. The pool is local, so this answers
    /// instantly; the balances are what take seconds each.
    #[arg(long)]
    pub(crate) no_balances: bool,
    /// Report the answer as one JSON object on stdout instead of the human view.

    /// This command prints FACTS a runtime reads -- addresses, balances, state -- and 203 makes the
    /// flag obligatory for exactly that: "hosted runtimes cannot reliably supervise dexdo by
    /// parsing human logs". The human view stays free to be rewritten, wrapped and coloured because
    /// this one does not move.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Args for `dexdo note balance`: read-only PrivateNote balance by address.
#[derive(Args)]
pub(crate) struct NoteBalanceArgs {
    /// PrivateNote address to inspect, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) note_addr: String,
    /// Report the answer as one JSON object on stdout instead of the human view.

    /// This command prints FACTS a runtime reads -- addresses, balances, state -- and 203 makes the
    /// flag obligatory for exactly that: "hosted runtimes cannot reliably supervise dexdo by
    /// parsing human logs". The human view stays free to be rewritten, wrapped and coloured because
    /// this one does not move.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Args for `dexdo note outstanding`: read-only, independently verified deal-lead diagnostics.
#[derive(Args)]
pub(crate) struct NoteOutstandingArgs {
    /// PrivateNote address to inspect, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) note_addr: String,
}

/// Args for `dexdo note topup`: raise an existing note's ECC[2] SHELL to an exact level from a wallet.

/// The level is a TARGET, not an amount to add, and that is a money-safety choice rather than a
/// convenience. A wallet spend can land while the client never learns it did (a dropped connection
/// after the POST); re-running an "add 40 SHELL" command then adds it twice, while re-running "bring
/// it to 350" the second time computes a shortfall of zero and submits nothing. The command is
/// therefore safe to repeat after any uncertain outcome, which is the state an operator is actually
/// in when a top-up appears to have failed.
#[derive(Args)]
#[command(group(clap::ArgGroup::new("topup_funding_key")
    .args(["multisig_private_key", "multisig_seed_file"])
    .multiple(false)
    .required(false)))]
pub(crate) struct NoteTopupArgs {
    /// PrivateNote address to credit, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) note_addr: String,
    /// SHELL level to bring the note UP TO. Already at or above it: nothing is submitted.
    #[arg(long = "to", value_name = "SHELL", value_parser = crate::cli::args::parse_shell_amount)]
    pub(crate) to_raw: u128,
    /// Deployed multisig WALLET address that funds the top-up. Omit it to spend from the
    /// active wallet binding; passing it still wins and is used exactly as given.
    #[arg(long, requires = "topup_funding_key")]
    pub(crate) multisig_address: Option<String>,
    /// File with the multisig wallet's 32-byte secret hex (the funding key); the secret is never logged.
    #[arg(
        long,
        value_name = "PATH",
        requires = "multisig_address",
        conflicts_with = "multisig_seed_file"
    )]
    pub(crate) multisig_private_key: Option<PathBuf>,
    /// File with the multisig wallet seed phrase. TVM-compatible derivation is used; the phrase is never logged.
    #[arg(
        long,
        value_name = "PATH",
        requires = "multisig_address",
        conflicts_with = "multisig_private_key"
    )]
    pub(crate) multisig_seed_file: Option<PathBuf>,
    /// How long to wait for a Hot top-up to arrive on chain, e.g. `20m`. The wallet
    /// specification fixes the default at ten minutes and makes this the only way to change it.
    /// Crossing it cancels nothing: a Vault transaction already created stays on chain and the same
    /// command re-run picks the wait back up.
    #[arg(long, value_parser = crate::cli::args::parse_funding_timeout)]
    pub(crate) funding_timeout: Option<std::time::Duration>,
}

/// Args for `dexdo note transfer`: move a note's spendable trading record into another note.

/// The level is a TARGET ON THE DESTINATION, not an amount to move, for the same reason `note topup`
/// takes one and with a sharper edge. A note-to-note transfer is an owner-signed send whose result
/// the client can fail to learn -- a dropped connection after the POST, a confirmation that never
/// comes back -- and re-running "move 40 SHELL" then moves it twice, out of a record that has no
/// wallet-side refill to correct the overshoot with. Re-running "bring the destination to 350"
/// computes a shortfall of zero the second time and submits nothing. The command is therefore safe
/// to repeat after any uncertain outcome, which is the state an operator is actually in when a
/// transfer appears to have failed.

/// The destination is named by ADDRESS here even though the contract takes a deposit hash: the hash
/// is read off the destination's own `getDetails()`, so the note whose balance is inspected and the
/// note the contract credits are the same note by construction rather than by the operator pasting
/// two matching values.
#[derive(Args)]
pub(crate) struct NoteTransferArgs {
    /// SENDING PrivateNote address, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`. Its spendable trading record is what moves.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) note_addr: String,
    /// File with the SENDING note's owner secret hex (the SDK secret of the on-chain `PrivateNote`);
    /// the secret is never logged. `initTransfer` is `onlyOwnerPubkey(_ephemeralPubkey)`.
    #[arg(long, value_name = "PATH")]
    pub(crate) note_key: PathBuf,
    /// RECEIVING PrivateNote address, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`. Its `depositIdentifierHash` is read from it and
    /// is what the contract derives the destination from.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) to_note_addr: String,
    /// SHELL level to bring the DESTINATION note's spendable trading record UP TO. Already at or
    /// above it: nothing is submitted.
    #[arg(long = "to", value_name = "SHELL", value_parser = crate::cli::args::parse_shell_amount)]
    pub(crate) to_raw: u128,
}

/// Args for `dexdo note withdraw`: submit a note token-balance withdrawal to a wallet.
#[derive(Args)]
pub(crate) struct NoteWithdrawArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// Destination wallet address in canonical `<dapp_id>::<account_id>` form. REQUIRED, and
    /// deliberately never defaulted or inferred from the note pool/funding wallet: the transfer is
    /// irreversible. Account-only legacy `0:<account_id>` is refused because it carries no
    /// destination DApp for withdrawal evidence.
    #[arg(long)]
    pub(crate) to: String,
}

/// Args for `dexdo note sweep`: `PrivateNote.sweepShell(destWalletAddr, dapp_id)`.

/// `withdrawTokens` is one-shot, but SHELL keeps arriving after it -- a cancelled inference order
/// refunds to the owner note, a deal returns a bond to the note that posted it, and none of those
/// paths ask whether the note has withdrawn. This is the contract's own door for that tail, and
/// without a command the money sits where no command can reach it.
#[derive(Args)]
pub(crate) struct NoteSweepArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// Destination wallet address in canonical `<dapp_id>::<account_id>` form. REQUIRED, and
    /// deliberately never defaulted: the transfer is irreversible, "our" wallet and "your" wallet
    /// are different objects, and the price of the wrong one here is everything the note holds.
    /// The DApp half is not a formality either -- it is the `dest_dapp_id` the contract sends to,
    /// which is why account-only legacy `0:<account_id>` is refused.
    #[arg(long)]
    pub(crate) to: String,
}

/// Args for `dexdo note deploy`: deploys the wallet-funded `PrivateNote` in-process through
/// `gosh.ackinacki`, then adapts the result into the `DEXDO_PN_POOL` schema.
#[derive(Args)]
#[command(group(clap::ArgGroup::new("deploy_funding_key")
    .args(["multisig_private_key", "multisig_seed_file"])
    .multiple(false)
    .required(false)))]
pub(crate) struct NoteDeployArgs {
    /// Emit one machine-readable JSON result on stdout.
    #[arg(long)]
    pub(crate) json: bool,
    /// Deployed multisig WALLET address that funds the note. Env fallback:
    /// DEXDO_MULTISIG_ADDRESS. Omit both to spend from the active wallet binding; passing
    /// the flag still wins and is used exactly as given.
    #[arg(long)]
    pub(crate) multisig_address: Option<String>,
    /// File with the multisig wallet's 32-byte secret hex (the funding key). Env fallback:
    /// DEXDO_MULTISIG_PRIVATE_KEY. The secret is never logged.
    #[arg(long, value_name = "PATH", conflicts_with = "multisig_seed_file")]
    pub(crate) multisig_private_key: Option<PathBuf>,
    /// File with the multisig wallet seed phrase. TVM-compatible derivation is used; the phrase is never logged.
    #[arg(long, value_name = "PATH", conflicts_with = "multisig_private_key")]
    pub(crate) multisig_seed_file: Option<PathBuf>,
    /// PN deposit nominal: `N100`, `N1000`, `N10000`, `N100000`, or `N1000000`. Required on purpose
    /// -- the deposit is a spend from the funding wallet, so the CLI never picks a
    /// denomination for the operator.
    #[arg(long)]
    pub(crate) nominal: String,
    /// Deposit currency. Dexdo markets use SHELL only.
    #[arg(
        long,
        default_value = SHELL_CURRENCY_LABEL,
        value_parser = [SHELL_CURRENCY_LABEL]
    )]
    pub(crate) token_type: String,
    /// The `DEXDO_PN_POOL` JSON to append the deployed note to (created if absent). Defaults to the
    /// canonical per-instance pool under the effective `--data-dir`.
    #[arg(long, value_name = "PATH")]
    pub(crate) pool: Option<PathBuf>,
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
    /// Test/live-gate failpoint: submit deployPrivateNote, wait until the note address is active/discoverable,
    /// then stop before recording pn_address/deposit_identifier_hash in recovery.
    #[arg(long, hide = true)]
    pub(crate) simulate_interrupt_after_deploy_before_note_record: bool,
    /// How long to wait for a Hot top-up to arrive on chain, e.g. `20m`. The wallet
    /// specification fixes the default at ten minutes and makes this the only way to change it.
    /// Crossing it cancels nothing: a Vault transaction already created stays on chain and the same
    /// command re-run picks the wait back up.
    #[arg(long, value_parser = crate::cli::args::parse_funding_timeout)]
    pub(crate) funding_timeout: Option<std::time::Duration>,
}

/// Args for `dexdo note recover`: finalize a deploy whose recovery state was persisted before/after spend.
#[derive(Args)]
pub(crate) struct NoteRecoverArgs {
    /// Crash-safe recovery state written by `dexdo note deploy`.
    #[arg(long, value_name = "PATH")]
    pub(crate) recovery: PathBuf,
    /// The `DEXDO_PN_POOL` JSON to append the recovered note to (created if absent). Defaults to the
    /// canonical per-instance pool under the effective `--data-dir`.
    #[arg(long, value_name = "PATH")]
    pub(crate) pool: Option<PathBuf>,
}

pub(crate) const DEXDO_MULTISIG_ADDRESS_ENV: &str = "DEXDO_MULTISIG_ADDRESS";
pub(crate) const DEXDO_MULTISIG_PRIVATE_KEY_ENV: &str = "DEXDO_MULTISIG_PRIVATE_KEY";

impl NoteDeployArgs {
    /// Apply the established explicit-first/env-fallback convention without enabling clap's env
    /// feature. The two environment variables are one BYO-Hot pair; a seed-file flag remains an
    /// explicit alternative and therefore suppresses the key fallback.
    pub(crate) fn apply_multisig_env_fallbacks(&mut self) -> Result<()> {
        if self.multisig_address.is_none() {
            self.multisig_address = match std::env::var_os(DEXDO_MULTISIG_ADDRESS_ENV) {
                Some(raw) if !raw.is_empty() => Some(raw.into_string().map_err(|_| {
                    anyhow::anyhow!("{DEXDO_MULTISIG_ADDRESS_ENV} is not valid UTF-8")
                })?),
                _ => None,
            };
        }
        if self.multisig_private_key.is_none() && self.multisig_seed_file.is_none() {
            self.multisig_private_key = std::env::var_os(DEXDO_MULTISIG_PRIVATE_KEY_ENV)
                .filter(|raw| !raw.is_empty())
                .map(PathBuf::from);
        }

        self.validate_multisig_pair()
    }

    pub(crate) fn validate_multisig_pair(&self) -> Result<()> {
        let has_address = self.multisig_address.is_some();
        let has_secret = self.multisig_private_key.is_some() || self.multisig_seed_file.is_some();
        match (has_address, has_secret) {
            (true, false) => bail!(
                "a BYO Hot address requires --multisig-private-key/--multisig-seed-file or \
                 {DEXDO_MULTISIG_PRIVATE_KEY_ENV}"
            ),
            (false, true) => {
                bail!("a BYO Hot key requires --multisig-address or {DEXDO_MULTISIG_ADDRESS_ENV}")
            }
            _ => Ok(()),
        }
    }
}

/// Oracle/OracleEventList/PMP discovery and reads, plus the existing range-market lifecycle.
#[derive(Args)]
pub(crate) struct OracleArgs {
    #[command(subcommand)]
    pub(crate) command: OracleCommand,
}

#[derive(Subcommand)]
pub(crate) enum OracleCommand {
    /// Derive the Oracle address by RootOracle name, without an oracle-market manifest or key.
    Address(OracleAddressArgs),
    /// Read or derive OracleEventList state by explicit contract address.
    #[command(name = "event-list")]
    EventList(OracleEventListArgs),
    /// Derive or inspect a prediction-market pool without an oracle-market manifest.
    Pmp(OraclePmpArgs),
    /// Inspect the standard OrderBook bound to an explicit PMP.
    Book(OracleBookArgs),
    /// Deploy-if-absent Oracle + OracleEventList, add a range event, deploy/approve the PMP from a note, and
    /// write the resulting oracle-market manifest.
    Provision(Box<OracleProvisionArgs>),
    /// Read OracleEventList/PMP state from an oracle-market manifest.
    State(OracleStateArgs),
    /// Resolve a range PMP: OracleEventList.resolveRange -> OB requestWeeklyMedian -> OEL onWeeklyMedian -> PMP submitResolve.
    Resolve(OracleResolveArgs),
    /// Cast this oracle's normal cancellation vote for the manifest PMP.
    Cancel(OracleResolveArgs),
    /// Delete the manifest event after its real deadline and after all PMP confirmations are released.
    Delete(OracleResolveArgs),
    /// Reclaim this note's stake from a cancelled PMP through owner-authenticated `PrivateNote.cancelStake`.
    CancelStake(OraclePmpExitArgs),
    /// Claim this note's resolved PMP payout through owner-authenticated `PrivateNote.claim`.
    Claim(OraclePmpExitArgs),
    /// Abandon a PMP stake to unfreeze the note. Costs the stake; refuses without the flag.
    ForfeitStake(OracleForfeitStakeArgs),
    /// Withdraw accumulated raw ECC[2] SHELL fees from an owner-authenticated Oracle.
    WithdrawFees(OracleWithdrawFeesArgs),
}

#[derive(Args)]
pub(crate) struct OracleAddressArgs {
    /// Deterministic RootOracle name for the Oracle.
    #[arg(long)]
    pub(crate) oracle_name: String,
}

#[derive(Args)]
pub(crate) struct OracleEventListArgs {
    #[command(subcommand)]
    pub(crate) command: OracleEventListCommand,
}

#[derive(Subcommand)]
pub(crate) enum OracleEventListCommand {
    /// Derive the deterministic OracleEventList address for an Oracle and index.
    Address(OracleEventListAddressArgs),
    /// Enumerate all events exposed by an OracleEventList.
    Events(OracleEventListEventsArgs),
}

#[derive(Args)]
pub(crate) struct OracleEventListAddressArgs {
    /// Oracle address, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) oracle: String,
    /// OracleEventList index under the Oracle.
    #[arg(long)]
    pub(crate) index: u128,
}

#[derive(Args)]
pub(crate) struct OracleEventListEventsArgs {
    /// OracleEventList address, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) event_list: String,
}

#[derive(Args)]
pub(crate) struct OraclePmpArgs {
    #[command(subcommand)]
    pub(crate) command: OraclePmpCommand,
}

#[derive(Subcommand)]
pub(crate) enum OraclePmpCommand {
    /// Derive the deterministic PMP address for an event, oracle-name list, and token type.
    Address(OraclePmpAddressArgs),
    /// Read PMP details, shutdown state, unclaimed balance, version, and bound book address.
    Status(OraclePmpStatusArgs),
}

#[derive(Args)]
pub(crate) struct OraclePmpAddressArgs {
    /// Event id as uint256 decimal or `0x` hex.
    #[arg(long)]
    pub(crate) event_id: String,
    /// Oracle name. Repeat once per oracle in the PMP's canonical name list.
    #[arg(long = "oracle-name", required = true)]
    pub(crate) oracle_names: Vec<String>,
    /// PMP collateral token type. Defaults to the canonical SHELL currency id (`2`).
    #[arg(long, default_value_t = SHELL_CURRENCY_ID)]
    pub(crate) token_type: u32,
}

#[derive(Args)]
pub(crate) struct OraclePmpStatusArgs {
    /// PMP address, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) pmp: String,
}

#[derive(Args)]
pub(crate) struct OracleBookArgs {
    #[command(subcommand)]
    pub(crate) command: OracleBookCommand,
}

#[derive(Subcommand)]
pub(crate) enum OracleBookCommand {
    /// Read the standard getters of the OrderBook bound to a PMP.
    Status(OracleBookStatusArgs),
    /// Read one order from the standard OrderBook bound to a PMP.
    Order(OracleBookOrderArgs),
    /// Read orders owned by one deposit identifier from the standard OrderBook bound to a PMP.
    Orders(OracleBookOrdersArgs),
}

#[derive(Args)]
pub(crate) struct OracleBookStatusArgs {
    /// PMP whose bound OrderBook is inspected, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) pmp: String,
}

#[derive(Args)]
pub(crate) struct OracleBookOrderArgs {
    /// PMP whose bound OrderBook is inspected, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) pmp: String,
    /// Standard OrderBook order id.
    #[arg(long)]
    pub(crate) order_id: u128,
}

#[derive(Args)]
pub(crate) struct OracleBookOrdersArgs {
    /// PMP whose bound OrderBook is inspected, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) pmp: String,
    /// Owner deposit identifier hash as uint256 decimal or `0x` hex.
    #[arg(long)]
    pub(crate) deposit_hash: String,
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
    #[arg(long, default_value_t = DEFAULT_ORACLE_EVENT_LIST_INDEX)]
    pub(crate) event_list_index: u128,
    /// Human-readable OracleEventList description, used only when the list needs deployment.
    #[arg(long, default_value = DEFAULT_ORACLE_EVENT_LIST_DESCRIPTION)]
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
    #[arg(long, default_value = DEFAULT_ORACLE_PMP_DESCRIPTION)]
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
    /// PMP collateral token type. Dexdo markets use SHELL (`2`) only.
    #[arg(
        long,
        default_value_t = SHELL_CURRENCY_ID,
        value_parser = parse_shell_currency_id
    )]
    pub(crate) token_type: u32,
    /// Oracle fee paid by the PMP deployer note to this OracleEventList.
    #[arg(long, default_value_t = DEFAULT_ORACLE_FEE)]
    pub(crate) oracle_fee: u128,
    /// Output path for the oracle-market manifest.
    #[arg(long, default_value = DEFAULT_ORACLE_MARKET_OUTPUT_PATH)]
    pub(crate) output: PathBuf,
}

#[derive(Args)]
pub(crate) struct OracleStateArgs {
    /// Oracle-market manifest produced by `dexdo oracle provision`.
    #[arg(long)]
    pub(crate) manifest: PathBuf,
}

#[derive(Args)]
pub(crate) struct OracleResolveArgs {
    /// Oracle-market manifest produced by `dexdo oracle provision`.
    #[arg(long)]
    pub(crate) manifest: PathBuf,
    /// Signing key for the public `resolveRange` external message. The oracle key is conventional.
    #[arg(long)]
    pub(crate) oracle_key: PathBuf,
}

#[derive(Args)]
#[command(group(clap::ArgGroup::new("pmp_exit_target")
    .args(["manifest", "pmp"])
    .multiple(false)
    .required(true)))]
pub(crate) struct OraclePmpExitArgs {
    #[command(flatten)]
    pub(crate) identity: IdentityArgs,
    /// Oracle-market manifest produced by `dexdo oracle provision`; binds event, oracle list, PMP, and token type.
    /// Either this or the raw `--pmp` + triple below; the run that creates a stake is free to delete
    /// its own manifest, and that must not leave the stake unmanageable.
    #[arg(long)]
    pub(crate) manifest: Option<PathBuf>,
    /// The PMP contract itself, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`. Use with
    /// `--event-id`, `--oracle-list-hash` and `--token-type` when no manifest survives. The triple is
    /// recoverable from chain alone: `oracleListHash` and `tokenType` are duplicated inside the note's
    /// own `_stakes` record, and a candidate `eventId` is confirmed by recomputing the stake key.
    #[arg(long, requires_all = ["event_id", "oracle_list_hash"], value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) pmp: Option<String>,
    /// `eventId` of the stake, 0x-prefixed uint256. Only with `--pmp`.
    #[arg(long, requires = "pmp")]
    pub(crate) event_id: Option<String>,
    /// `oracleListHash` of the stake, 0x-prefixed uint256. Only with `--pmp`.
    #[arg(long, requires = "pmp")]
    pub(crate) oracle_list_hash: Option<String>,
    /// Token type of the stake. Only with `--pmp`; dexdo markets use SHELL.
    #[arg(long, requires = "pmp", default_value_t = SHELL_CURRENCY_ID)]
    pub(crate) token_type: u32,
}

/// Args for `dexdo oracle forfeit-stake`: `PrivateNote.deleteStake`.

/// The THIRD exit from a PMP stake, and the only one with no lifecycle gate -- `PMP.forfeitStake`
/// checks the sender and nothing else, while `cancelStake` needs the event cancelled and the book
/// drained. That makes it the way out of the states where a stake freezes the WHOLE note
/// (`withdrawTokens` and `initTransfer` both require `_stakes.empty()`) and `cancel-stake` can
/// never pass.

/// It costs the stake. The abandoned mass is recorded in `PMP._forfeited`, never paid to the
/// forfeiter, and leaves only at the market's close -- to the PMP's deployer, which for any stake
/// this client can hold is this same note. Hence the flag: the price is real and the operator says
/// it out loud.
#[derive(Args)]
pub(crate) struct OracleForfeitStakeArgs {
    #[command(flatten)]
    pub(crate) exit: OraclePmpExitArgs,
    /// REQUIRED to proceed, and deliberately never defaulted. Without it the command refuses and
    /// prints what the forfeit costs and in which order to act -- the refusal is the feature, not
    /// an obstacle to it.
    #[arg(long = "abandon-the-stake")]
    pub(crate) abandon_the_stake: bool,
}

#[derive(Args)]
pub(crate) struct OracleWithdrawFeesArgs {
    /// Oracle contract that holds the fees, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) oracle: String,
    /// Oracle owner key; the secret is never logged.
    #[arg(long)]
    pub(crate) oracle_key: PathBuf,
    /// Recipient address accepted by `Oracle.withdrawFees(address,uint128)`, canonical `<dapp_id>::<account_id>` or legacy `0:<64 hex>`.
    #[arg(long, value_parser = dexdo_core::address::arg_to_chain_param)]
    pub(crate) to: String,
    /// SHELL to withdraw. Must be greater than zero and no more than the live Oracle balance.
    #[arg(long, value_parser = crate::cli::args::parse_shell_amount)]
    pub(crate) amount: u128,
}

/// Args for `dexdo accumulator`: exchange SHELL <-> eccUSDC from the operator multisig.
#[derive(Args)]
pub(crate) struct AccumulatorArgs {
    #[command(subcommand)]
    pub(crate) command: AccumulatorCommand,
}

#[derive(Subcommand)]
pub(crate) enum AccumulatorCommand {
    /// Read the accumulator's rate, balances and the four FIFO queues. Read-only; no key, no spend.
    Status(AccumulatorStatusArgs),
    /// Sell SHELL for eccUSDC: deposit SHELL into fixed-size lots (1, 10, 100 or 1000 eccUSDC).
    /// A lot cannot be cancelled -- it pays out only once a buyer matches it and `claim` collects it.
    Sell(AccumulatorSellArgs),
    /// Buy SHELL with eccUSDC. The amount is exact: whatever resting lots do not cover is minted.
    Buy(AccumulatorBuyArgs),
    /// List the lots this wallet owns, recovered from the chain alone. Read-only; no spend.
    Lots(AccumulatorLotsArgs),
    /// Collect the eccUSDC owed on matched lots.
    Claim(AccumulatorClaimArgs),
}

/// Args for `dexdo accumulator status`.
#[derive(Args)]
pub(crate) struct AccumulatorStatusArgs {}

/// Args for `dexdo accumulator sell`: convert the wallet's SHELL into eccUSDC sell lots.

/// `--usdc` is stated in WHOLE eccUSDC of lot value, not in SHELL, because that is the unit the
/// contract denominates lots in: `--usdc 154` creates 1x100 + 5x10 + 4x1 lots and costs 15,400
/// SHELL. Without it the command converts as much of the balance as divides into whole lots and
/// reports the remainder it cannot convert.
#[derive(Args)]
#[command(group(clap::ArgGroup::new("accumulator_sell_amount")
    .args(["usdc", "all"])
    .multiple(false)
    .required(true)))]
#[command(group(clap::ArgGroup::new("accumulator_sell_key")
    .args(["multisig_private_key", "multisig_seed_file"])
    .multiple(false)
    .required(false)))]
pub(crate) struct AccumulatorSellArgs {
    /// Whole eccUSDC of lot value to create. Decomposed into 1/10/100/1000 lots, largest first.
    #[arg(long, value_name = "WHOLE")]
    pub(crate) usdc: Option<u64>,
    /// Convert as much of the wallet's SHELL as divides into whole lots, leaving only the remainder.
    #[arg(long)]
    pub(crate) all: bool,
    /// Deployed multisig WALLET address that holds the SHELL. Omit it to spend from the active
    /// wallet binding; passing it still wins and is used exactly as given.
    #[arg(long, requires = "accumulator_sell_key")]
    pub(crate) multisig_address: Option<String>,
    /// File with the multisig wallet's 32-byte secret hex; the secret is never logged.
    #[arg(long, value_name = "PATH", requires = "multisig_address")]
    pub(crate) multisig_private_key: Option<PathBuf>,
    /// File with the multisig wallet seed phrase; the phrase is never logged.
    #[arg(long, value_name = "PATH", requires = "multisig_address")]
    pub(crate) multisig_seed_file: Option<PathBuf>,
}

/// Args for `dexdo accumulator buy`: spend eccUSDC and receive SHELL at the fixed rate.
#[derive(Args)]
#[command(group(clap::ArgGroup::new("accumulator_buy_key")
    .args(["multisig_private_key", "multisig_seed_file"])
    .multiple(false)
    .required(false)))]
pub(crate) struct AccumulatorBuyArgs {
    /// Whole eccUSDC to spend. The accumulator refuses any amount that is not a whole eccUSDC.
    #[arg(long, value_name = "WHOLE")]
    pub(crate) usdc: u64,
    /// Deployed multisig WALLET address that holds the eccUSDC. Omit it to spend from the active
    /// wallet binding; passing it still wins and is used exactly as given.
    #[arg(long, requires = "accumulator_buy_key")]
    pub(crate) multisig_address: Option<String>,
    /// File with the multisig wallet's 32-byte secret hex; the secret is never logged.
    #[arg(long, value_name = "PATH", requires = "multisig_address")]
    pub(crate) multisig_private_key: Option<PathBuf>,
    /// File with the multisig wallet seed phrase; the phrase is never logged.
    #[arg(long, value_name = "PATH", requires = "multisig_address")]
    pub(crate) multisig_seed_file: Option<PathBuf>,
}

/// Args for `dexdo accumulator lots`: recover this wallet's lots from the chain.
#[derive(Args)]
pub(crate) struct AccumulatorLotsArgs {
    /// Wallet whose lots to list. Omit it to use the active wallet binding.
    #[arg(long)]
    pub(crate) multisig_address: Option<String>,
    /// Lowest order id to scan in each queue. The scan walks every issued id from here, so a run
    /// that knows when its lots were created can bound the work; the default scans from the first.
    #[arg(long, value_name = "ORDER_ID", default_value_t = 1)]
    pub(crate) from_order_id: u64,
}

/// Args for `dexdo accumulator claim`: collect eccUSDC owed on matched lots.

/// With neither `--denom` nor `--order-id` this claims every matched lot the wallet owns; a lot that
/// no buyer has matched yet is left alone rather than submitted, because the accumulator would
/// refuse it (`ERR_ORDER_NOT_SOLD`, 205).
#[derive(Args)]
#[command(group(clap::ArgGroup::new("accumulator_claim_key")
    .args(["multisig_private_key", "multisig_seed_file"])
    .multiple(false)
    .required(false)))]
pub(crate) struct AccumulatorClaimArgs {
    /// Claim only this denomination. Requires `--order-id`.
    #[arg(long, requires = "order_id")]
    pub(crate) denom: Option<u16>,
    /// Claim only this order id. Requires `--denom`.
    #[arg(long, requires = "denom")]
    pub(crate) order_id: Option<u64>,
    /// Lowest order id to scan in each queue while locating the wallet's lots.
    #[arg(long, value_name = "ORDER_ID", default_value_t = 1)]
    pub(crate) from_order_id: u64,
    /// Deployed multisig WALLET that owns the lots. Omit it to use the active wallet binding.
    #[arg(long, requires = "accumulator_claim_key")]
    pub(crate) multisig_address: Option<String>,
    /// File with the multisig wallet's 32-byte secret hex; the secret is never logged.
    #[arg(long, value_name = "PATH", requires = "multisig_address")]
    pub(crate) multisig_private_key: Option<PathBuf>,
    /// File with the multisig wallet seed phrase; the phrase is never logged.
    #[arg(long, value_name = "PATH", requires = "multisig_address")]
    pub(crate) multisig_seed_file: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn mock_model_network_policy_refuses_only_real_mainnet() {
        for network in ["mainnet", "shellnet"] {
            for mock_chain in [false, true] {
                for mock_model in [false, true] {
                    let flags = super::MockFlags {
                        mock_model,
                        mock_chain,
                    };
                    let result = flags.reject_mainnet_mock_model(network);
                    let refused = network == "mainnet" && mock_model && !mock_chain;

                    if refused {
                        assert_eq!(
                            result.expect_err("real mainnet mock-model must be refused").to_string(),
                            "--mock-model cannot run against mainnet: it serves deterministic fake output into a real market for real SHELL; remove --mock-model, or point DEXDO_MANIFEST at shellnet for the mock-model + real-chain test stage"
                        );
                    } else {
                        result.unwrap_or_else(|error| {
                            panic!(
                                "network={network} mock_chain={mock_chain} mock_model={mock_model} \
                                 must remain allowed: {error}"
                            )
                        });
                    }
                }
            }
        }
    }

    #[test]
    fn migrated_cli_consumers_do_not_redeclare_canonical_parameters() {
        let args_source = include_str!("args.rs")
            .split_once("#[cfg(test)]\nmod tests")
            .expect("args unit-test module boundary")
            .0;
        let consumers = [
            ("args.rs", args_source),
            ("admin.rs", include_str!("admin.rs")),
            ("commands.rs", include_str!("commands.rs")),
            ("indexer.rs", include_str!("indexer.rs")),
            ("oracle.rs", include_str!("oracle.rs")),
        ];
        let canonical_names = [
            "CLI_POSITIVE_U64_MIN",
            "DEFAULT_BUYER_MAX_TOKENS",
            "DEFAULT_BUYER_TICKS",
            "DEFAULT_CHAIN_READ_TIMEOUT_SECS",
            "crate::cli::commands::manifest_path()?",
            "DEFAULT_CONTINUITY_MODE",
            "DEFAULT_DASHBOARD_LISTEN",
            "default_deposit_shells",
            "DEFAULT_EXECUTABLE_BOOK_TICKS",
            "DEFAULT_EXPORT_FORMAT",
            "DEFAULT_MARKET_DATA_OUTPUT",
            "DEFAULT_MARKET_DATA_TIMEOUT_MS",
            "DEFAULT_MARKETS_FRAME_MODEL",
            "DEFAULT_MARKET_MANIFEST_OUTPUT_PATH",
            "DEFAULT_MODELS_PATH",
            "DEFAULT_MONITOR_TREE_WIDTH",
            "DEFAULT_NOTE_INDEX",
            "DEFAULT_ORACLE_EVENT_LIST_DESCRIPTION",
            "DEFAULT_ORACLE_EVENT_LIST_INDEX",
            "DEFAULT_ORACLE_FEE",
            "DEFAULT_ORACLE_MARKET_OUTPUT_PATH",
            "DEFAULT_ORACLE_PMP_DESCRIPTION",
            "DEFAULT_POLICY_ROLE",
            "DEFAULT_PROVISION_MAX_TICKS",
            "DEFAULT_SELLER_GATEWAY_LISTEN",
            "DEFAULT_SELLER_MOCK_TOKEN_COUNT",
            "INDEXER_ERROR_BODY_MAX_BYTES",
            "MARKET_DATA_DEPTH_LIMIT_MAX",
            "MARKET_DATA_DEPTH_LIMIT_MIN",
            "MARKET_DATA_LIST_LIMIT_MAX",
            "MARKET_DATA_LIST_LIMIT_MIN",
            "MARKET_DEPLOY_ACTIVATION_MAX_READS",
            "MARKET_DEPLOY_ACTIVATION_POLL_INTERVAL",
            "ORACLE_RESOLUTION_MAX_READS",
            "ORACLE_RESOLUTION_POLL_INTERVAL",
        ];

        for name in canonical_names {
            assert!(
                consumers.iter().any(|(_, source)| source.contains(name)),
                "a migrated CLI consumer must use params::{name}"
            );
            let declaration = format!("const {name}");
            for (path, source) in consumers {
                assert!(
                    !source.contains(&declaration),
                    "{path} must consume params::{name} directly instead of redeclaring it"
                );
            }
        }
    }

    /// Real `dexdo seller` argv through the real parser, so what is asserted is what an operator
    /// can actually type.
    fn seller_args(extra: &[&str]) -> super::SellerArgs {
        use clap::Parser as _;
        let mut argv = vec![
            "dexdo",
            "seller",
            "--note-addr",
            "0:note",
            "--token-contract",
            "0:tc",
            "--model",
            "qwen",
        ];
        argv.extend_from_slice(extra);
        let crate::Command::Seller(args) = crate::Cli::try_parse_from(argv)
            .expect("seller argv parses")
            .command
        else {
            panic!("expected Command::Seller");
        };
        args
    }

    /// E2E-ADV-16 -- when a seller lets the machine pick its port and the real port is written back
    /// into the address buyers are told to dial, that rewrite cannot turn a refused address into
    /// an accepted one, or leave a port nobody can dial.

    /// Setup: one address of every kind, each paired with a real port the machine handed out. Do:
    /// apply the rewrite in three situations -- the address was inherited and has no port yet, the
    /// operator named the address themselves, and the operator named a real port already. Observe:
    /// only the first is rewritten; the rewrite keeps the same verdict about who can dial it and
    /// never leaves the placeholder port; and a rewrite offered a port belonging to a different
    /// address is refused.

    /// Not proven: that the rewritten address is actually reachable. That is proven separately by
    /// a test that runs the whole seller and dials what a buyer would be handed.

    /// E2E-ADV-16, `tests/e2e/test-specification.md`.
    #[test]
    fn the_inherited_zero_port_rewrite_keeps_the_classifier_verdict_and_never_yields_port_zero() {
        use dexdo::seller::advertise::classify_advertise;

        // One address per decided class, plus a public one: the rewrite must be verdict-preserving
        // for all of them, not only for the loopback case the live shape happens to use.
        let hosts = [
            "127.0.0.1",
            "0.0.0.0",
            "10.1.2.3",
            "169.254.10.1",
            "100.64.0.1",
            "94.156.178.14",
            "[::1]",
            "[fd00::1]",
            "[2606:4700::1111]",
        ];

        for host in hosts {
            let listen: std::net::SocketAddr = format!("{host}:0").parse().expect("listen host:0");
            let bound: std::net::SocketAddr =
                format!("{host}:54321").parse().expect("bound host:port");

            // The inherited case: no `--gateway-advertise`, listen `:0`.
            let inherited = seller_args(&[
                "--allow-private-advertise",
                "--gateway-listen",
                &listen.to_string(),
            ]);
            let before = inherited.gateway_advertise_addr();
            assert_eq!(before, listen.to_string());
            let after = inherited.bound_gateway_advertise(before.clone(), bound);

            assert_eq!(
                after,
                bound.to_string(),
                "{host}: an inherited :0 advertise must adopt the bound port"
            );
            assert_eq!(
                classify_advertise(&after),
                classify_advertise(&before),
                "{host}: the rewrite changed the classifier verdict; it must only move the port"
            );
            assert_ne!(
                after.parse::<std::net::SocketAddr>().expect("rewritten").port(),
                0,
                "{host}: the rewrite produced the placeholder port the explicit form is refused for"
            );

            // An EXPLICIT advertise is the operator's own word and is never rewritten behind their
            // back -- including when the bind landed on a different port entirely.
            let explicit_host = format!("{host}:8443");
            let explicit = seller_args(&[
                "--allow-private-advertise",
                "--gateway-listen",
                &listen.to_string(),
                "--gateway-advertise",
                &explicit_host,
            ]);
            assert_eq!(
                explicit.bound_gateway_advertise(explicit.gateway_advertise_addr(), bound),
                explicit_host,
                "{host}: an explicit advertise was rewritten"
            );

            // A listen address that already named a real port is not a placeholder either.
            let concrete = seller_args(&[
                "--allow-private-advertise",
                "--gateway-listen",
                &explicit_host,
            ]);
            assert_eq!(
                concrete.bound_gateway_advertise(concrete.gateway_advertise_addr(), bound),
                explicit_host,
                "{host}: a listen address with a real port must not be rewritten"
            );
        }

        // A rewrite is only ever an adoption of the SAME host's real port: a bind that reports a
        // different IP than the one being advertised must leave the advertise alone, because the
        // host is what the classifier verdict is about.
        let cross = seller_args(&["--allow-private-advertise", "--gateway-listen", "0.0.0.0:0"]);
        assert_eq!(
            cross.bound_gateway_advertise(
                "0.0.0.0:0".to_string(),
                "94.156.178.14:54321".parse().unwrap()
            ),
            "0.0.0.0:0",
            "a bind on a different host must not silently re-home the advertised address"
        );
    }

    /// E2E-ADV-09 -- the switch that lets a seller advertise a local address does not also let
    /// through a port nobody can dial or an address that is not an address.

    /// Setup: real command lines, parsed the way an operator's typing is parsed. Do: with the
    /// switch on, ask for an address whose port is the placeholder, then for a local address with
    /// a real port, then for six malformed addresses; repeat each with the switch off. Observe:
    /// the placeholder port is refused either way with the same message, the local address is
    /// refused only with the switch off, and every malformed address is rejected while it is still
    /// being read, with the switch on and off alike.

    /// Not proven, and it narrows what is owed: turning the switch on does skip the address check
    /// rather than loosening it, but an operator cannot exploit that from the command line,
    /// because a malformed address never gets that far and an inherited one cannot be malformed.
    /// Reaching it needs a caller that hands in a raw string, so the gap is inside the library
    /// rather than at the command line.

    /// E2E-ADV-09, `tests/e2e/test-specification.md`.
    #[test]
    fn the_private_advertise_opt_in_relaxes_neither_the_port_zero_guard_nor_the_parser() {
        // 1 -- port 0, with the opt-in set.
        let zero = seller_args(&[
            "--allow-private-advertise",
            "--gateway-advertise",
            "seller.example.net:0",
        ]);
        let refusal = zero
            .checked_gateway_advertise_addr()
            .expect_err("port 0 stays refused under the private-advertise opt-in")
            .to_string();
        assert!(
            refusal.starts_with("error[E_ADVERTISE_NOT_PUBLIC] (config): --gateway-advertise ")
                && refusal.contains("uses port 0, which no remote buyer can dial"),
            "{refusal}"
        );
        // And it is the same refusal without the flag, so the flag changed nothing here.
        let zero_default = seller_args(&["--gateway-advertise", "seller.example.net:0"]);
        assert_eq!(
            zero_default
                .checked_gateway_advertise_addr()
                .expect_err("port 0 is refused by default")
                .to_string(),
            refusal
        );

        // A private host with a real port is what the flag IS for: refused by default, admitted
        // with it. The pair is what makes the two assertions above mean "the flag works and still
        // did not relax the port guard", rather than "the flag does nothing".
        assert!(seller_args(&["--gateway-advertise", "192.168.1.10:8443"])
            .checked_gateway_advertise_addr()
            .is_err());
        assert_eq!(
            seller_args(&[
                "--allow-private-advertise",
                "--gateway-advertise",
                "192.168.1.10:8443",
            ])
            .checked_gateway_advertise_addr()
            .expect("the opt-in admits a private address class"),
            "192.168.1.10:8443"
        );

        // 2 -- malformed advertise strings never reach `validate_advertise`, flag or no flag.
        use clap::Parser as _;
        for malformed in [
            "",
            ":8443",
            "seller.example.net",
            "user@seller.example.net:8443",
            "seller.example.net:not-a-port",
            "seller example net:8443",
        ] {
            for extra in [vec![], vec!["--allow-private-advertise"]] {
                let mut argv = vec![
                    "dexdo",
                    "seller",
                    "--note-addr",
                    "0:note",
                    "--token-contract",
                    "0:tc",
                    "--model",
                    "qwen",
                    "--gateway-advertise",
                    malformed,
                ];
                argv.extend_from_slice(&extra);
                assert!(
                    crate::Cli::try_parse_from(argv).is_err(),
                    "a malformed --gateway-advertise {malformed:?} was accepted by the parser \
                     (opt-in: {})",
                    !extra.is_empty()
                );
            }
        }
    }

    #[test]
    fn pmp_token_type_accepts_only_shell() {
        assert_eq!(
            super::parse_shell_currency_id("2").unwrap(),
            dexdo_core::params::SHELL_CURRENCY_ID
        );
        assert!(super::parse_shell_currency_id("1").is_err());
        assert!(super::parse_shell_currency_id("3").is_err());
    }
}
