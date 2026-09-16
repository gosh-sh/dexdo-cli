---
name: dexdo-buy-model-for-agent
description: Guides a BUYER end-to-end through buying model inference on the dexdo market (real shellnet) -- install the client, derive and fund the operational wallet, mint a wallet-funded private note, receive the deal address from the seller (market.json or token_contract), read the real price with `dexdo market`/`dexdo quote`, answer the required failure policy the client asks for on first run, run `dexdo buyer --local-listen` (places the buy and brings up a local OpenAI-compatible endpoint), use the purchased model from any OpenAI client (curl / OPENAI_BASE_URL), and check by-fact accounting (`dexdo status`/`dexdo history`) -- how much SHELL was paid, how many ticks were received, and what per-deal escrow remains frozen. Load this when the user wants to BUY a model, connect to a seller, use someone else's model locally, or check what they paid for. For the seller side, use the `dexdo-sell-model-for-agent` skill. Written for an agent acting on the user's behalf: the agent runs every command and reports what came back. A buyer who wants to run the commands themselves should read `dexdo-buy-model-for-human` instead.
---

# dexdo -- buying model access (buyer side)

Walk the buyer through the real shellnet flow: install -> wallet -> note -> price -> policy -> buy -> stream
the model locally -> status. After each command, show the output and do not advance until the step
is green. Secrets (wallet seed/key, note owner secret, the pool file) are never printed or committed.

If any command fails, run `dexdo doctor` first -- it reports the shellnet version, manifest
freshness, and whether your `policy.json` is complete.

**Prerequisites:** the user's own `--note-key` seed and access to SHELL for the planned purchases and
deployment costs. An existing deployed and funded `UpdateCustodianMultisigWallet_v2` v2.2.0 or
v2.4.0 wallet can be reused if it has exactly one custodian whose public key matches the supplied
funding key. Other wallet contract types are not supported. `dexdo note deploy` itself does not
create or fund the multisig. Buying also requires a live market with a seller ask; Phase 5 shows how
to discover one.

---

## Choose order semantics before committing money

Use the mapping below before offering a command. `N` is the requested number of ticks; one tick is
1,000,000 tokens. `L` is the buyer's limit price per tick, and `A` is the seller's actual ask at a
fill, with `A <= L`. For any price `p`, `fee(p) = floor(p * 250 / 10000)`,
`E_p = N * (p + fee(p))` is the fee-inclusive service deposit, and `T_p = E_p + 2p` adds the buyer
bond.

### "I want ongoing access, not a one-off purchase"

**Support:** Supported only as a fixed four-week subscription.

**Flags:** Place it with
`subscription --model A place --max-price-per-tick L --ticks N`, then use the matched deal with
`buyer --resume --frame-model A` and optional `--local-listen ADDR`. For an explicit due-week
booking, add `--settle` to `subscription ... status ID`; plain status is read-only. The seller must
run `seller --model A --subscription`. Both orders carry `AON|SUBSCRIPTION`.

**Money and time:** At placement, `T_L` leaves the note; cancelling while unfilled returns all of
it. At match, `T_A` enters the deal and the limit-price spread returns. The term is exactly four
weeks. Each week allows `N/4` ticks, and unused allowance expires at that week's boundary instead
of rolling forward. The take-or-pay clock starts when the probe is accepted, not when the order is
placed or matched. Before probe acceptance, no week is charged, but buyer STOP burns one probe
price. After acceptance, buyer STOP bills every elapsed week and the current week in full whether
or not its allowance was used. Future unstarted weeks return on a clean close, as does the `2A`
buyer bond. A dispute or penalty can instead burn stake `D` between `A` and `2A`. Resume reuses the
same match, places no new BUY, and preserves the subscription when the process exits.

**Refuse nearby:** There is no custom term, custom week length, rollover, subscription MARKET
order, or split across sellers. There is no `buyer --subscription` flag; placement must use the
`subscription` subcommand. Do not resume an order that is still resting.

### "Buy model A, with model B as fallback"

**Support:** Not supported. Say that before suggesting any command.

**Flags:** None. Each buyer or subscription invocation accepts one model, and separate invocations
create separate orders.

