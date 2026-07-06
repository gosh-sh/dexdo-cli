---
name: dexdo-install
description: Install and verify the dexdo CLI so an agent (or a fresh machine) is ready to sell or buy model inference on the dexdo market (real Acki Nacki shellnet). Covers the one-line installer (or a source build), putting `dexdo` on PATH, fetching the deployed-contracts manifest, verifying with `dexdo --version` / `dexdo doctor`, and the prerequisites (a funded multisig wallet + seed, a model access key, `dexdo policy init`) needed before running the seller or buyer. Load this to get `dexdo` installed and green from scratch; then use the `dexdo-sell-model` skill to sell or `dexdo-buy-model` to buy.
---

# dexdo -- install and verify the CLI

Goal: end this skill with a working `dexdo` binary on PATH, the deployed-contracts manifest in place,
`dexdo doctor` green, and a clear list of what the operator must still provide before real trading.
Run each step, show its output, and do not advance until the step succeeds. Secrets (wallet seed/key,
note owner secret, provider API keys) are never printed, logged, or committed.

---

## Phase 1. Install the binary

One-line installer (primary):

```sh
# Linux / macOS
curl -fsSL https://get.dex.do/install.sh | sh
# Windows (PowerShell)
# irm https://get.dex.do/install.ps1 | iex
```

The installer downloads the latest release, verifies its SHA256 against the published `SHA256SUMS`,
unpacks it, and places `dexdo` on PATH (`~/.local/bin` on Linux/macOS, `%LOCALAPPDATA%\dexdo\bin` on
Windows). The Linux binaries are static musl and run on any distro (Ubuntu 20.04+, Debian, RHEL,
Alpine) with no glibc version requirement. If `dexdo` is not found after install, add its directory to
PATH (Linux/macOS: `export PATH="$HOME/.local/bin:$PATH"`) and restart the shell.

Build from source (alternative, needs Rust):

```sh
git clone https://github.com/gosh-sh/dexdo-cli && cd dexdo-cli
cargo build --release -p dexdo --features shellnet   # binary: target/release/dexdo
```

The `shellnet` feature is required for any on-chain command; a build without it fails closed with
`unavailable: build with --features shellnet`. The released binary already includes it.

## Phase 2. Verify the binary

```sh
dexdo --version   # prints the installed release, e.g. "dexdo 0.0.4"
dexdo --help      # lists the commands: note, provision, market, seller, buyer, quote, status, ...
```

Both must succeed (exit 0) before continuing.

## Phase 3. Fetch the deployed-contracts manifest

Every on-chain command needs `contracts/deployed.shellnet.json` in the working directory (it pins the
deployed contract addresses and the shellnet version). If you installed the binary (did not build from
source), download it once:

```sh
mkdir -p contracts
curl -fsSL https://raw.githubusercontent.com/gosh-sh/dexdo-cli/main/contracts/deployed.shellnet.json \
  -o contracts/deployed.shellnet.json
```

A source checkout already ships this file.

## Phase 4. Health check

```sh
dexdo doctor --contracts contracts/deployed.shellnet.json
```

`dexdo doctor` reports the reachable shellnet version, whether your manifest is fresh (matches the
deployed contracts), and whether your `policy.json` is complete. A green doctor means the binary,
manifest, and network are ready. If it flags manifest drift, re-download the manifest (Phase 3). If it
flags shellnet unreachable, check network access to `shellnet.ackinacki.org`.

## Phase 5. Prerequisites for real trading (before seller / buyer)

The binary is ready, but real trading needs these from the operator -- gather them now so the sell/buy
flow does not stall:

1. A deployed **multisig wallet** holding test tokens, plus its **seed phrase** (or a key file). This
   funds note deploys and per-deal markets. Keep the seed/key private -- pass it by file path, never
   inline.
2. A **model access key** for the seller only (for example `GROQ_API_KEY`), exported in the
   environment (`export GROQ_API_KEY=...`), never written to logs or files that get committed.
3. A completed **failure policy**. Scaffold and fill it now:

   ```sh
   dexdo policy init --role seller    # or --role buyer
   dexdo policy show
   ```

   The real `dexdo seller`/`dexdo buyer` refuse to start until every field is set (no `UNSET`). For a
   seller, `seller.max_open_deals` must be exactly `1`. Use the allowed values listed under
   `_legend.allowed` in the scaffold.
