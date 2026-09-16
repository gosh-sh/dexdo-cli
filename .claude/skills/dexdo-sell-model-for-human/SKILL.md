---
name: dexdo-sell-model-for-human
description: Teaches a SELLER to run the dexdo client themselves, by hand, without an agent doing it for them -- what you are actually selling and what you are actually risking, how to fund the operational wallet in the two legs the chain forces, how to mint a private note and read its two separate balances, how to price an offer against the live order book, how to provision one deal and run the gateway that serves it, which terminal must stay open and what happens when you close it, which steps cannot be undone, and how to read your own revenue back from the chain instead of being told. Written for a person running the commands themselves. If you would rather describe what you want and have an assistant run each command and report back, read `dexdo-sell-model-for-agent` instead. To keep several sellers running on one machine, read `seller-ops-onboarding`. To install the client first, read `dexdo-install`.
---

# Selling model access, by hand

This document teaches you to run `dexdo` yourself. It shows the same commands the assistant-driven
version runs, but it is written on the assumption that nobody is watching the output for you: every
step says what you should see, how to tell it worked, and what to do when it did not.

Work through it in order the first time. After that, the sections stand on their own.

---

## 1. What you are selling, and what you are risking

You are selling **inference by the tick**. A buyer locks SHELL in a per-deal contract, your gateway
streams model output, and each delivered tick moves part of that lock from the buyer's side to
yours. Nobody holds your money for you and nobody can release it early.

Four things are worth understanding before you spend anything, because each of them has cost
somebody money:

**A note has two balances, and only one of them is your revenue.** A private note holds SHELL (the
currency you earn) and native gas (what it spends to send messages). A note full of SHELL with an
empty gas pocket cannot do anything at all, and the refusal it gives does not look like a funding
problem. Read both with `dexdo note balance`.

**SHELL sent as native can never be spent as currency again.** The wallet funding below has two
legs, and they carry different flags for exactly this reason. Sending the whole nominal on the first
leg does not "pre-fund" anything; it converts your money into gas.

**Your gateway's advertised address is what a buyer dials.** If it is not publicly reachable, the
buyer cannot reach you -- and the client refuses to post the offer at all rather than leave a
resting ask pointing nowhere.

**Closing a deal burns its remainder.** `dexdo destroy` selfdestructs the per-deal contract. Size
the deposit to the deal, not to the note.

---

## 2. Before you start

Five things. Check each yourself rather than assuming:

| What | How you check it |
|---|---|
| the client is installed and green | `dexdo doctor` |
| a seed file you control | it is a file you wrote; the client never invents it |
| SHELL to fund the wallet and the note | your own funding wallet's balance |
| a model access key, e.g. `GROQ_API_KEY` | `echo "${GROQ_API_KEY:+set}"` prints `set` |
| a `models.json` naming the model you serve | you write it; see section 5 |

`dexdo doctor` is the command to run first whenever anything later behaves strangely. It reports the
chain version, whether the manifest matches it, and whether your policy file is complete. Most
confusing failures answer to it in one line.

If `dexdo doctor` is not green, stop here and read `dexdo-install`.

---

## 3. Fund the operational wallet

The operational wallet is where your notes are minted from. Its address is derived from your seed,
so it is deterministic: the same seed always gives the same address, and there is no separate wallet
identity to choose or write down.

`--note-key` here is the **wallet-derivation seed**, and it is yours to supply -- it is what makes
the address deterministic. It is *not* the note owner key the trading commands sign with; that one
the client writes and reads itself, out of the pool file in section 4. Two different secrets, and
the file names below say which is which.

Start by asking for the address and the amounts:

```sh
dexdo note wallet \
  --note-key /path/to/note.key \
  --nominal N1000
```

On a fresh seed this prints the address and both funding amounts without writing anything to the
chain. It uses no faucet. Read the two amounts off that output -- they are not the same figure, and
the order matters.

**Leg one: gas.** Send the smaller, stage-one amount to the printed address using the
**non-bounceable, flag-16** form. The account becomes `Uninit` with that amount as native balance.
This leg buys deploy gas and nothing else, which is why it is a small flat figure that does not grow
with your nominal.

**Deploy.** Run the same `dexdo note wallet` command again. With native balance present it deploys
the wallet, and the account becomes `Active`.

**Leg two: the money.** Send the stage-two amount to the now-`Active` wallet as **ECC[2] with the
flag-1** form, then run `dexdo note wallet` once more to confirm. This is the larger amount and the
one your nominal belongs to.