**Money and time:** Two submitted orders have independent escrows and either or both can fill. The
only safe sequence would be external logic that proves the A order is terminal and refunded before
submitting B; that sequence is not a shipped CLI capability.

**Refuse nearby:** There are no linked orders, order priority, OCO or cancel-on-fill, conditional
submission, cross-model atomicity, or automatic fallback. Never emulate fallback by placing two
live orders with the user's money.

### "Buy from whoever is cheapest right now"

**Support:** Supported within one model only when one ask can supply all `N` ticks.

**Flags:** For an immediate attempt, use
`buyer --frame-model A --ticks N --max-price-per-tick L` and omit `--wait-for-seller`, `--market`,
and `--token-contract`; this is `AON|FOK`. Add `--wait-for-seller` to permit a resting `AON` order.
`--market FILE` selects a manifest; it is not a MARKET-order flag.

**Money and time:** The client selects the lowest `(price, order_id)` at or below `L`, but that head
ask alone must cover `N`. A fill clears at ask `A`. The immediate form fills in full now or rejects
and refunds. The rest-capable form may fill on arrival; otherwise `E_L` remains in the book until
match, cancellation, or expiry. A fill spends `E_A` and posts the refundable `2A` buyer bond.

**Refuse nearby:** There is no buyer MARKET order, cheapest-across-models search, multi-seller
aggregation, or POST_ONLY mode. An undersized cheapest head is refused even if a later ask could
cover the whole order, and `--wait-for-seller` must not be described as POST_ONLY.

### "Spend no more than X"

**Support:** A direct total-budget order is not supported. The CLI supports an exact tick count
derived from `X`, `L`, and the formulas above.

**Flags:** For an ordinary order, use `--ticks N --max-price-per-tick L` and omit `--escrow`, or
pass exactly `E_L`. For a subscription, use
`subscription --model A place --ticks N --max-price-per-tick L`; it has no escrow flag.

**Money and time:** `L` is stated in whole SHELL a tick -- one SHELL is the book's price step, and
nothing finer is accepted. An ordinary
order freezes `E_L` in the book and requires the note balance to cover `T_L`; its `2A` bond is taken
only on fill. A subscription freezes `T_L` up front for the fixed four-week shape. For a total cap
`X` that includes collateral and satisfies `X >= 2L`, first reserve `2L`, divide what remains by
`L + fee(L)`, and round down:

```text
N <= floor((X - 2L) / (L + fee(L)))
```

Then require `N >= 2` for an ordinary order, or `4 <= N <= 40320` with `N` divisible by four for a
subscription. The actual service debit uses ask `A <= L`, and the bond returns on a clean close.

**Refuse nearby:** There is no `--budget` or fiat-budget flag, fractional tick, sub-SHELL price, or
arbitrary escrow; the CLI rejects both underfunding and overfunding. This bound covers SHELL for
this one order and deal, not native gas or any separately submitted order.

### "Cancel if unfilled in an hour"

**Support:** A user-selected BUY TTL is not supported. The shipped behavior is a fixed one-hour
deadline with lazy refund.

**Flags:** Add `--wait-for-seller` if an ordinary BUY may rest; without it, `FOK` ends the attempt
immediately. A subscription placement may rest. Before the deadline, use `orders ... cancel ID` or
`subscription ... cancel ID`. After it, use `orders ... expire ID`.

**Money and time:** Every CLI BUY deadline is placement time plus 3600 seconds. At the deadline the
order becomes non-fillable, but time alone sends no transaction. A crossing scan or a
permissionless expiry transaction must remove it. Removal returns all remaining BID escrow; with
no fill, there is no book trading fee. Funds can therefore remain recorded after the hour until a
removal transaction lands.

**Refuse nearby:** There is no selectable BUY TTL, CLI GTC, guaranteed refund transaction exactly
at 3600 seconds, automatic BUY sweep in the buyer process, or keeper reward. Seller expiry is a
different path and must not be promised for a buyer.

### "Sell only to subscribers"

**Support:** Supported with exact symmetric separation between subscription and ordinary orders.

**Flags:** The seller uses `seller --model A --subscription`; the buyer counterpart uses
`subscription --model A place`. Both carry `AON|SUBSCRIPTION`.

