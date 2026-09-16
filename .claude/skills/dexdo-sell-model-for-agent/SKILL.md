---
name: dexdo-sell-model-for-agent
description: Guides a SELLER end-to-end through selling model inference on the dexdo market (real shellnet) -- install the client, derive and fund the operational wallet, mint a wallet-funded private note, configure the model access key and models.json, read the current price with `dexdo market`, answer and validate the required seller policy, provision a per-deal market (`dexdo provision` -> market.json), run the `dexdo seller` gateway (posts the offer, forces the model, proxies the real upstream, streams tick by tick), hand the deal address to the buyer, and check by-fact accounting (`dexdo status`/`dexdo monitor`) -- how many ticks were delivered and how much SHELL was received. Load this when the user wants to SELL access to their model, stand up a seller gateway, serve buyers, or check revenue and delivered tokens. For the buyer side, use the `dexdo-buy-model-for-agent` skill. Written for an agent acting on the user's behalf: the agent runs every command and reports what came back. A seller who wants to run the commands themselves should read `dexdo-sell-model-for-human` instead.
---

# dexdo -- selling model access (seller side)

Walk the seller through the real shellnet flow: install -> wallet -> note -> price -> policy -> validate ->
provision -> gateway -> status. After each command, show the output and do not advance until the step is green.
Secrets (wallet seed/key, note owner secret, the pool file, `GROQ_API_KEY`) are never printed or
committed.

If any command fails, run `dexdo doctor` first -- it reports the shellnet version, manifest
freshness, and whether your `policy.json` is complete.

**Prerequisites:** the user's own `--note-key` seed, access to SHELL for the planned trading and
deployment costs, and a model access key (for example `GROQ_API_KEY`). An existing deployed and
funded `UpdateCustodianMultisigWallet_v2` v2.2.0 or v2.4.0 wallet can be reused if it has exactly
one custodian whose public key matches the supplied funding key. Other wallet contract types are
not supported. `dexdo note deploy` itself does not create or fund the multisig.

---

## Choose order semantics before posting an offer

Use the mapping below before offering a command. `N` is the requested number of ticks; one tick is
1,000,000 tokens. `L` is the buyer's limit price per tick, and `A` is the seller's actual ask at a
fill, with `A <= L`. For any price `p`, `fee(p) = floor(p * 250 / 10000)`,
`E_p = N * (p + fee(p))` is the fee-inclusive service deposit, and `T_p = E_p + 2p` adds the buyer
bond.

### "I want to offer ongoing access, not a one-off purchase"

**Support:** Supported only as a fixed four-week subscription.

**Flags:** The seller uses `seller --model A --subscription`. The buyer counterpart must place with
`subscription --model A place --max-price-per-tick L --ticks N`, then use the matched deal with
`buyer --resume --frame-model A` and optional `--local-listen ADDR`. For an explicit due-week
booking, the buyer adds `--settle` to `subscription ... status ID`; plain status is read-only. Both
orders carry `AON|SUBSCRIPTION`.

**Money and time:** Posting the resting SELL holds no escrow. The offered product is exactly four
weeks. On the buyer side, `T_L` leaves the note at placement; cancelling while unfilled returns all
of it. At match, `T_A` enters the deal and the limit-price spread returns. Each week allows `N/4`
ticks, and unused allowance expires at that week's boundary instead of rolling forward. The
take-or-pay clock starts when the probe is accepted, not when the order is placed or matched.
Before probe acceptance, no week is charged, but buyer STOP burns one probe price. After
acceptance, buyer STOP bills every elapsed week and the current week in full whether or not its
allowance was used. Future unstarted weeks return on a clean close, as does the `2A` buyer bond. A
dispute or penalty can instead burn stake `D` between `A` and `2A`. Buyer resume reuses the same
match, places no new BUY, and preserves the subscription when the process exits.

**Refuse nearby:** There is no custom term, custom week length, rollover, subscription MARKET
order, or split across sellers. There is no buyer-side `--subscription` flag; placement must use
the `subscription` subcommand. Do not tell a buyer to resume an order that is still resting.

### "Cancel my sell offer if it is unfilled in an hour"

**Support:** A user-selected seller TTL is not supported. The shipped seller uses a fixed one-hour
deadline and its liveness path submits expiry for its own stale ask.

