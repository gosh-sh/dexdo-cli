---
name: dexdo-buy-model
description: Guides a BUYER end-to-end through buying model inference on the dexdo market (real shellnet) -- install the client, deploy a wallet-funded private note, prepare the note key, receive the deal address from the seller (market.json or token_contract), read the real price with `dexdo market`/`dexdo quote`, fill the required failure policy (`dexdo policy init --role buyer`), run `dexdo buyer --local-listen` (places the buy and brings up a local OpenAI-compatible endpoint), use the purchased model from any OpenAI client (curl / OPENAI_BASE_URL), and check by-fact accounting (`dexdo status`/`dexdo history`) -- how much SHELL was paid, how many ticks were received, and what is locked. Load this when the user wants to BUY a model, connect to a seller, use someone else's model locally, or check what they paid for. For the seller side, use the `dexdo-sell-model` skill.
---

# dexdo -- buying model access (buyer side)

Walk the buyer through the real shellnet flow: install -> note -> price -> policy -> buy -> stream
the model locally -> status. After each command, show the output and do not advance until the step
is green. Secrets (wallet seed/key, note owner secret, the pool file) are never printed or committed.

If any command fails, run `dexdo doctor` first -- it reports the shellnet version, manifest
freshness, and whether your `policy.json` is complete.

**Prerequisite:** a deployed multisig wallet holding test tokens plus its seed phrase (or key file).
Buying works only after the seller has stood up their side and given you a deal address.

---

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
cargo build --release -p dexdo --features shellnet   # binary: target/release/dexdo
```

Verify with `dexdo --help`. Every command defaults to the deployed-contracts manifest at
`contracts/deployed.shellnet.json` in the working directory; if you installed the binary (did not
build from source), download it once:

```sh
mkdir -p contracts
curl -fsSL https://raw.githubusercontent.com/gosh-sh/dexdo-cli/main/contracts/deployed.shellnet.json \
  -o contracts/deployed.shellnet.json
```

## Phase 2. Deploy a private note

`dexdo note deploy` funds a fresh private note from your multisig wallet (no giver) and folds it
into a pool file. The buyer note pays escrow plus gas, so pick a nominal with enough SHELL (a larger
`N...` = more SHELL).

```sh
dexdo note deploy \
  --multisig-address 0:<WALLET-ADDRESS> \
  --multisig-seed-file /path/to/wallet.seed \
  --nominal N10000 \
  --token-type nackl \
  --endpoint shellnet.ackinacki.org \
  --pool pn_pool.json
```

Use `--multisig-key /path/to/wallet.key` (a file with the 32-byte hex secret) instead of
`--multisig-seed-file` if you hold the raw key. `pn_pool.json` holds the note owner secret -- keep it
private, never commit it.

## Phase 3. Prepare the note key

Pull the note address and owner secret out of the pool with `jq` (the secret goes straight to a
`0600` file, never to the screen). `--note-addr` = `$NOTE_ADDR`; `--note-key` = `note.secret.hex`.

```sh
NOTE_ADDR=$(jq -r '.notes[-1].address' pn_pool.json)
jq -r '.notes[-1].owner_secret_key_hex' pn_pool.json > note.secret.hex
chmod 600 note.secret.hex
```

Confirm the note holds SHELL for escrow + gas (read-only, no key):

```sh
dexdo note balance --note-addr "$NOTE_ADDR" --contracts contracts/deployed.shellnet.json
```

## Phase 4. Get the deal address from the seller

Ask the seller for one of two things: the `market.json` file OR the `token_contract` string
(`0:...`). Without it there is nothing to buy (the deal contract exists only after the seller's
offer). If the seller gives you the bare `token_contract`, also get the canonical frame model
(`qwen--qwen3--32b`) -- you pass it as `--frame-model` alongside `--token-contract`. Both sides must
use the same `contracts/deployed.shellnet.json`.

## Phase 5. Check the price and cost before you buy

Set `--max-price-per-tick` from the real ask, not a guess. With the seller's `market.json` you can
read the book and price the deal read-only (writes nothing):

```sh
# The resting asks (price per tick, in whole SHELL) and their deal addresses:
dexdo market qwen--qwen3--32b --market market.json --contracts contracts/deployed.shellnet.json