**Money and time:** Compatibility is checked before pricing or settlement. A subscription ask can
match only a subscription BUY, and an ordinary ask can match only an ordinary BUY. If the
subscription matches, the buyer's four-week, deposit, bond, billing, refund, and burn commitments
are exactly those stated in the first mapping above.

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
address deterministic. It is not the note owner key the buying commands below sign with; that one
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


A private note is the funded trading account used by the buyer commands below. One note can fund
trading up to its nominal, so choose the nominal for the amount that note needs to trade against.

The contract allows `N100`, `N1000`, `N10000`, `N100000`, and `N1000000`. The pinned SDK parser used
by the CLI accepts only `N100`, `N1000`, and `N10000` today. `N100000` and `N1000000` are
contract-legal but CLI-impossible. Therefore `N10000` is the largest note this tool can mint today.
To buy more, mint several notes.

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
into a pool file. Notes are funded in SHELL only, so `--token-type shell` is the only accepted
currency. The buyer note pays escrow plus gas, and `--nominal` is required with no default: pick
one of the three CLI-supported values above.

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
private, never commit it. `dexdo note deploy` is the user note-creation path; it creates or appends
the pool file that money-moving buyer commands require. Success is the block
that opens `Note deployed` and carries `address: <dapp>::<account>` and a `folded:` line naming the
pool; do not advance on an earlier progress line.

Before wallet onboarding, wallet funding, or chain preflight, dexdo atomically persists the fresh owner key in
`pn_pool.json.recovery.json` with owner-only permissions. A completed pool stores note credentials,
not funding-wallet provenance; notes funded by different multisigs may share it when nominal and
token type match.

Point the CLI at that same pool after deploy:

```sh
export DEXDO_PN_POOL="$PWD/pn_pool.json"
```

## Phase 4. Check the note

Do **not** copy the note's owner secret out of the pool. `dexdo note deploy` wrote it there beside
the address, and every command that signs for a note reads it back from that same pool entry, so
`--note-key` is not a flag you type. It stays available for the one case it was made for: a note
that lives outside any pool.

The read-only book commands still take the note address, so keep that to hand:

```sh
NOTE_ADDR=$(jq -r '.notes[-1].address' pn_pool.json)
```

Confirm the note holds SHELL for escrow + gas (read-only, no key):

```sh
dexdo note balance --note-addr "$NOTE_ADDR"
```

## Phase 5. Discover a live market

Start with the read-only market index. These commands need no note, wallet, or market file:

```sh
dexdo market-data list --output table --limit 20
dexdo market-data show <DAPP-ID>::<ACCOUNT-ID> --output json
dexdo market-data depth <DAPP-ID>::<ACCOUNT-ID> --output json --limit 50
```

Use `list` to find a live model market, then `show` and `depth` to inspect its identity and current
asks. Record the canonical frame model and the best ask in the CLI's integer price units. You can
buy by model without contacting a seller directly; the CLI selects an executable ask from the
model's order book.

If a seller gives you a specific deal instead, ask for either the `market.json` file or the
`token_contract` string (`<DAPP-ID>::<ACCOUNT-ID>`). With a bare `token_contract`, also get the canonical frame model
and pass both to the buyer.

The reading commands need no catalogue: `market`, `market-data`, `quote` and `orders` resolve a model
name against the on-chain ModelRegistry, and `markets address --model '<name>'` prints a market's book
without any local file.

Create `models.json` in the working directory before the `buyer` command itself. The buyer defaults
to this path and uses the entry for model identity verification -- that check is what stops a seller
substituting a cheaper model for the one you paid for. Without the file the buyer has no verification
data and fails closed on every model. There is no catalogue in the release archive: write your own,
using the shape below.

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
      "identity_aliases": ["Qwen/Qwen3.8-27B"],
      "fingerprints": [
        {
          "probe_prompt": "Reply with exactly: QWEN38",
          "expected_contains": "QWEN38"
        }
      ],
      "capabilities": { "max_output_tokens": 16384 }
    }
  }
}
```

`capabilities.logprobs` and `capabilities.top_logprobs` are retired: the client no longer requests, parses or
verifies log probabilities, so neither key affects verification. A `models.json` that still carries them keeps
loading -- they are **ignored, not rejected**, deliberately, so an older config does not break -- but writing
them does nothing.

## Phase 6. Check the price and cost before you buy

Set `--max-price-per-tick` from the real ask, not a guess. With the seller's `market.json` you can
read the book and price the deal read-only (writes nothing):

```sh
# The resting asks (SHELL per tick) and their deal addresses:
dexdo market Qwen3.8-27B --market market.json