**Flags:** There is no TTL flag. Use `seller --model A` for an ordinary offer or add
`--subscription` for a subscription offer; the backend supplies 3600 seconds. Before the deadline,
use `orders ... cancel ID`. After it, `orders ... expire ID` is the explicit removal action.

**Money and time:** At the fixed deadline, 3600 seconds after placement, the offer becomes
non-fillable, but time alone sends no transaction. The seller liveness path reaps its stale ask; a
permissionless expiry can also remove it. A resting SELL holds no escrow, its deal latch is released
on removal, and no fill means no book trading fee.

**Refuse nearby:** There is no selectable seller TTL or CLI GTC. Do not promise a removal
transaction at exactly 3600 seconds or a keeper reward. BUY expiry is different: the buyer process
does not automatically sweep it, and BUY funds remain recorded until a removal transaction lands.

### "Sell only to subscribers"

**Support:** Supported with exact symmetric separation between subscription and ordinary orders.

**Flags:** Use `seller --model A --subscription`. The buyer counterpart must use
`subscription --model A place`. Both orders carry `AON|SUBSCRIPTION`.

**Money and time:** Compatibility is checked before pricing or settlement. A subscription ask can
match only a subscription BUY, and an ordinary ask can match only an ordinary BUY. The resting SELL
holds no escrow. If the subscription matches, its fixed four-week shape and the buyer's deposit,
bond, billing, refund, and burn commitments are exactly those stated in the first mapping above.

**Refuse nearby:** There is no single ask that accepts both products, subscriber preference with
ordinary fallback, or buyer-side `--subscription` flag. A live resting ask cannot change shape;
remove it before reposting with different flags.

## Addresses

dexdo prints and stores addresses in the canonical Acki Nacki form
`<dapp_id>::<account_id>` (two 64-hex halves). Paste that form back into later
commands; every address example below uses `<DAPP-ID>::<ACCOUNT-ID>`. For a fresh
operational wallet, `dexdo note wallet` prints `<ACCOUNT-ID>::<ACCOUNT-ID>` because
the wallet's dapp id is its own account id.

## Phase 1. Install the client

One-line installer (primary):

```sh
# Linux / macOS
curl -fsSL https://github.com/gosh-sh/dexdo-cli/releases/latest/download/install.sh | sh
# Windows (PowerShell)
# irm https://github.com/gosh-sh/dexdo-cli/releases/latest/download/install.ps1 | iex
```

Build from source (alternative):

```sh
git clone https://github.com/gosh-sh/dexdo-cli && cd dexdo-cli
cargo build --release -p dexdo   # binary: target/release/dexdo
```

Verify with `dexdo --help`. Every on-chain command reads a deployed-contracts manifest. The manifest
NAMES ITS NETWORK, and the client takes the network, the endpoint and the pins from it -- so which
file you point at IS which network you work on. With `DEXDO_MANIFEST` unset the client reads the
manifest the release installer puts beside the `dexdo` binary, which pins mainnet, and it looks in
that one directory only -- never the working directory, `$HOME`, or the platform configuration
directories. **The build above leaves no manifest beside `target/release/dexdo`**, so set the
variable explicitly here. It wins wherever it is set, and a path it names that does not exist is
refused against that path rather than falling back.

```sh
curl -fsSL https://raw.githubusercontent.com/gosh-sh/dexdo-cli/main/manifest/mainnet.manifest.json \
  -o ~/dexdo/mainnet.manifest.json

export DEXDO_MANIFEST=~/dexdo/mainnet.manifest.json
```

## Phase 2. Prepare and fund the operational wallet

The operational multisig is the user's source of funds for minting notes. Its address is
deterministic: derive it from the user's own `--note-key` seed and show it to the user before
anything is deployed. The same seed controls the note and the operational wallet, so there is no
separate `--wallet-addr` identity and no independently chosen wallet address.

Run the onboarding command with the nominal of the note you intend to mint:

```sh
dexdo note wallet \
  --note-key /path/to/note.key \
  --nominal N1000
```

`--note-key` here is the wallet-derivation seed and is still yours to supply -- it is what makes the
address deterministic. It is not the note owner key the trading commands below sign with; that one
the client writes and reads itself.

On a fresh seed, this command derives and prints the canonical address without submitting a chain
write. While funding is absent it reports that the wallet is waiting and exits without deploying.
It uses no faucet or giver. Mainnet has no giver, and this path does not need one.

