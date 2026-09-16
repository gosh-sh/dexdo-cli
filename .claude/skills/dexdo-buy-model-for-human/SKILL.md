---
name: dexdo-buy-model-for-human
description: Teaches a BUYER to run the dexdo client themselves, by hand, without an agent doing it for them -- what you are actually buying and what your escrow is exposed to, how to fund the operational wallet in the two legs the chain forces, how to mint a private note and read its two separate balances, how to find a live market and price a purchase before committing, how to set a per-tick ceiling that actually crosses the ask, how to bring up the local OpenAI-compatible endpoint and point ordinary tools at it, and how to read back what you paid and what is still locked. Written for a person running the commands themselves. If you would rather describe what you want and have an assistant run each command and report back, read `dexdo-buy-model-for-agent` instead. To install the client first, read `dexdo-install`.
---

# Buying model access, by hand

This document teaches you to run `dexdo` yourself. It shows the same commands the assistant-driven
version runs, but it assumes nobody is reading the output for you: every step says what you should
see, how to tell it worked, and what to do when it did not.

Work through it in order the first time. After that, the sections stand on their own.

---

## 1. What you are buying, and what your money is doing

You are buying **inference by the tick**. You lock SHELL in a per-deal contract, a seller's gateway
streams model output, and each delivered tick moves part of your lock to the seller. What is not
delivered stays yours.

Four things decide whether this goes well, and each has cost somebody money:

**Escrow is computed from your ceiling, not from the price you end up paying.** A ceiling set far
above the ask locks far more SHELL than the purchase needs. It is not lost, but it is not available
either.

**A ceiling below the ask does not always fail loudly.** Buying by model name fails fast with `no
matchable ask`. Buying a specific deal can instead rest quietly and wait -- and a waiting buyer looks
exactly like a seller who never opened the stream. Set the ceiling at or above the real ask.

**A note has two balances, and only one of them is money.** A private note holds SHELL and,
separately, native gas for sending messages. A note full of SHELL with an empty gas pocket cannot
act, and the refusal does not look like a funding problem.

**SHELL sent as native can never be spent as currency again.** The wallet funding below has two legs
carrying different flags for exactly that reason.

---

## 2. Before you start

| What | How you check it |
|---|---|
| the client is installed and green | `dexdo doctor` |
| a seed file you control | it is a file you wrote; the client never invents it |
| SHELL to fund the wallet and the note | your own funding wallet's balance |
| a live market with a seller ask | section 5 |

`dexdo doctor` is the first command to run whenever something behaves strangely: it reports the
chain version, whether the manifest matches, and whether your policy is complete. If it is not
green, stop and read `dexdo-install`.

---

## 3. Fund the operational wallet

The operational wallet is where your note is minted from. Its address is derived from your seed, so
it is deterministic and there is no separate wallet identity to choose.

`--note-key` here is the **wallet-derivation seed**, and it is yours to supply -- it is what makes
the address deterministic. It is *not* the note owner key the buying commands sign with; that one
the client writes and reads itself, out of the pool file in section 4.

```sh
dexdo note wallet \
  --note-key /path/to/note.key \
  --nominal N1000
```

On a fresh seed this prints the address and both funding amounts and writes nothing to the chain. It
uses no faucet. The two amounts differ and the order is forced:

1. **Gas leg.** Send the smaller, stage-one amount to the printed address in the **non-bounceable,
   flag-16** form. The account becomes `Uninit` with that as native balance. This buys deploy gas
   only, which is why it does not grow with your nominal.
2. **Deploy.** Run `dexdo note wallet` again. With native present it deploys, and the account
   becomes `Active`.
3. **Money leg.** Send the stage-two amount to the now-`Active` wallet as **ECC[2], flag 1**, then
   run `dexdo note wallet` once more to confirm.

ECC[2] cannot reach an account that does not exist, so the money leg cannot come first. Your funding
wallet needs native for the outbound message as well as ECC[2]: with ECC[2] only, the transaction
fails in its action phase with `result_code=37` and `no_funds=true`, nothing moves, and every
balance looks unchanged -- which reads exactly like nothing happened.

Stopping after leg one does not lose the money: it waits at the deterministic address until the
matching wallet is deployed. There is no automatic refund.

---

## 4. Mint the note

The address form here is not a typo. For a fresh operational wallet `dexdo note wallet` prints
`<ACCOUNT-ID>::<ACCOUNT-ID>`, because that wallet's dapp id **is** its own account id. Everywhere
else in this document an address reads `<DAPP-ID>::<ACCOUNT-ID>`. Paste what the client printed
rather than substituting a dapp id from elsewhere, which produces an address that is not your
wallet.