# Executable cost for the ticks you intend to buy -- `total_with_fee` is the SHELL escrow you need:
dexdo quote --market market.json --ticks 8
```

If you only have a bare `token_contract` (no `market.json`), add `--note-addr "$NOTE_ADDR"` to these
two read-only commands (they use your note only to reach the chain, and sign nothing).

`dexdo buyer` also re-renders this book (with an `exec` column at your ceiling) right before it buys.

> **A ceiling below the ask does not always error -- and can look exactly like a stalled seller.**
> `--max-price-per-tick` must be **>=** the ask, or the order never crosses. On a model-only buy that
> fails fast (`no matchable ask`); on the `--market` / `--token-contract` path the buy can instead
> rest silently and the buyer just waits -- indistinguishable from the "seller did not open the
> stream" timeout below. Set the ceiling at or above the ask, and confirm `total_with_fee` from
> `dexdo quote` fits your note balance (Phase 4).

## Phase 7. Answer the failure policy (required before the buy)

The real `dexdo buyer` refuses to start without a complete policy at `~/.config/dexdo/policy.json`
(Windows `%APPDATA%\dexdo\policy.json`). You do not fill it in by hand: the first `dexdo buyer` run
on a terminal asks for it instead -- one situation at a time, in words, with the suggested answer
marked -- and writes the answers to that same file in that same format. It asks once, and nothing
asks again unless you change them. The eight things a buyer is asked about are the six failures
below plus how many sellers to try and how much may be spent in total across those attempts.

Without a terminal -- a script, a service unit, a CI job -- or with `--non-interactive`, there is no
interview: the buy refuses and says what is unanswered rather than hanging on a question nobody can
answer. So a scripted buyer wants this file complete before it starts, which means answering the
questions once on a terminal (or copying in a file that already has). A complete buyer section
has this shape:

```json
{
  "version": 1,
  "buyer": {
    "on": {
      "no_handover_after_match": "wait_then_reclaim",
      "malformed_handover": "reclaim",
      "dead_gateway": "retry_then_reclaim",
      "empty_stream": "reclaim",
      "seller_stalls_mid_stream": "accept_delivered_then_reclaim",
      "bad_output_scam": "dispute"
    },
    "failover": {
      "max_sellers_to_try": 3,
      "total_spend_cap_shells": 24600000000
    }
  }
}
```

Allowed values (the scaffold also lists these under `_legend.allowed`):

- `buyer.on.no_handover_after_match`: `wait_then_reclaim | next_seller | fail_closed`
- `buyer.on.malformed_handover`: `reclaim | dispute | fail_closed`
- `buyer.on.dead_gateway`: `retry_then_reclaim | next_seller | fail_closed`
- `buyer.on.empty_stream`: `reclaim | next_seller | fail_closed`
- `buyer.on.seller_stalls_mid_stream`: `accept_delivered_then_reclaim | dispute`
- `buyer.on.bad_output_scam`: `stop | dispute | stop_and_blacklist` -- use `stop`/`dispute`
  (`stop_and_blacklist` is not yet supported and fails closed when it fires).
- `buyer.failover.max_sellers_to_try`: integer >= 1.
- `buyer.failover.total_spend_cap_shells`: integer >= 1 in raw ECC[2] units (the field keeps its
  legacy name) -- total spend ceiling across failover. The example covers three attempts at the
  Phase 6 price/tick count (`3 x 8200000000` raw); otherwise size it from the quoted
  `total_with_fee`.

Confirm with `dexdo policy show`.

`policy.json` is the recovery control point. Read it with `dexdo policy show`; `dexdo policy init`
and `dexdo policy edit` remain for a file you want to scaffold or amend by hand. It decides how no
handover, malformed handover, a dead gateway, an empty or stalled stream, or suspected scam is
handled. A stop closes the deal and
honors finalized delivery, a dispute freezes only that TC's contested funds for arbitration, reclaim waits for the
contract timeout and returns eligible escrow, and `next_seller` performs bounded failover within the
configured seller and spend caps.

## Phase 8. Buy and bring up the local endpoint

```sh
dexdo buyer \
  --market market.json \
  --models models.json \
  --ticks 8 \
  --max-price-per-tick 1 \
  --local-listen 127.0.0.1:8080