The chain forces this order:

1. Send the stage-one amount the command printed to the derived address with the non-bounceable
   ECC[2] flag-`16` form. The destination becomes `Uninit` with that amount as native balance and
   ECC[2] still zero. This leg buys deploy gas and nothing else, so it is a small flat figure that
   does not depend on the nominal: SHELL that lands as native can never be spent as currency again.
2. Rerun `dexdo note wallet`. It deploys the wallet once the native balance is present, and the
   account becomes `Active`.
3. Send the stage-two amount the command printed to the now-`Active` wallet as ECC[2] with the
   active-account flag-`1` form, then rerun `dexdo note wallet` to confirm the funding. This is the
   larger of the two and the one the nominal belongs to.

The non-bounceable predeploy leg leaves the money at the deterministic address. There is no
automatic refund if the operator stops; it becomes reachable by deploying the matching derived
wallet.

ECC[2] cannot be delivered to an account that does not exist, so the ECC[2] leg cannot precede the
deploy. A live shellnet proof completed both funding legs from an ordinary canonical v2.4 multisig.
That funding wallet needs native for the outbound message and its fees as well as ECC[2] for the
note. ECC[2]-only provisioning fails in the transaction's action phase: no outbound message is
emitted, no ECC[2] moves, and the balances appear unchanged. The observed failure was
`result_code=37` with `no_funds=true`.

In the shellnet proof, deploying from a 1,250 SHELL predeploy balance consumed `156 222 000` raw
native, about 0.156 SHELL, in fee and gas. That is a shellnet measurement, not a mainnet cost
promise. The remaining balance stayed in the user's wallet for spending or withdrawal.

## Phase 3. Mint a private note

### The proving material, and where it lives

`note deploy` proves in zero knowledge, which needs a 64 MB KZG reference string and a ~464 MB
proving key derived from it. The reference string is now fetched during `dexdo wallet onboard`, so
the first command that moves money does not stop to download it. The proving key is built by the
prover during its first proof and cached, so the FIRST `note deploy` on a machine takes several
minutes longer than every later one.

Both live under `params/` and `params/halo2_cache/` **relative to the directory you run from** --
not the `--data-dir`, and not next to the binary. Run from a different directory and the client
finds neither and fetches and rebuilds both. Either always run from the same directory, or pin them:

```sh
export PARAMS_DIR="$PWD/params"
export HALO2_PK_CACHE="$PWD/params/halo2_cache"
```


A private note is the funded trading account used by the seller commands below. One note can fund
trading up to its nominal, so choose the nominal for the amount that note needs to trade against.

The contract allows `N100`, `N1000`, `N10000`, `N100000`, and `N1000000`. The pinned SDK parser used
by the CLI accepts only `N100`, `N1000`, and `N10000` today. `N100000` and `N1000000` are
contract-legal but CLI-impossible. Therefore `N10000` is the largest note this tool can mint today.
To trade against more, mint several notes.

For the deposit voucher, the wallet attaches the chosen nominal plus the contract's
`GAS_DEPOSIT` of 250 SHELL:

```text
+-------------+----------------+
| Wanted note | Attach (SHELL) |
+-------------+----------------+
| N100        | 350            |
| N1000       | 1,250          |
| N10000      | 10,250         |
+-------------+----------------+
```

The attachment is not the price of a note: 350 SHELL is only the `N100` case, and wallet deploy
and transaction gas are separate. Do not attach the bare nominal. Attaching exactly 10,000 SHELL
leaves 9,750 after `GAS_DEPOSIT`, which is not an allowed nominal and fails with
`ERR_NOT_ALLOWED` (141). Attaching exactly 100 SHELL is below `GAS_DEPOSIT` and fails with
`ERR_BELOW_GAS_DEPOSIT` (408).

`dexdo note deploy` funds a fresh private note from your multisig wallet (no giver) and folds it
into a pool file. Notes are funded in SHELL only -- SHELL is what pays the per-deal market deploys,
gas, and runtime -- so `--token-type shell` is the only accepted currency. `--nominal` is required
and has no default: pick one of the three CLI-supported values above.

```sh
dexdo note deploy \
  --multisig-address <ACCOUNT-ID>::<ACCOUNT-ID> \
  --multisig-seed-file /path/to/wallet.seed \
  --nominal N10000 \
  --token-type shell \
  --pool pn_pool.json
```

