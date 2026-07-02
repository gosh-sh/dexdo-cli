---
name: dexdo-sell-model
description: Guides a SELLER end-to-end through selling model inference on the dexdo market (real shellnet) -- install the client, deploy a wallet-funded private note, configure the model access key and models.json, read the current price with `dexdo market`, provision a per-deal market (`dexdo provision` -> market.json), fill the required failure policy (`dexdo policy init --role seller`), run the `dexdo seller` gateway (posts the offer, forces the model, proxies the real upstream, streams tick by tick), hand the deal address to the buyer, and check by-fact accounting (`dexdo status`/`dexdo monitor`) -- how many ticks were delivered and how much SHELL was received. Load this when the user wants to SELL access to their model, stand up a seller gateway, serve buyers, or check revenue and delivered tokens. For the buyer side, use the `dexdo-buy-model` skill.
---

# dexdo -- selling model access (seller side)

Walk the seller through the real shellnet flow: install -> note -> price -> market -> policy ->
gateway -> status. After each command, show the output and do not advance until the step is green.
Secrets (wallet seed/key, note owner secret, the pool file, `GROQ_API_KEY`) are never printed or
committed.

If any command fails, run `dexdo doctor` first -- it reports the shellnet version, manifest
freshness, and whether your `policy.json` is complete.

**Prerequisites:** a deployed multisig wallet holding test tokens plus its seed phrase (or key
file), and a model access key (for example `GROQ_API_KEY`).

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
into a pool file. The note's SHELL funds the per-deal market deploys, gas, and runtime, so pick a
nominal with enough SHELL (a larger `N...` = more SHELL).

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

## Phase 3. Note key, balance check, models.json, and the upstream key

Pull the note address and owner secret out of the pool with `jq` (the secret goes straight to a
`0600` file, never to the screen). `--note-addr` = `$NOTE_ADDR`; `--note-key` = `note.secret.hex`.

```sh
NOTE_ADDR=$(jq -r '.notes[-1].address' pn_pool.json)
jq -r '.notes[-1].owner_secret_key_hex' pn_pool.json > note.secret.hex
chmod 600 note.secret.hex
```

Confirm the note actually holds SHELL before you spend it (read-only, no key):

```sh
dexdo note balance --note-addr "$NOTE_ADDR" --contracts contracts/deployed.shellnet.json
```

**Sizing:** the note's on-chain SHELL (its ECC currency-2 balance) must cover `--deposit-shells` for
the deal deploys (Phase 4, whole SHELL) plus runtime gas. If it is short, deploy a larger `--nominal`
(or another note). Provision fails closed if `--deposit-shells` exceeds this balance.

`models.json` in the working directory maps a model key to its canonical id, upstream, and metadata.
`frame_model` is the on-chain canonical id (the market name); `served_model` is sent upstream;
`api_key_env` names the env var holding the key. Add another model as a new entry.

```json
{
  "models": {
    "qwen": {
      "frame_model": "qwen--qwen3--32b",
      "base_url": "https://api.groq.com/openai/v1",
      "served_model": "qwen/qwen3-32b",
      "api_key_env": "GROQ_API_KEY",
      "tokenizer_family": "qwen",
      "price_per_tick": 1000,
      "capabilities": { "logprobs": true, "top_logprobs": 5 }
    }
  }
}
```

The `price_per_tick` here is decorative metadata -- it does NOT set the live deal price. The price
buyers pay is whatever you set at `dexdo provision --price-per-tick` (Phase 4); editing this field
changes nothing on-chain.

Export the upstream key (not written to logs): `export GROQ_API_KEY=<your-key>`

## Phase 4. Read the price, then provision the deal

First look at the model's shared order book (read-only, writes nothing) so you can price your offer
against the market:

```sh
dexdo market qwen--qwen3--32b --note-addr "$NOTE_ADDR" \
  --contracts contracts/deployed.shellnet.json
```

It prints the resting asks (price per tick, max ticks) and their deal addresses. `dexdo markets
--models models.json --note-addr "$NOTE_ADDR" --contracts contracts/deployed.shellnet.json` lists
every configured book. To be taken by a best-price buyer, price at or below the current best ask.

Now provision the per-deal `TokenContract`. Once per deal. `--nonce` is required and must be unique
per deal (it derives the deal address). `--price-per-tick`, `--max-ticks`, and `--deposit-shells`
are all in whole SHELL / ticks -- **1 SHELL = 1e9 raw**.

```sh
dexdo provision \
  --note-addr "$NOTE_ADDR" \
  --note-key note.secret.hex \
  --frame-model qwen--qwen3--32b \
  --nonce 1 \
  --price-per-tick 1000 \
  --max-ticks 1024 \
  --deposit-shells 20 \
  --output market.json \
  --contracts contracts/deployed.shellnet.json
```

`--price-per-tick` is the live tick price (whole SHELL); `--max-ticks` bounds the deal.
`--deposit-shells` (whole SHELL) funds the two deploys (RootModel + TokenContract, split ~half each),
defaults to ~20, and must fit the note balance from Phase 3 -- do not set it to the whole note (the
remainder burns at `destroy`, and the note still needs runtime SHELL). The result `market.json`
carries the deal address (`token_contract`), the model, and the nonce.

## Phase 5. Fill the failure policy (required before the gateway)

The real `dexdo seller` refuses to start without a complete policy at `~/.config/dexdo/policy.json`
(Windows `%APPDATA%\dexdo\policy.json`). Scaffold it, then set every field:

```sh
dexdo policy init --role seller
```

This writes each required field as `UNSET`. Edit the file (or `dexdo policy edit`) and replace every
`UNSET` with a valid choice:

```json
{
  "version": 1,
  "seller": {
    "on": {
      "buyer_no_show": "cleanup_and_retire",
      "after_deal_done": "retire",
      "dispute_against_me": "release_if_clean"
    },
    "max_open_deals": 1
  }
}
```

Allowed values (the scaffold also lists these under `_legend.allowed`):

- `seller.on.buyer_no_show`: `cleanup_and_republish | cleanup_and_retire` -- use `cleanup_and_retire`
  (the republish variant is not yet supported by the daemon and fails closed when it fires).
- `seller.on.after_deal_done`: `republish | republish_with_backoff | retire` -- use `retire` (the
  republish variants fail closed on completion).
- `seller.on.dispute_against_me`: `release_if_clean | hold`.
- `seller.max_open_deals`: integer >= 1, but **must be exactly `1`** -- the gateway owns one deal per
  process and refuses to start with any other value.

Confirm with `dexdo policy show`.

## Phase 6. Run the seller gateway

```sh
dexdo seller \
  --market market.json \
  --model qwen \
  --models models.json \
  --note-addr "$NOTE_ADDR" \
  --note-key note.secret.hex \
  --gateway-listen 0.0.0.0:8443 \
  --contracts contracts/deployed.shellnet.json
```

The offer price and volume come from the provisioned deal in `market.json` (set in Phase 4). The
seller's own `--price-per-tick` flag is **ignored on the `--market` path** -- to re-price, run a new
`dexdo provision` with a fresh `--nonce` and serve that manifest. `--gateway-listen` must be
reachable by the buyer; if the buyer is on another host, also pass `--gateway-advertise
<public-host>:8443` (the public address written into the handover -- never `127.0.0.1`). With
`--market`, do not also pass `--token-contract`/`--nonce` (they come from the file). On start the
gateway posts the offer, then daemonizes: it polls for a match, opens the stream, and streams tick by
tick. The wait for a buyer is open-ended -- the resting offer is not torn down.

## Phase 7. Hand the deal address to the buyer

Give the buyer either the `market.json` file OR the `token_contract` string (`0:...`) from it. If you
hand over the bare `token_contract` (not the file), you **must also give the buyer the canonical
frame model** `qwen--qwen3--32b` -- the buyer needs it as `--frame-model` alongside
`--token-contract`. The buyer places the buy; the gateway opens the stream automatically and forces
the configured model.

## Phase 8. Check status (by-fact accounting)

Authoritative deal state (reads the chain, moves nothing) -- pass the deal `token_contract` from
`market.json`:

```sh
dexdo status 0:<TOKEN-CONTRACT> --contracts contracts/deployed.shellnet.json
```

It prints the lifecycle `state=` (`placed`/`funded-but-never-opened`/`probe`/`streaming`/`stopped`/
`disputed`), the boolean flags (`funded`/`opened`/`probe_accepted`/`disputed`), and accounting
(`finalized_owed`, `buyer_locked`, `deposit`, ...). For a revenue roll-up across one or more markets:

```sh
dexdo monitor --market market.json --contracts contracts/deployed.shellnet.json
```

Read-only: ticks delivered, SHELL received, what is locked or burned, whether the deal is closed.
Repeat `--market` for several markets; run it in a separate terminal.

## Wrap-up

After the buyer closes (stops) the deal, close the deal contract to release resources (any leftover
deal gas burns cross-dapp):

```sh
dexdo destroy --market market.json --note-addr "$NOTE_ADDR" --note-key note.secret.hex \
  --contracts contracts/deployed.shellnet.json
```

Move the note's remaining token balance back to a wallet (rejected while stream-locked -- close the
deal first):

```sh
dexdo note withdraw --note-addr "$NOTE_ADDR" --note-key note.secret.hex \
  --to 0:<WALLET-ADDRESS> --contracts contracts/deployed.shellnet.json
```

---

## Common errors

- `policy (...) is missing or unreadable ... Run dexdo policy init` (or `... is incomplete`) -- the seller
  policy is absent or still has `UNSET`/invalid fields. Run `dexdo policy init --role seller`, fill
  every field (Phase 5), and remember `seller.max_open_deals` must be `1`.
- `--note-addr ... is required` / `--note-key ... is required` -- pass the note address and key (Phase 3).
- `--nonce <n> is required and must be UNIQUE per deal` -- set a new unique `--nonce` each deal.
- provision fails for lack of SHELL -- the note's ECC currency-2 balance is below `--deposit-shells`;
  deploy a larger `--nominal` or lower `--deposit-shells` (check `dexdo note balance`).
- `unavailable: build with --features shellnet` -- only a source build compiled WITHOUT the feature.
  The released binary already includes shellnet; rebuild with `--features shellnet` (Phase 1).
- buyer cannot connect -- check `--gateway-listen`/`--gateway-advertise` reachability and that both
  sides use the same `contracts/deployed.shellnet.json`. `dexdo doctor` diagnoses manifest drift.

## Hard rules

- Never print, log, or commit the wallet seed/key, the note owner secret (`owner_secret_key_hex`),
  the pool file, or `GROQ_API_KEY`.
- Do not trust the buyer request's `model` field -- the market forces the model from `--model`.
- `dexdo destroy` is destructive: run it only on a closed (stopped) deal.