# Executable cost for the ticks you intend to buy -- `total_with_fee` is the SHELL escrow you need:
dexdo quote --market market.json --ticks 8 --contracts contracts/deployed.shellnet.json
```

If you only have a bare `token_contract` (no `market.json`), add `--note-addr "$NOTE_ADDR"` to these
two read-only commands (they use your note only to reach the chain, and sign nothing).

`dexdo buyer` also re-renders this book (with an `exec` column at your ceiling) right before it buys.

> **A ceiling below the ask does not always error -- and can look exactly like a stalled seller.**
> `--max-price-per-tick` must be **>=** the ask, or the order never crosses. On a model-only buy that
> fails fast (`no matchable ask`); on the `--market` / `--token-contract` path the buy can instead
> rest silently and the buyer just waits -- indistinguishable from the "seller did not open the
> stream" timeout below. Set the ceiling at or above the ask, and confirm `total_with_fee` from
> `dexdo quote` fits your note balance (Phase 3).

## Phase 6. Fill the failure policy (required before the buy)

The real `dexdo buyer` refuses to start without a complete policy at `~/.config/dexdo/policy.json`
(Windows `%APPDATA%\dexdo\policy.json`). Scaffold it, then set every field:

```sh
dexdo policy init --role buyer
```

This writes each required field as `UNSET`. Edit the file (or `dexdo policy edit`) and replace every
`UNSET` with a valid choice:

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
      "total_spend_cap_shells": 100000
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
- `buyer.failover.total_spend_cap_shells`: integer >= 1 (whole SHELL) -- total spend ceiling across
  failover; set it above one deal's escrow (`total_with_fee` from Phase 5).

Confirm with `dexdo policy show`.

`policy.json` is the recovery control point. Manage it with `dexdo policy init`,
`dexdo policy show`, and `dexdo policy edit`: it decides how no handover, malformed handover, a
dead gateway, an empty or stalled stream, or suspected scam is handled. A stop closes the deal and
honors finalized delivery, a dispute freezes both notes for arbitration, reclaim waits for the
contract timeout and returns eligible escrow, and `next_seller` performs bounded failover within the
configured seller and spend caps.

## Phase 7. Buy and bring up the local endpoint

```sh
dexdo buyer \
  --market market.json \
  --note-addr "$NOTE_ADDR" \
  --note-key note.secret.hex \
  --ticks 8 \
  --max-price-per-tick 1000 \
  --local-listen 127.0.0.1:8080 \
  --contracts contracts/deployed.shellnet.json
```

Without `market.json`, replace `--market market.json` with
`--token-contract 0:<ADDRESS> --frame-model qwen--qwen3--32b`. `--ticks` is how many ticks you buy;
`--max-price-per-tick` is your per-tick price ceiling in whole SHELL (1 SHELL = 1e9 raw) and must be
>= the ask. Escrow is computed automatically as about `ticks x max-price-per-tick x 1.025` (a 2.5%
book fee) -- do not set `--escrow` without a reason (over-funding a resting buy can strand the
surplus). Wait for the line `consumer API listening (loopback)` -- the endpoint is ready.

Two flags worth knowing:

- `--allow-unverified-model`: model families with no content-identity check cannot be bought on
  name-only evidence unless you pass this flag. The `qwen--qwen3--32b` family here has a check, so
  add it only if the buyer bails asking for it.
- `--continuity-mode` (default `proactive`): with `--local-listen` left running, `proactive` keeps a
  warm next deal ready and **may pre-buy while idle**, spending the probe/idle cost even with no
  requests. Use `--continuity-mode on-demand` to hold on idle (the first request after idle then
  waits for a fresh deal).

## Phase 8. Use the model

The request `model` field must equal the deal's frame model (`qwen--qwen3--32b`) or be omitted.

```sh
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"qwen--qwen3--32b","messages":[{"role":"user","content":"hello"}],"stream":true}'
```

For OpenAI-compatible tools and SDKs, point them at the local endpoint and set the model to
`qwen--qwen3--32b`:

```sh
export OPENAI_BASE_URL="http://127.0.0.1:8080/v1"
export OPENAI_API_KEY="local"   # loopback: any value; the key is not checked
```

## Phase 9. Check status (by-fact accounting)

`dexdo monitor` is a seller-side tool (it needs the seller's `market.json`). As a buyer, use your
own deal handles instead. List your deals (secret-free, reads local handles):

```sh
dexdo history --note "$NOTE_ADDR"
```

Then read one deal's by-fact state on-chain (reads the chain, moves nothing) -- pass the
`token_contract` (or handle) from `history`:

```sh
dexdo status 0:<TOKEN-CONTRACT> --contracts contracts/deployed.shellnet.json
```

It shows how much SHELL you paid (`finalized_owed`), the lifecycle `state=`
(`placed`/`probe`/`streaming`/`stopped`/`disputed`) with boolean flags (`funded`/`opened`/
`probe_accepted`), and what is locked (`buyer_locked`, <= 2 ticks -- the invariant). Stream responses
also carry `usage` per request. Run these in a separate terminal while the buyer is up.

## Phase 10. Anti-abuse and recovery

`dexdo note withdraw` can refuse with `ERR_STREAM_LOCKED (405)` while a stream or dispute is live.
It can also return `ERR_NOTE_BUSY (121)` when another note operation or stake state makes withdrawal
unsafe. These gates are intentional, not a bug: the note lock prevents either side from reusing the
same capital during a live deal or dispute, which makes wash trading, freeriding, and note hopping
unprofitable (spec sections 4.3 and 4.4).

Use the recovery action that matches the failure:

```text
+--------------------------------------+-------------------------------------+-------------------------------------------------------------+
| Situation                            | Command                             | What it gives you                                           |
+--------------------------------------+-------------------------------------+-------------------------------------------------------------+
| Buyer process died on an OPEN deal   | dexdo recover                       | Stops the orphan; finalized delivered ticks are still paid. |
| Seller no-show or deal never opened  | dexdo reclaim                       | Returns eligible escrow after the contract timeout.         |
| Fraud or model substitution observed | dexdo dispute                       | Locks both notes pending arbitration.                       |
| Withdrawal fails with 405            | dexdo note stream-locks             | Lists locks, deal addresses, and the force-clear deadline.  |
| Stale locks past the max-lock window | PrivateNote.forceClearStreamLocks() | Owner backstop clears stale locks after the deadline.       |
+--------------------------------------+-------------------------------------+-------------------------------------------------------------+
```

The three buyer deal commands take `--note-addr` and `--note-key`, plus either
`--token-contract 0:<TC>` or `--market market.json`. For example:

```sh
dexdo recover --note-addr "$NOTE_ADDR" --note-key note.secret.hex \
  --token-contract 0:<TOKEN-CONTRACT> --contracts contracts/deployed.shellnet.json