Use `--multisig-private-key /path/to/wallet.key` (a file with the 32-byte hex secret) instead of
`--multisig-seed-file` if you hold the raw key. `pn_pool.json` holds the note owner secret -- keep it
private, never commit it. `dexdo note deploy` is the user note-creation path. Point later seller
commands at the pool it creates. Success is the block that opens `Note deployed` and carries an `address:` line with the note and a
`folded:` line with the pool; do not advance on an earlier progress line.

Before wallet onboarding, wallet funding, or chain preflight, dexdo atomically persists the fresh owner key in
`pn_pool.json.recovery.json` with owner-only permissions. A completed pool stores note credentials,
not funding-wallet provenance; notes funded by different multisigs may share it when nominal and
token type match.

Point later seller commands at the pool it creates:

```sh
export DEXDO_PN_POOL="$PWD/pn_pool.json"
```

## Phase 4. Note check, models.json, and the upstream key

Do **not** copy the note's owner secret out of the pool. `dexdo note deploy` wrote it there beside
the address, and every command that signs for a note reads it back from that same pool entry, so
`--note-key` is not a flag you type. It stays available for the one case it was made for: a note
that lives outside any pool.

The read-only book commands still take the note address, so keep that to hand:

```sh
NOTE_ADDR=$(jq -r '.notes[-1].address' pn_pool.json)
```

Confirm the note actually holds SHELL before you spend it (read-only, no key):

```sh
dexdo note balance --note-addr "$NOTE_ADDR"
```

**Sizing:** the note's on-chain SHELL (its ECC currency-2 balance) must cover `--deposit-shells` for
the deal deploy (Phase 6, whole SHELL) plus runtime gas. If it is short, deploy a larger `--nominal`
(or another note). Provision fails closed if `--deposit-shells` exceeds this balance.

`models.json` in the working directory maps a model key to its canonical id, upstream, and metadata.
`frame_model` is the on-chain canonical id (the market name); `served_model` is sent upstream;
`api_key_env` names the env var holding the key. Add another model as a new entry.

```json
{
  "models": {
    "qwen": {
      "frame_model": "Qwen3.8-27B",
      "base_url": "https://api.groq.com/openai/v1",
      "served_model": "qwen/qwen3.8-27b",
      "api_key_env": "GROQ_API_KEY",
      "tokenizer_family": "qwen",
      "price_per_tick": 1,
      "capabilities": { "max_output_tokens": 16384 }
    }
  }
}
```

`capabilities.max_output_tokens` is the model's own maximum completion length at that provider (Groq
answers `400` above `16384` for `qwen/qwen3.8-27b`). The seller clamps every outbound request to it, so the
field is REQUIRED: a model entry without it is refused before the provider is contacted rather than served
with an unbounded limit. Take the number from your provider's model card.

`capabilities.logprobs` and `capabilities.top_logprobs` are RETIRED. The client no longer requests, parses or
verifies log probabilities anywhere, so setting either key changes nothing about what the seller sends or how
it is paid. An existing `models.json` that still carries them keeps working: the keys are **ignored, not
rejected** -- deliberately, so an upgrade does not take a live seller off the market. Do not add them to a new
config; `max_output_tokens` is the only capability the seller reads.

The `price_per_tick` here is decorative metadata -- it does NOT set the live deal price. The price
buyers pay is whatever you set at `dexdo provision --price-per-tick` (Phase 6); editing this field
changes nothing on-chain.

Export the upstream key (not written to logs): `export GROQ_API_KEY=<your-key>`

### Selling Claude through the native Anthropic upstream

Dexdo selects `seller/upstream/anthropic.rs` when the model entry points to `api.anthropic.com`. The seller
calls the Anthropic Messages API directly; do not put a LiteLLM/OpenAI-compatible proxy between them. Keep the
real key in the environment named by `api_key_env`, never in `models.json`. Its `capabilities` need only the
model's own output limit:

```json
{
  "models": {
    "claude-sonnet": {
      "frame_model": "claude-sonnet-4-5",
      "served_model": "claude-sonnet-4-5-20250929",
      "base_url": "https://api.anthropic.com",
      "api_key_env": "ANTHROPIC_API_KEY",
      "tokenizer_family": "claude",
      "price_per_tick": 1,
      "capabilities": { "max_output_tokens": 64000 }
    }
  }
}
```