```

The note is not on that command line. Before it buys, `dexdo buyer` offers the pool's notes as a
list you move through with the arrow keys and pick with Enter: each row is the address in the
canonical `dapp::account` form, shortened, beside what that note holds in SHELL -- read the same way
`dexdo note balance` reads it. The owner key comes from the entry you picked. A run with nobody to
answer -- no terminal, `--json`, or `--non-interactive` -- does not ask: it refuses and names
`--note-addr` as the flag that carries the answer, which is what a script passes.

The value `1` is one SHELL a tick. Replace it with the best ask you recorded in Phase 5 or a nearby
ceiling, stated the same way -- a whole number of SHELL. The automatic escrow is calculated from your ceiling, not from the eventual
fill price, so an unnecessarily high ceiling locks unnecessarily high escrow.

To buy from the discovered model order book without `market.json`, use:

```sh
dexdo buyer \
  --frame-model Qwen3.8-27B \
  --models models.json \
  --ticks 8 \
  --max-price-per-tick 1 \
  --local-listen 127.0.0.1:8080
```

For a specific bare deal, add `--token-contract <DAPP-ID>::<ACCOUNT-ID>` to that model command. `--ticks` is how
many ticks you buy. `--max-price-per-tick` is your per-tick ceiling in whole SHELL
(`--max-price-per-tick 1` is one SHELL a tick); it must be a whole number of them and at least the
ask.
Escrow is computed automatically as `ticks x max-price-per-tick` plus the book fee. Do not set
`--escrow` without a reason:
over-funding a resting buy can strand the surplus. Wait for the line
`consumer API listening (loopback)` -- the endpoint is ready.

Two flags worth knowing:

- `--allow-unverified-model`: named here only so a refusal that mentions it is recognisable. Do not
  offer it to the user as a step. It waives the check that proves the served model is the one bought,
  so it is the user's decision to raise, not the agent's to suggest.
- `--continuity-mode` (default `proactive`): with `--local-listen` left running, `proactive` keeps a
  warm next deal ready and **may pre-buy while idle**, spending the probe/idle cost even with no
  requests. Use `--continuity-mode on-demand` to hold on idle (the first request after idle then
  waits for a fresh deal).

## Phase 9. Use the model

The request `model` field must equal the deal's frame model (`Qwen3.8-27B`) or be omitted.

```sh
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"Qwen3.8-27B","messages":[{"role":"user","content":"hello"}],"stream":true}'
```

For OpenAI-compatible tools and SDKs, point them at the local endpoint and set the model to
`Qwen3.8-27B`:

```sh
export OPENAI_BASE_URL="http://127.0.0.1:8080/v1"
export OPENAI_API_KEY="local"   # loopback: any value; the key is not checked
```

## Phase 10. Check status (by-fact accounting)

`dexdo monitor` is a seller-side tool (it needs the seller's `market.json`). As a buyer, use your
own deal handles instead. List your deals (secret-free, reads local handles):

```sh
dexdo history --note "$NOTE_ADDR"
```

Then read one deal's by-fact state on-chain (reads the chain, moves nothing) -- pass the
`token_contract` (or handle) from `history`:

```sh
dexdo status <DAPP-ID>::<ACCOUNT-ID>
```

It shows how much SHELL you paid (`finalized_owed`), the lifecycle `state=`
(`placed`/`probe`/`streaming`/`stopped`/`disputed`) with boolean flags (`funded`/`opened`/
`probe_accepted`), and what buyer escrow remains held in that TC (`buyer_locked`, <= 2 ticks -- the invariant). Stream responses
also carry `usage` per request. Run these in a separate terminal while the buyer is up.

## Phase 11. Anti-abuse and recovery

Each deal keeps its own buyer escrow and seller `2P` bond in that TokenContract. A dispute freezes
only that deal's contested amount and bond; it does not lock either actor's whole PrivateNote, so
independent deals remain possible.

Use the recovery action that matches the failure:

```text
+--------------------------------------+-------------------------------------+-------------------------------------------------------------+
| Situation                            | Command                             | What it gives you                                           |
+--------------------------------------+-------------------------------------+-------------------------------------------------------------+
| Buyer process died on an OPEN deal   | dexdo recover                       | Stops the orphan; finalized delivered ticks are still paid. |
| Seller no-show or deal never opened  | dexdo reclaim                       | Returns eligible escrow after the contract timeout.         |
| Fraud or model substitution observed | dexdo dispute                       | Freezes this deal's contested funds pending resolution.     |
+--------------------------------------+-------------------------------------+-------------------------------------------------------------+
```

The three buyer deal commands take either `--token-contract <DAPP-ID>::<ACCOUNT-ID>` or
`--market market.json`; the note and its owner key come from the pool record of the deal, so neither
`--note-addr` nor `--note-key` is something you type. For example:

```sh
dexdo recover --token-contract <DAPP-ID>::<ACCOUNT-ID>
```

Replace `recover` with `reclaim` or `dispute` for those situations. Resolve a disputed TC with the
seller's `release-dispute` path or arbitration; no whole-note unlock or force-clear step exists.

## Wrap-up

Stop `dexdo buyer` (Ctrl-C) when the session is done -- the deal closes cleanly and leftover escrow
returns to the note. Your on-chain exposure per open deal never exceeds 2 ticks. Qualifier: under the
default `proactive` continuity, a `--local-listen` buyer left running idle may keep pre-buying fresh
deals (extra probe/idle spend beyond that 2-tick lock), so stop it -- or use `--continuity-mode
on-demand` -- when you are not actively sending requests.

---

## Common errors

- `policy (...) is missing or unreadable ... Run dexdo policy init` (or `... is incomplete`) -- the buyer
  policy is absent or still has `UNSET`/invalid fields, and this run had nobody to ask. Re-run it on
  a terminal and answer the questions (Phase 7), or fill the file and check it with
  `dexdo policy show`.
- `the note to spend from has to be chosen, and this run cannot ask: pass --note-addr` -- the run is
  not on a terminal (or carries `--json` / `--non-interactive`), so the note list was not offered.
  Pass `--note-addr` (Phase 4), or run it where it can ask.
- `provide --token-contract or --market` -- pass the deal address or the manifest (Phase 5).
- `the seller did not open the stream / did not write the handover within ...s` (or `timed out
  waiting for InferenceFilledConfirmed`) -- the match or handover did not complete. **Do not re-run
  the buy verbatim** -- the escrow may already be committed, so a fresh buy would double-pay. Instead
  reconnect with `--resume`, which re-scans your own note's fill event and serves the already-matched
  deal without new escrow:

  ```sh
  dexdo buyer --resume \
    --frame-model Qwen3.8-27B \
    --local-listen 127.0.0.1:8080
  ```

  (`--resume` also accepts `--market`/`--token-contract`.) If no match happened at all, check the
  seller is up on the same manifest and that your `--max-price-per-tick` was >= the ask (Phase 6).
- request rejected as `outside the configured frame` -- send `model` as `Qwen3.8-27B` (or omit it).

## Hard rules

- Never print, log, or commit the wallet seed/key, the note owner secret (`owner_secret_key_hex`),
  or the pool file.
- Never re-run a timed-out buy verbatim -- reconnect with `--resume` (a fresh buy double-pays).
- Never let a note withdrawal or sweep infer its destination: supply canonical
  `--to <DAPP-ID>::<ACCOUNT-ID>` explicitly every time.
- Do not set `--escrow` by hand without a reason (risk of stranded surplus).
- Buy against the deal `token_contract`, not an order-book address.
- Your on-chain lock per open deal never exceeds 2 ticks.