`--multisig-seed-file` takes the wallet's seed phrase -- the same secret you gave as `--note-key`
above, in whatever file you keep it in. If you hold the 32-byte hex secret instead, use
`--multisig-private-key`. A *different* seed here derives a *different* wallet, and the mint then
spends from an account you never funded.

```sh
dexdo note deploy \
  --multisig-address <ACCOUNT-ID>::<ACCOUNT-ID> \
  --multisig-seed-file /path/to/wallet.seed \
  --nominal N10000 \
  --token-type shell \
  --pool pn_pool.json
```

`--nominal` has no default on purpose: this is a real spend. Use `--multisig-private-key` with a
file holding the 32-byte hex secret if that is the form you keep.

Before wallet onboarding, wallet funding, or chain preflight, dexdo atomically writes the fresh owner key to
`pn_pool.json.recovery.json` with owner-only permissions. The completed pool holds note credentials,
not funding-wallet provenance: notes funded by different multisigs may share it when nominal and
token type match. The address and owner secret land in `pn_pool.json`. **Do not copy the secret
out.** Every command that signs reads it back from that file, which is why no `--note-key` appears
on the trading commands below. Treat the pool file as a secret: never commit it, never paste it,
never attach it to a report.

```sh
NOTE_ADDR=$(jq -r '.notes[-1].address' pn_pool.json)
dexdo note balance --note-addr "$NOTE_ADDR"
```

A deploy that dies partway leaves `pn_pool.json.recovery.json`. Continue it with `dexdo note
recover`; deploying again spends again.

---

## 5. Find a live market

These read-only commands need no note, no wallet and no market file:

```sh
dexdo market-data list --output table --limit 20
dexdo market-data show <DAPP-ID>::<ACCOUNT-ID> --output json
dexdo market-data depth <DAPP-ID>::<ACCOUNT-ID> --output json --limit 50
```

`list` finds a live model market; `show` and `depth` inspect its identity and current asks. Write
down two things: the **canonical frame model** and the **best ask** in whole SHELL. You can buy by
model without contacting any seller -- the client picks an executable ask from the book.

If a seller gives you a specific deal instead, ask for the `market.json` file, or for the
`token_contract` string **and the canonical frame model**. The bare string alone is not enough: you
need the model as `--frame-model` alongside `--token-contract`.

The reading commands below need no catalogue: they resolve a model name against the on-chain
ModelRegistry, and `dexdo markets address --model '<name>'` names a market's book without any local
file.

Before the `buyer` command itself, put a `models.json` in the working directory. The buyer defaults
to that path and uses the entry to verify the model's identity -- that check is what stops a seller
substituting a cheaper model for the one you paid for, so without the file the buyer fails closed on
every model. The release archive ships no catalogue, only `models.example.json` to copy and edit;
nothing loads it under that name.

---

## 6. Price it before you commit

Read-only; these write nothing:

```sh
dexdo market Qwen3.8-27B --market market.json
dexdo quote --market market.json --ticks 8
```

`market` shows the resting asks and their deal addresses. `quote` gives you `total_with_fee` -- the
SHELL escrow the purchase actually needs. Check it against your note balance from section 4 before
going further.

With only a bare `token_contract` and no `market.json`, add `--note-addr "$NOTE_ADDR"` to both. They
use your note to reach the chain and sign nothing.

Set your ceiling from the real ask you just read, not from a guess.

---

## 7. Answer the failure policy, once

```sh
POLICY="${XDG_CONFIG_HOME:-$HOME/.config}/dexdo/policy.json"
dexdo policy init --role buyer --path "$POLICY"
dexdo policy edit --path "$POLICY"
dexdo policy validate --role buyer --path "$POLICY"
```

This is where you decide in advance what should happen when a stream fails or a seller goes quiet --
before your money is committed rather than while it is. Answer it deliberately.

---

## 8. Buy, and bring up the local endpoint

With a seller's `market.json`:

```sh
dexdo buyer \
  --market market.json \
  --models models.json \
  --ticks 8 \
  --max-price-per-tick 1 \
  --local-listen 127.0.0.1:8080
```

Or straight from the model's order book, with no `market.json` at all:

```sh
dexdo buyer \
  --frame-model Qwen3.8-27B \
  --models models.json \
  --ticks 8 \
  --max-price-per-tick 1 \
  --local-listen 127.0.0.1:8080
```