Set `ANTHROPIC_API_KEY`, then run the seller with `--model claude-sonnet --models models.json`. The adapter
streams text immediately and reconciles billing to Anthropic's cumulative `usage.output_tokens`, not SSE
content-delta count.

## Phase 5. Read the price, then answer the failure policy

First look at the model's shared order book (read-only, writes nothing) so you can price your offer
against the market:

```sh
dexdo market Qwen3.8-27B --note-addr "$NOTE_ADDR"
```

It prints the resting asks (price per tick, max ticks) and their deal addresses. `dexdo markets
--models models.json --note-addr "$NOTE_ADDR"` lists every configured book. To be taken by a
best-price buyer, price at or below the current best ask.

The real `dexdo provision` and `dexdo seller` commands use the same complete seller policy. Keep one
explicit path for both commands:

```sh
POLICY="${XDG_CONFIG_HOME:-$HOME/.config}/dexdo/policy.json"
dexdo policy init --role seller --path "$POLICY"
dexdo policy edit --path "$POLICY"
```

Those two are how you write the file yourself, and they are no longer the normal route. On a
terminal you can skip both: the first of `dexdo provision` or `dexdo seller` to run asks for the
rules itself -- one situation at a time, in words, with the suggested answer marked -- and writes
the answers to this same path. It asks once, and nothing asks again unless you change them. The four situations a seller
is asked about are: a deal has run to the end; a buyer paid and never connected; a buyer disputes
your work; how many deals to run at once.

Run the pair above when the machine that sells will not have a terminal -- a service unit, a CI job
-- or when the seller carries `--non-interactive`. There is no interview then: the command refuses
and names what is unanswered, so the file has to be complete before it starts. Either way the
answers land as:

```json
{
  "version": 1,
  "seller": {
    "on": {
      "buyer_no_show": "retire_gateway",
      "after_deal_done": "retire",
      "dispute_against_me": "release_if_clean"
    },
    "max_open_deals": 1
  }
}
```

Allowed values (the scaffold also lists these under `_legend.allowed`):

- `seller.on.buyer_no_show`: `retire_gateway`.
- `seller.on.after_deal_done`: `retire`.
- `seller.on.dispute_against_me`: `release_if_clean | hold`.
- `seller.max_open_deals`: **exactly `1`** -- the gateway owns one deal per
  process and refuses to start with any other value.

The republish and cleanup action families are not seller runtime capabilities. Validation rejects
them locally; it never substitutes a supported action, and the interview does not offer them either:
where only one answer is executable it states that answer and sets it rather than drawing a menu of
one.

`policy.json` is the shared recovery control point. Read it with `dexdo policy show` and check it
with `dexdo policy validate`; `dexdo policy init` and `dexdo policy edit` remain for a file you want
to scaffold or amend by hand. The seller section chooses the supported terminal action after buyer
no-show or deal completion and whether to release or hold a dispute.

## Phase 6. Validate the policy, then provision the deal

Validate immediately before provisioning. `provision` repeats the same pure-local validation before
it reads the note key, connects to shellnet, prompts for a deposit, writes a manifest, or submits a
transaction:

```sh
dexdo policy validate --role seller --path "$POLICY"
dexdo provision \
  --policy "$POLICY" \
  --frame-model Qwen3.8-27B \
  --nonce 1 \
  --price-per-tick 1 \
  --max-ticks 1024 \
  --deposit-shells 20 \
  --output market.json
```

The note is not on that command line. Once the local checks pass, `provision` offers the pool's
notes as a list you move through with the arrow keys and pick with Enter: each row is the address in
the canonical `dapp::account` form, shortened, beside what that note holds in SHELL -- read the same
way `dexdo note balance` reads it. The owner key comes from the entry you picked. A run with nobody
to answer -- no terminal, or `--non-interactive` -- does not ask: it refuses and names `--note-addr`
as the flag that carries the answer, which is what a script passes.