The order is forced by the chain, not by the client: ECC[2] cannot be delivered to an account that
does not exist, so the money leg cannot come first. Your funding wallet needs native for the
outbound message *and* ECC[2] for the note; a wallet holding only ECC[2] fails in the action phase
with `result_code=37` and `no_funds=true`, no message leaves, and every balance looks unchanged --
which reads exactly like nothing happened.

If you stop after leg one, the money is not lost: it sits at the deterministic address and becomes
reachable when you deploy the matching wallet. There is no automatic refund.

---

## 4. Mint the note

The address form here is not a typo. For a fresh operational wallet `dexdo note wallet` prints
`<ACCOUNT-ID>::<ACCOUNT-ID>`, because that wallet's dapp id **is** its own account id. Everywhere
else in this document an address reads `<DAPP-ID>::<ACCOUNT-ID>`. Paste what the client printed;
substituting a dapp id you found elsewhere produces an address that is not your wallet.

`--multisig-seed-file` takes the wallet's seed phrase, which is the same secret you gave as
`--note-key` above, in whatever file you keep it in. If you hold the 32-byte hex secret instead, use
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

`--nominal` has no default, deliberately: this is a real spend. Use `--multisig-private-key` with a
file holding the 32-byte hex secret if that is the form you keep.

Before wallet onboarding, wallet funding, or chain preflight, dexdo atomically writes the fresh owner key to
`pn_pool.json.recovery.json` with owner-only permissions. The completed pool holds note credentials,
not funding-wallet provenance: notes funded by different multisigs may share it when nominal and
token type match. The note's address and owner secret are written into `pn_pool.json`. **Do not copy
the secret out of it.** Every command that signs for a note reads it back from that file, which is
why you will not see a `--note-key` flag on the trading commands below. Treat `pn_pool.json` as a
secret file: never commit it, never paste it, never put it in a bug report.

Keep the address to hand -- the read-only commands take it:

```sh
NOTE_ADDR=$(jq -r '.notes[-1].address' pn_pool.json)
dexdo note balance --note-addr "$NOTE_ADDR"
```

If the deploy dies partway, it leaves `pn_pool.json.recovery.json`. Continue it with `dexdo note
recover` rather than deploying again -- a second deploy spends again.

---

## 5. Name the model you serve

Write a `models.json` describing the upstream you proxy, and set the access key in your environment
(`GROQ_API_KEY`, or the equivalent for your provider). The gateway forces the configured model: a
buyer cannot talk your gateway into serving something else.

Never put the key in `models.json`, in a command line, or in a file you commit. Command lines are
visible to every process on the machine and land in your shell history.

---

## 6. Price the offer against the live book

Look at the market before you pick a number:

```sh
dexdo market Qwen3.8-27B --note-addr "$NOTE_ADDR"
```

Read-only; it writes nothing and costs nothing. It prints the resting asks -- price per tick and
maximum ticks -- and the deal address behind each. To be taken by a buyer who is choosing on price,
you must be at or below the current best ask.

Prices are whole SHELL. One SHELL is the book's price step, and nothing finer is accepted, so
`--price-per-tick 3` means three SHELL a tick and there is no way to express less than one.

---

## 7. Answer the failure policy, once

Both `provision` and `seller` read the same policy file, so give it one explicit path and reuse it:

```sh
POLICY="${XDG_CONFIG_HOME:-$HOME/.config}/dexdo/policy.json"
dexdo policy init --role seller --path "$POLICY"
dexdo policy edit --path "$POLICY"
```

The policy is where you decide, in advance, what happens when things go wrong at runtime -- before
money is at stake rather than during. Answer it deliberately; it is not boilerplate.

Then check it, and check it again right before you provision:

```sh
dexdo policy validate --role seller --path "$POLICY"
```

`provision` repeats this same check itself before it touches the note key, connects, or submits
anything, so a policy problem always stops you for free.

---

## 8. Provision one deal

```sh
dexdo provision \
  --policy "$POLICY" \
  --frame-model Qwen3.8-27B \
  --nonce 1 \
  --price-per-tick 1 \
  --max-ticks 1024 \
  --deposit-shells 20 \
  --output market.json
```

Note that the note is **not** on that command line. Once the local checks pass, `provision` shows
you the pool's notes as a list -- arrow keys, Enter to pick -- each row being the address in
shortened canonical form beside what that note holds in SHELL. The owner key comes from the row you
picked. If there is no terminal to ask, or you passed `--non-interactive`, it refuses and names
`--note-addr` as the flag that carries the answer.

What the flags mean, in the terms that matter to you:

- `--nonce` is required and must be new for every deal. It derives the deal address, so reusing one
  is not a naming choice, it is a collision.