4. A **private note** (wallet-funded, no giver) once the wallet is ready:

   ```sh
   dexdo note deploy --multisig-address 0:<WALLET> --multisig-seed-file /path/to/wallet.seed \
     --nominal N10000 --token-type nackl --endpoint shellnet.ackinacki.org --pool pn_pool.json
   ```

   `pn_pool.json` holds the note owner secret -- keep it private, never commit it.

## Phase 6. Run it -- work end to end

After Phase 5 (wallet, key, policy, note), pull the note address and owner secret out of the pool (the
secret goes to a `0600` file, never the screen):

```sh
NOTE_ADDR=$(jq -r '.notes[-1].address' pn_pool.json)
jq -r '.notes[-1].owner_secret_key_hex' pn_pool.json > note.secret.hex
chmod 600 note.secret.hex
```

### Sell (seller side)

Needs a `models.json` mapping your model (frame id, upstream base_url, served_model, `api_key_env`).
Read the current price, provision one per-deal market, then run the gateway:

```sh
dexdo market qwen--qwen3--32b --note-addr "$NOTE_ADDR" --contracts contracts/deployed.shellnet.json
dexdo provision --note-addr "$NOTE_ADDR" --note-key note.secret.hex --frame-model qwen--qwen3--32b \
  --nonce 1 --price-per-tick 1000 --max-ticks 1024 --deposit-shells 20 --output market.json \
  --contracts contracts/deployed.shellnet.json
export GROQ_API_KEY=<your-key>
dexdo seller --market market.json --model qwen --models models.json \
  --note-addr "$NOTE_ADDR" --note-key note.secret.hex --gateway-listen 0.0.0.0:8443 \
  --contracts contracts/deployed.shellnet.json
```

Hand the buyer the deal address (`token_contract` in `market.json`) and the frame model
`qwen--qwen3--32b`. Check revenue: `dexdo status 0:<TC> --contracts contracts/deployed.shellnet.json`
or `dexdo monitor --market market.json --contracts contracts/deployed.shellnet.json`.

### Buy (buyer side)

Read an executable quote, place the buy, then send OpenAI-style requests to the local listener:

```sh
dexdo quote --market market.json --ticks 8 --contracts contracts/deployed.shellnet.json
dexdo buyer --market market.json --note-addr "$NOTE_ADDR" --note-key note.secret.hex \
  --ticks 8 --max-price-per-tick 1000 --local-listen 127.0.0.1:8080 \
  --contracts contracts/deployed.shellnet.json
# in another shell, send OpenAI-style requests to the buyer's local endpoint:
curl http://127.0.0.1:8080/v1/chat/completions -H 'content-type: application/json' \
  -d '{"model":"qwen--qwen3--32b","messages":[{"role":"user","content":"hi"}]}'
```

`--max-price-per-tick` must be `>=` the ask or the order never crosses. Check the deal with
`dexdo status 0:<TC> --contracts contracts/deployed.shellnet.json`.

## Next (full flows + recovery)

- To SELL model access: load the `dexdo-sell-model` skill -- models.json, pricing, provision, the
  gateway, status/monitor accounting, and wrap-up.
- To BUY model access: load the `dexdo-buy-model` skill -- quote depth, price ceilings, continuity
  modes, using the model, and recovery/resume.

## Hard rules

- Never print, log, or commit the wallet seed/key, the note owner secret (`owner_secret_key_hex`), the
  pool file, or any provider API key.
- Every on-chain command takes the same `contracts/deployed.shellnet.json`; a mismatch between two
  sides is diagnosed by `dexdo doctor`.

## Common install errors

- `dexdo: command not found` after install -- the install directory is not on PATH; add it and restart
  the shell (Linux/macOS: `~/.local/bin`).
- `unavailable: build with --features shellnet` -- a source build compiled without the feature; rebuild
  with `--features shellnet` (Phase 1). The released binary already includes it.
- `dexdo doctor` reports manifest drift -- re-download `contracts/deployed.shellnet.json` (Phase 3).
- On an older Linux the released binary still runs (static musl); if a self-built glibc binary fails
  with `GLIBC_... not found`, use the released musl binary instead of a local glibc build.