Provision the per-deal `TokenContract` once per deal. `--nonce` is required and must be unique per
deal (it derives the deal address). `--price-per-tick` is the tick price in whole SHELL:
`--price-per-tick 3` is three SHELL a tick. One SHELL is the book's price step, so a price is always
a whole number of them and nothing finer is accepted;
`--max-ticks` is a tick count and bounds the deal.
`--deposit-shells` (whole SHELL) funds the ONE deploy the note still pays for -- the per-deal
`TokenContract`. It stopped being two in contracts 4.0.34: `SuperRoot` deploys the `RootModel` itself
with an internal message that carries its own value, so there is no second address to pre-fund.
Omit the flag and the client sizes it from `--max-ticks`; the default is the deal's own requirement,
not a flat number. It must fit the note balance from Phase 4 -- do not set it to the whole note (the
remainder burns at `destroy`, and the note still needs runtime SHELL). The result `market.json`
carries the deal address (`token_contract`), the model, and the nonce.

## Phase 7. Run the seller gateway

```sh
dexdo seller \
  --policy "$POLICY" \
  --market market.json \
  --model qwen \
  --models models.json \
  --gateway-listen 0.0.0.0:8443 \
  --gateway-advertise <public-host>:8443
```

The note is chosen the same way `provision` chose it, from the same list and with the same key
lookup, and refused the same way off a terminal.

The offer price and volume come from the provisioned deal in `market.json` (set in Phase 6). The
seller's own `--price-per-tick` flag is **ignored on the `--market` path** -- to re-price, run a new
`dexdo provision` with a fresh `--nonce` and serve that manifest. `--gateway-listen` is the local
bind address; `--gateway-advertise` is the address a REMOTE buyer dials (the public address written
into the handover -- never `127.0.0.1`). It must be publicly reachable: startup rejects a bind-all
(`0.0.0.0`/`::`), loopback, RFC1918, link-local or CGNAT advertise -- and equally a reserved range
that is never routed (documentation `192.0.2.0/24`, `198.51.100.0/24`, `203.0.113.0/24`,
`2001:db8::/32`, `3fff::/20`; benchmarking `198.18.0.0/15`; `240.0.0.0/4`; `0.0.0.0/8`; multicast)
-- with `error[E_ADVERTISE_NOT_PUBLIC] (config)` BEFORE the offer is posted, so no resting ask ever
points at an unreachable gateway. For same-host/LAN testing only, add `--allow-private-advertise`. With
`--market`, do not also pass `--token-contract`/`--nonce` (they come from the file). On start the
gateway posts the offer, then daemonizes: it polls for a match, opens the stream, and streams tick by
tick. The wait for a buyer is open-ended -- the resting offer is not torn down.

## Phase 8. Hand the deal address to the buyer

Give the buyer either the `market.json` file OR the `token_contract` string
(`<DAPP-ID>::<ACCOUNT-ID>`) from it. If you hand over the bare `token_contract` (not the file), you
**must also give the buyer the canonical
frame model** `Qwen3.8-27B` -- the buyer needs it as `--frame-model` alongside
`--token-contract`. The buyer places the buy; the gateway opens the stream automatically and forces
the configured model.

## Phase 9. Check status (by-fact accounting)

Authoritative deal state (reads the chain, moves nothing) -- pass the deal `token_contract` from
`market.json`:

```sh
dexdo status <DAPP-ID>::<ACCOUNT-ID>
```

It prints the lifecycle `state=` (`placed`/`funded-but-never-opened`/`probe`/`streaming`/`stopped`/
`disputed`), the boolean flags (`funded`/`opened`/`probe_accepted`/`disputed`), and accounting
(`finalized_owed`, `buyer_locked`, `deposit`, ...). For a revenue roll-up across one or more markets:

```sh
dexdo monitor --market market.json
```

Read-only: ticks delivered, SHELL received, per-deal collateral held or burned, and whether the deal is closed.
Repeat `--market` for several markets; run it in a separate terminal.

## Phase 10. Anti-abuse and recovery

Each deal keeps its own exact `2P` seller bond and contested buyer amount in that TokenContract.
A dispute freezes only those per-deal funds; it does not lock the seller's or buyer's whole
PrivateNote, so other independent deals remain possible.

Use the recovery action that matches the failure:

```text
+----------------------------------------+-------------------------------------+-------------------------------------------------------------------+
| Situation                              | Command                             | What it gives you                                                 |
+----------------------------------------+-------------------------------------+-------------------------------------------------------------------+
| Concede a dispute                      | dexdo release-dispute               | Returns this TC's contested amount and seller bond.                |
| Collect finalized closed-deal earnings | dexdo withdraw-shell                | Pays finalized SHELL to the seller note the deal stored.          |
| Cancel one stale resting offer         | dexdo orders cancel <ID>            | Removes that order from the model book.                           |
| Cancel all stale resting offers        | dexdo orders cancel-all             | Removes all of this note's orders from the model book.            |
+----------------------------------------+-------------------------------------+-------------------------------------------------------------------+
```

`dexdo release-dispute` and `dexdo withdraw-shell` need either
`--token-contract <DAPP-ID>::<ACCOUNT-ID>` or `--market market.json`; the note they act for is
offered from the pool exactly as above, and its owner key comes from the entry you pick.
`dexdo orders` is the exception that still wants `--note-addr` written out -- it filters the book by
that note and will not guess which one -- but it does not want `--note-key`. Put the shared note and
market flags before the subcommand:

```sh
dexdo orders --note-addr "$NOTE_ADDR" --market market.json cancel <ID>
dexdo orders --note-addr "$NOTE_ADDR" --market market.json cancel-all
```

Resolve a disputed TC with `release-dispute` or arbitration. There is no whole-note stream lock or
force-clear step: an offer, an open deal and a dispute are all isolated inside their own
`TokenContract` and never freeze the whole note.

## Wrap-up

After the buyer closes (stops) the deal, close the deal contract to release resources (any leftover
deal gas burns cross-dapp):

```sh
dexdo destroy --market market.json
```

Move the note's remaining token balance back to a wallet:

```sh
dexdo note withdraw --note-addr "$NOTE_ADDR" --to <DAPP-ID>::<ACCOUNT-ID>
```

`note withdraw` still names its note -- it is one-shot and it will not choose which note to end --
but not its key. Its canonical `--to` is required every time and is never inferred from the pool,
the funding wallet or a previous command.

---

## Common errors

- `policy (...) is missing or unreadable ... Run dexdo policy init` (or `... is incomplete`) -- the seller
  policy is absent or still has `UNSET`/invalid fields, and this run had nobody to ask. Re-run it on a
  terminal and answer the questions (Phase 5), or fill the file and check it with
  `dexdo policy validate --role seller`; remember `seller.max_open_deals` must be `1`.
- `the note to spend from has to be chosen, and this run cannot ask: pass --note-addr` -- the run is
  not on a terminal (or carries `--non-interactive`), so the note list was not offered. Pass
  `--note-addr` (Phase 4), or run it where it can ask.
- `--nonce <n> is required and must be UNIQUE per deal` -- set a new unique `--nonce` each deal.
- provision fails for lack of SHELL -- the note's ECC currency-2 balance is below `--deposit-shells`;
  deploy a larger `--nominal` or lower `--deposit-shells` (check `dexdo note balance`).
- buyer cannot connect -- check `--gateway-listen`/`--gateway-advertise` reachability and that both
  sides use the same manifest. `dexdo doctor` diagnoses manifest drift.
- `error[E_ADVERTISE_NOT_PUBLIC] (config)` -- `--gateway-advertise` (or the `--gateway-listen` it
  defaulted to) is not an address a remote buyer can dial. Pass a public `host:port`, or
  `--allow-private-advertise` for local/LAN testing only.
- `advertised_gateway ... status="fail"` with `error[E_ADVERTISE_UNREACHABLE] (network)` -- the
  startup self-probe to the advertised address failed at the transport level. This is fatal and no
  offer is posted. Behind NAT/a VPN/an SSH reverse tunnel the probe can hairpin back into
  this same process and fail while a remote buyer connects fine -- but the seller cannot tell that
  apart from an address no buyer can reach, so it refuses to publish. Verify from outside
  (`curl -k https://<advertise>/`) and fix the path the probe took.
- `error[E_ADVERTISE_WRONG_GATEWAY] (tls)` -- something answered on the advertised address but it is
  not this gateway (certificate-pin mismatch). This is always fatal; fix the address or the tunnel
  target, never bypass it.

## Hard rules

- Never print, log, or commit the wallet seed/key, the note owner secret (`owner_secret_key_hex`),
  the pool file, or `GROQ_API_KEY`.
- Do not trust the buyer request's `model` field -- the market forces the model from `--model`.
- `dexdo destroy` is destructive: run it only on a closed (stopped) deal.