For one specific deal, add `--token-contract <DAPP-ID>::<ACCOUNT-ID>` to the model form.

The note is not on the command line. Before buying, `dexdo buyer` shows the pool's notes as a list --
arrow keys, Enter to pick -- each row the shortened canonical address beside what it holds in SHELL.
The owner key comes from the row you pick. With no terminal to ask, or with `--json` or
`--non-interactive`, it refuses and names `--note-addr` as the flag that carries the answer.

What the numbers mean:

- `--ticks` is how many ticks you are buying.
- `--max-price-per-tick` is your ceiling in whole SHELL. It must be a whole number and **at least
  the ask**, or the order never crosses.
- escrow is `ticks x max-price-per-tick` plus the book fee, computed for you. Do not pass `--escrow`
  without a reason: over-funding a resting buy can strand the surplus.

**This command holds the terminal.** Wait for `consumer API listening (loopback)` -- that line is
how you know the endpoint is up. Close the terminal and the endpoint goes with it.

Two flags worth knowing before you meet them:

- `--allow-unverified-model` exists and `--help` describes it. This document does not walk you
  through it on purpose: it waives the check that proves the model you are served is the model you
  paid for, and for a new buyer that is almost never the right answer. If the client stops and names
  it, read what it says before reaching for it.
- `--continuity-mode` defaults to `proactive`, which keeps a warm next deal ready and **may pre-buy
  while idle**, spending probe and idle cost with no requests running. Use `--continuity-mode
  on-demand` to hold while idle; the first request after an idle period then waits for a fresh deal.

---

## 9. Use it like any OpenAI endpoint

The `model` field must be the deal's frame model, or omitted:

```sh
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model":"Qwen3.8-27B","messages":[{"role":"user","content":"hello"}],"stream":true}'
```

For OpenAI-compatible tools and SDKs:

```sh
export OPENAI_BASE_URL="http://127.0.0.1:8080/v1"
export OPENAI_API_KEY="local"
```

The endpoint is loopback-only, so the key is not checked and any value does.

---

## 10. Read back what you paid

Do not take a counterparty's word for it. Both read your own records or the chain and move nothing:

```sh
dexdo history --note "$NOTE_ADDR"
dexdo status <DAPP-ID>::<ACCOUNT-ID>
```

`history` lists your deals, secret-free, from local handles. `status` gives one deal's by-fact
state: what you paid (`finalized_owed`), the lifecycle `state=` (`placed`, `probe`, `streaming`,
`stopped`, `disputed`), the flags (`funded`, `opened`, `probe_accepted`), and what escrow is still
held (`buyer_locked`, which stays at or below two ticks by design).

`dexdo monitor` is a seller-side tool and needs the seller's `market.json`; it is not your view.

Run these in a second terminal -- the buyer is holding the first.

---

## 11. When something goes wrong

| What you see | What it means | What to do |
|---|---|---|
| `no matchable ask` | your ceiling is below every resting ask | re-read the book, raise the ceiling to the real ask |
| the buyer waits and nothing happens | either the ceiling never crossed, or the seller has not opened the stream | check the ask with `dexdo market`; the two look identical from here |
| the note holds SHELL but nothing works | the gas pocket is empty | `dexdo note topup` fills it from your wallet |
| `result_code=37`, `no_funds=true` | the funding wallet has ECC[2] but no native | send native to the funding wallet |

If a deal is stuck OPEN because your buyer process died while the note and key survived, `dexdo
recover` signs STOP on it without placing a new buy, so it can be closed. If you have evidence a
seller substituted or defrauded, `dexdo dispute` freezes the contested amount and the seller bond --
it is strictly stronger than `recover`, which still pays for what was delivered. If a deal was
funded but never opened, `dexdo reclaim` recovers the escrow once the match-open timer has passed.

When the cause is not obvious, run `dexdo doctor` first.

---

## 12. What cannot be undone

- **the first funding leg** turns SHELL into native gas permanently.
- **`dexdo note withdraw`** is one-shot and irreversible; its canonical
  `--to <DAPP-ID>::<ACCOUNT-ID>` is always explicit and is never inferred from the pool or funding wallet.
- **a committed buy** is committed; what is not delivered comes back to you, but the purchase is not
  cancelled by closing the terminal.

Everything else here is read-only or repeatable.

---

## 13. Where to go next

- having an assistant run all of this for you: `dexdo-buy-model-for-agent`
- the seller's side of the same trade: `dexdo-sell-model-for-human`