- `--price-per-tick` is whole SHELL per tick.
- `--max-ticks` bounds the deal. It is the ceiling on what this deal can ever deliver.
- `--deposit-shells` funds the one deploy your note still pays for. Leave it out and the client
  sizes it from `--max-ticks`, which is the deal's own requirement rather than a flat number.

**Do not set the deposit to the whole note.** The remainder burns when you `destroy` the deal, and
the note still needs SHELL to keep running.

The resulting `market.json` carries the deal address (`token_contract`), the model, and the nonce.

---

## 9. Run the gateway

```sh
dexdo seller \
  --policy "$POLICY" \
  --market market.json \
  --model qwen \
  --models models.json \
  --gateway-listen 0.0.0.0:8443 \
  --gateway-advertise <public-host>:8443
```

**This command holds the terminal.** It posts the offer, then waits for a match, opens the stream
and serves tick by tick. The wait is open-ended: the resting offer is not torn down because nobody
turned up yet. Close the terminal and you stop serving -- run it under `tmux`, `screen` or a service
manager if it needs to outlive your session. `seller-ops-onboarding` covers that properly.

Two addresses, and they are not the same thing:

- `--gateway-listen` is where the process binds locally.
- `--gateway-advertise` is what a remote buyer dials. It is written into the handover, so it must be
  publicly reachable and it must never be `127.0.0.1`.

Startup rejects an advertise address that cannot work -- bind-all, loopback, RFC1918, link-local,
CGNAT, the documentation and benchmarking ranges, multicast -- with
`error[E_ADVERTISE_NOT_PUBLIC] (config)`, and it does so **before** posting the offer, so a resting
ask never points somewhere unreachable. For testing on one machine or a LAN, and only for that, add
`--allow-private-advertise`.

Price and volume come from `market.json`. The seller's own `--price-per-tick` is ignored on the
`--market` path. To re-price, provision a new deal with a fresh `--nonce` and serve that file.

---

## 10. Hand the deal to the buyer

Give the buyer either the `market.json` file or the `token_contract` string from it, in the
`<DAPP-ID>::<ACCOUNT-ID>` form.

If you hand over the bare string rather than the file, you **must also tell them the canonical frame
model** -- `Qwen3.8-27B` -- because they need it as `--frame-model` alongside
`--token-contract`. Hand over the string without the model and their command cannot be completed.

---

## 11. Read your own revenue back

Do not rely on what a counterparty tells you. Both of these read the chain and move nothing:

```sh
dexdo status <DAPP-ID>::<ACCOUNT-ID>
```

Prints the lifecycle `state=` (`placed`, `funded-but-never-opened`, `probe`, `streaming`, `stopped`,
`disputed`), the flags (`funded`, `opened`, `probe_accepted`, `disputed`) and the accounting
(`finalized_owed`, `buyer_locked`, `deposit`).

```sh
dexdo monitor --market market.json
```

A roll-up: ticks delivered, SHELL received, collateral held or burned, and whether the deal closed.
Repeat `--market` for several markets. Run it in a second terminal -- the gateway is holding the
first one.

---

## 12. When it refuses

Refusals here are deliberate and each one names its own cause. The ones you are most likely to meet:

| What you see | What it means | What to do |
|---|---|---|
| `E_ADVERTISE_NOT_PUBLIC` | the address you advertised cannot be dialled from outside | give a real public address, or `--allow-private-advertise` for local testing only |
| `ERR_NOTE_BUSY` | the note is mid-operation | wait for it to finish; do not run a second command against the same note |
| `ERR_OPEN_ORDERS_EXIST` | resting orders still stand | `dexdo orders --model <NAME> --note-addr "$NOTE_ADDR" list`, then `... cancel <ID>` |
| `result_code=37`, `no_funds=true` | the funding wallet has ECC[2] but no native for the message | send native to the funding wallet |
| the note holds SHELL but nothing works | the gas pocket is empty | `dexdo note topup` fills it from your wallet |

When the cause is not obvious, run `dexdo doctor` before anything else.

---

## 13. What cannot be undone

Know these before you type them:

- **`dexdo destroy`** selfdestructs a stopped deal's contract. The unrecovered remainder is gone.
- **`dexdo note withdraw`** is one-shot and irreversible; its canonical
  `--to <DAPP-ID>::<ACCOUNT-ID>` is always explicit and is never inferred from the pool or funding wallet.
- **the first funding leg** converts SHELL into native gas permanently.
- **a spent `--nonce`** cannot be reused; the deal address is derived from it.

Everything else in this document is either read-only or repeatable.

---

## 14. Where to go next

- several sellers on one machine, logs, restarts, handover: `seller-ops-onboarding`
- having an assistant run all of this for you: `dexdo-sell-model-for-agent`
- the buyer's side of the same trade: `dexdo-buy-model-for-human`