```

Replace `recover` with `reclaim` or `dispute` for those situations. Inspect a blocked withdrawal
without signing anything:

```sh
dexdo note stream-locks --note-addr "$NOTE_ADDR" \
  --contracts contracts/deployed.shellnet.json
```

First bring every listed deal to a clean end: stop or settle it, resolve its dispute, or let the
stream/dispute timeout expire. Once no stream or dispute lock remains, `dexdo note withdraw` passes
the lock gate. After the reported max-lock deadline, `PrivateNote.forceClearStreamLocks()` is the
owner-only backstop for stale locks. The current CLI reports that deadline but does not expose the
force-clear call as a subcommand or flag; it must be submitted with an owner-signing contract tool.

## Wrap-up

Stop `dexdo buyer` (Ctrl-C) when the session is done -- the deal closes cleanly and leftover escrow
returns to the note. Your on-chain lock per open deal never exceeds 2 ticks. Qualifier: under the
default `proactive` continuity, a `--local-listen` buyer left running idle may keep pre-buying fresh
deals (extra probe/idle spend beyond that 2-tick lock), so stop it -- or use `--continuity-mode
on-demand` -- when you are not actively sending requests.

---

## Common errors

- `policy (...) is missing or unreadable ... Run dexdo policy init` (or `... is incomplete`) -- the buyer
  policy is absent or still has `UNSET`/invalid fields. Run `dexdo policy init --role buyer` and
  fill every field (Phase 6).
- `--note-addr ... is required` / `provide --token-contract or --market` -- pass the note address or
  the deal address (Phases 3-4).
- `the seller did not open the stream / did not write the handover within ...s` (or `timed out
  waiting for InferenceFilledConfirmed`) -- the match or handover did not complete. **Do not re-run
  the buy verbatim** -- the escrow may already be committed, so a fresh buy would double-pay. Instead
  reconnect with `--resume`, which re-scans your own note's fill event and serves the already-matched
  deal without new escrow:

  ```sh
  dexdo buyer --resume \
    --frame-model qwen--qwen3--32b \
    --note-addr "$NOTE_ADDR" \
    --note-key note.secret.hex \
    --local-listen 127.0.0.1:8080 \
    --contracts contracts/deployed.shellnet.json
  ```

  (`--resume` also accepts `--market`/`--token-contract`.) If no match happened at all, check the
  seller is up on the same manifest and that your `--max-price-per-tick` was >= the ask (Phase 5).
- `unavailable: build with --features shellnet` -- only a source build compiled WITHOUT the feature.
  The released binary already includes shellnet; rebuild with `--features shellnet` (Phase 1).
- request rejected as `outside the configured frame` -- send `model` as `qwen--qwen3--32b` (or omit it).

## Hard rules

- Never print, log, or commit the wallet seed/key, the note owner secret (`owner_secret_key_hex`),
  or the pool file.
- Never re-run a timed-out buy verbatim -- reconnect with `--resume` (a fresh buy double-pays).
- Do not set `--escrow` by hand without a reason (risk of stranded surplus).
- Buy against the deal `token_contract`, not an order-book address.
- Your on-chain lock per open deal never exceeds 2 ticks.
