---
name: seller-ops-onboarding
description: Ops runbook for standing sellers up on a machine and keeping them there -- install and verify the binary, bind the funding wallet, mint the note, provision one market per seller, post SUBSCRIPTION sell offers (fixed four-week term, volume divisible by four and capped at 40 320 ticks), run the gateway, cancel resting orders, and read what each process left on disk. Covers running SEVERAL sellers on one host, the exact meaning of every flag an operator types, where logs, note keys and wallet secrets live, and what breaks when the state directory is not given. Load this for deployment, capacity, incident work or handover -- not for a one-off demo sale, which is `dexdo-sell-model-for-agent`. Written for a person running the commands themselves: an operator at a shell, standing sellers up and keeping them there. To sell one model once by hand, read `dexdo-sell-model-for-human`; to have an assistant do it, `dexdo-sell-model-for-agent`.
---

# Standing up sellers: an operator's runbook

This is written for the person who runs the machine, not for the person selling one model once.
It assumes shell access, a funding wallet, and no knowledge of how dexdo is built inside.

`dexdo-sell-model-for-agent` walks one seller through one sale. This document is about the host: several
sellers side by side, which file holds which secret, what survives a restart, and which missing flag
costs money.

Every command below is real; run it against `--help` when in doubt, because the binary is the
authority and this text is not.

---

## 1. What a seller is, in terms of what runs

One seller is one long-lived process -- `dexdo seller` -- plus the state it owns on disk:

- a **note** (`PrivateNote`): the seller's money on chain. Deals pay it, and it funds its own gas;
- a **market** for the model, and one per-deal `TokenContract` per seller, made by `dexdo provision`;
- a **gateway**: the port the buyer connects to for the actual model stream;
- a **state directory**: everything the process writes down, including the note's owner secret.

The process posts a resting sell offer -- here a subscription one, reserving this seller's capacity
for one buyer over a fixed four-week term -- waits, serves the buyer that takes it, and closes the
deal. Kill it and the offer stays in the order book until its own deadline: the book does not know
the process died, and a buyer can still take it.

---

## 2. Install and prove the binary before spending anything

```
dexdo --version
dexdo doctor
```

The deployed-contracts manifest is the one `DEXDO_MANIFEST` names, so no path is typed here or
anywhere below. It is still accepted, and the help marks it temporary: the manifest names the
network and the endpoint, so neither is something an operator supplies.

`dexdo doctor` is read-only and reports whether the binary's contract generation matches what is
deployed on the network. Run it before every deployment and after every upgrade.

A binary built from a different generation than the deployed contracts does not fail early -- it
fails after `dexdo note deploy` has already spent the note deposit plus its gas. The check costs
seconds; the mistake costs a note.

---

## 3. One state directory per seller, and why it is not optional

```
dexdo seller --data-dir /srv/dexdo/seller1 ...
```

`--data-dir` is the root of everything one instance owns. Give each seller its own, on every command
that seller runs -- not just the gateway.

**Without `--data-dir`** the client falls back to the machine's platform data directory (one per
user, not per seller). Two sellers then share one directory, and sharing it means:

- one wallet binding for both, so the second seller spends from the first one's wallet;
- one note pool file, so `dexdo seller` picks up a note that belongs to the other instance;
- one deal-handle directory, so the deals of both mix into one list.

The client refuses the crudest form of this: the second process with the same directory stops with
`another seller instance is already using data directory <path>; choose a different --data-dir for
the second instance` -- an exclusive lock file `.dexdo-seller.instance.lock` inside the directory.
The lock catches two processes at once. It does not catch two sellers taking turns in the same
directory, and that is the case that quietly moves money from the wrong wallet.

Layout under the state directory, with what each file is:

| Path | What it is |
| --- | --- |
| `pn_pool.json` | the note pool: each note's address and **its owner secret**, which signs everything that note does |
| `pn_pool.json.recovery.json` | the record of an unfinished note deploy, **carrying the owner secret too** |
| `wallet/active/<network>.json` | the live wallet binding: provider, network, operational wallet address |
| `wallet/bindings/<id>/` | that binding's secrets: the operational wallet's key or recovery phrase |
| `wallet/archive/` | replaced bindings, kept rather than deleted because the old wallet may still hold money |
| `deals/` | this participant's deal records; `dexdo status`, `close` and `export` work from them |
| `endpoints.json` | the gateway address handed to the buyer |
| `policy.json` | the failure policy; a seller will not start without it |
| `models.json` | the model descriptions and the provider keys they name |

Rights: the client creates the state root and its private files owner-only (`0700` for directories,
`0600` for files). Keep it that way -- the note owner secret and the wallet key are readable text.

---

## 4. Funding wallet: bound once, never passed by hand

```
dexdo wallet onboard manual \
  --multisig-address <DAPP-ID>::<ACCOUNT-ID> \
  --multisig-private-key /srv/dexdo/seller1/wallet.key \
  --data-dir /srv/dexdo/seller1
```

The network is not on that command line and there is no flag for it. `DEXDO_MANIFEST` names the
manifest, the manifest names the network and the endpoint, and that is the only source. An earlier
version of this runbook showed `--network shellnet` here; the binary has no such flag and refuses
with `unexpected argument '--network' found`, so the wallet was never bound and every later spending
command answered `E_WALLET_NOT_CONFIGURED`. It also contradicted section 14 of this same
file, three pages down, which states the manifest rule correctly.

The binding is written into the state directory and used by every spending command afterwards, so
the wallet address never appears in day-to-day commands. Three providers exist --
`ackinacki-wallet`, `gosh-ai`, `manual`; the ops path is `manual`, an existing wallet the operator
already controls.

Facts an operator needs:

- **one binding per state directory.** A second `onboard` on the same directory is refused; replacing
  a binding is `dexdo wallet rebind`, and the previous one is archived, not deleted;
- **the key is referenced, not copied.** The binding stores the path to the secret file. Move or
  delete that file and the seller stops being able to spend, with the binding still looking healthy;
- **an explicit `--multisig-address` on a spending command overrides the binding.** Passing it "just
  in case" defeats the whole point, and on a machine with several sellers it is how one seller spends
  from another's wallet.

Check what is bound: `dexdo wallet` writes `wallet/active/<network>.json`; read that file, or run the
command that would spend and see it refuse with `E_WALLET_NOT_CONFIGURED` before touching the chain.

---

## 5. The note: the seller's money

```
dexdo note deploy --nominal N100 --token-type shell \
  --data-dir /srv/dexdo/seller1
```

- `--nominal` has no default on purpose: the deposit is a real spend from the funding wallet.
  `N100` ... `N1000000` are the denominations;
- the deployed note is appended to `pn_pool.json` in the state directory, and `dexdo seller` takes
  its note from there;
- a crash mid-deploy leaves `pn_pool.json.recovery.json`. Continue it with `dexdo note recover` --
  never with a second `note deploy`, which pays the deposit twice;
- gas is separate from trading money: `dexdo note topup` fills the note's gas pocket,
  `dexdo note transfer` moves trading balance between notes. `dexdo note balance` shows both.

The note deposit and its gas are the floor cost of one seller. A seller that will serve deals of a
given size needs the deposit, the gas, and the seller bond -- twice the tick price per open deal --
available at the moment a buyer takes the offer.

---

## 6. The market, one per seller

```
dexdo provision --frame-model <MODEL-NAME> --price-per-tick <PRICE> --max-ticks <VOLUME> \
  --output /srv/dexdo/seller1/market.json --data-dir /srv/dexdo/seller1
```

`--max-ticks` is the volume of one deal. For a subscription seller it also has to fit the four-week
term -- divisible by four and no greater than 40 320 -- see section 7; a volume that does not fit is refused
when the offer is posted, after this command has already spent the note's gas.

`dexdo provision` brings up the shared order book for the model if it is missing, deploys this
seller's own per-deal `TokenContract`, and writes the market manifest named by `--output`. It is
funded from the note, not from the wallet.

Which note is not on the command line. On a terminal `provision` -- and `dexdo seller` after it --
offers the notes recorded in this state directory's pool as an arrow-key list: the address in the
canonical `dapp::account` form beside what that note holds in SHELL, Enter to pick. The owner key
comes from the entry picked, so `--note-key` is not typed either. Under a service manager there is
no terminal and nothing to ask: see section 14 for the flags a script passes instead.

- the manifest is what `dexdo seller --market` reads afterwards, so keep it inside that seller's
  state directory;
- `--nonce` distinguishes several deals of one seller. With `--market` the nonce comes from the
  manifest -- do not pass both;
- the model name must exist in the on-chain model registry. `dexdo provision` refuses an unknown
  name, and it refuses **after** the note has been deployed, so check the name first with
  `dexdo markets` or `dexdo market <MODEL-NAME>`.

`dexdo deploy-market` lists a model without provisioning a deal. It is idempotent: the book address
is derived from the model, so a second run changes nothing.

---

## 7. The subscription sell offer

A subscription reserves this seller's capacity for one buyer over a fixed term. It is the offer this
operation posts, and it is one flag on the seller plus one rule about volume.

```
dexdo seller --data-dir /srv/dexdo/seller1 --subscription \
  --market /srv/dexdo/seller1/market.json \
  --model <MODEL-NAME> --models /srv/dexdo/seller1/models.json \
  --gateway-listen 0.0.0.0:8443 --gateway-advertise <PUBLIC-HOST>:8443 \
  --policy /srv/dexdo/seller1/policy.json
```

**The term is fixed at four weeks**, a week being 604 800 seconds. It is not a parameter: neither
the seller nor the buyer picks it.

**Volume is set at `dexdo provision --max-ticks` and must fit the term.** The client refuses to post
otherwise, naming the number it got:

- greater than zero;
- divisible by four, because the volume is delivered week by week over a four-week term;
- no greater than 40 320 ticks -- the physical ceiling of 10 080 ticks per week across four weeks.

So `--max-ticks 4000` posts (1000 ticks a week), `--max-ticks 4001` is refused, and `--max-ticks
41000` is refused. The weekly quota is what the buyer may draw in one week; an open final-claim
grace period after the fourth week is not a fifth week's quota.

A subscription offer is all-or-none from a single counterparty: one buyer takes the whole term or
nobody does. It rests in the book until such a buyer arrives or until its deadline.

**What the buyer does on the other side** -- worth knowing when a deal does not appear:

```
dexdo subscription place --ticks <VOLUME> --max-price-per-tick <PRICE-CEILING>
dexdo subscription status
dexdo subscription cancel
```

The buy also carries all-or-none plus the subscription shape, so the two sides match only when this
seller alone can cover the whole requested volume. A subscription offer sized below what buyers ask
for is passed over at any price rather than partially filled -- volume is a business decision here,
not a formality.

`dexdo executable-book <MODEL-NAME> --ticks <VOLUME> --max-price-per-tick <PRICE-CEILING>` shows what is takeable right now; `dexdo quote` prices a given
size against current depth. Use them to see whether the volume being offered is the volume being
asked for.

**An ordinary, non-subscription offer** is the same command without `--subscription`: a resting sell
with no term and no weekly quota, taken by an ordinary buy. Ordinary buys are all-or-none plus
fill-or-kill, so the same volume rule applies to them in practice, without the divisibility
constraint.

Sell offer parameters worth naming:

| Flag | What it decides |
| --- | --- |
| `--subscription` | offer to subscription buyers: a four-week term, taken whole by one of them |
| `--price-per-tick` (on `provision`) | the price of one tick (a million tokens) in whole SHELL |
| `--max-ticks` (on `provision`) | the volume for the whole term: divisible by four, no greater than 40 320 |
| `--model`, `--models` | which model is sold, and where its provider key comes from |
| `--gateway-listen` | what to bind locally |
| `--gateway-advertise` | what to write into the handover as the buyer's connection address |
| `--policy` | the failure policy; a seller will not start without it |

---

## 8. The advertised address is the one that fails at 3 a.m.

`--gateway-advertise` is what the buyer will dial. On the live network the client rejects an address
a remote buyer cannot reach -- bind-all, loopback, private, link-local, carrier-grade NAT -- **before
any offer is posted**, so a misconfigured host fails at startup instead of after taking money.

Set it to the host's public address and port, and make sure the port is open from outside. For a
same-host or LAN test there is `--allow-private-advertise`, and buyers off the host will not connect.

Check the address without any money in play: `dexdo gateway-check --endpoint <ADDRESS>
--tls-fingerprint <FINGERPRINT>` probes a decrypted endpoint with no note, no deal and no chain call.

---

## 9. Several sellers on one host

Give each one its own, all of them:

| What | Why it must be its own |
| --- | --- |
| `--data-dir` | otherwise one note pool, one wallet binding and one deal list for both |
| the wallet and its key | otherwise two sellers spend from one, and the balance runs out for both at once |
| the note | one note for two means both sellers' orders and deals sit on one account |
| the `--gateway-listen` port | two processes cannot bind the same port |
| the `--gateway-advertise` port | it is how the buyer arrives; a collision sends traffic to the neighbour |
| the market (`provision --output`) | the manifest carries this seller's own deal contract address |

Model config `models.json` and the deployed-contracts manifest are read-only inputs and may be
shared. Everything the process writes must not be.

Sanity check after deployment, per seller: `dexdo monitor --data-dir <DIR>` shows that seller's
own offers, deals and delivered ticks -- from its note, not from the machine.

---

## 10. Logs

The client writes to standard output and standard error; there are no log files of its own. Level is
`info` by default and set through `RUST_LOG` (for example `RUST_LOG=debug`).

Under a service manager, capture both streams per seller -- the journal of one process is the only
record of what its gateway did. In the acceptance suite each participant's output goes to its own
`logs/` directory for exactly this reason.

What the log does **not** contain: the cancellation of the seller's own resting order. The chain
holds that. Read it from the order book events, or with `dexdo orders list`.

---

## 11. Live operations

The model name and the note are given to `dexdo orders` itself -- `--model`, `--note-addr` -- and the
subcommand takes only the order id:

| Task | Command |
| --- | --- |
| what this note has resting in the book | `dexdo orders --model <NAME> ... list` |
| cancel one order | `dexdo orders --model <NAME> ... cancel <ID>` |
| cancel every order of this note | `dexdo orders --model <NAME> ... cancel-all` |
| sweep an order whose deadline has passed | `dexdo orders --model <NAME> ... expire <ID>` |
| find the deals when the process is lost | `dexdo orders --model <NAME> ... fills` |
| a deal's state and accounting | `dexdo status <DEAL-HANDLE-OR-ADDRESS>` |
| close an open deal from the seller side | `dexdo close <DEAL-HANDLE>` |
| take the money out of a closed deal | `dexdo withdraw-shell`, then `dexdo destroy` |
| what was sold and for how much | `dexdo history`, `dexdo monitor` |

`dexdo orders` is the one command that still needs its note written out: it filters the book by that
note and offers no list to pick from. The key that signs a cancel is not written out -- it comes
from the pool entry for that address.

Restart hygiene: stop the process, cancel its resting orders, then restart. A restarted seller does
not adopt the offer left by its predecessor -- that one rests until its deadline, and while it rests
a buyer can take it and find nobody serving.

---

## 12. What costs money when done wrong

- **no `--data-dir`** -- two sellers on one platform directory; the second spends from the first
  one's wallet, and the mistake is visible only in the wallet balance;
- **`--multisig-address` passed by hand on a bound instance** -- the binding is ignored, and the
  named wallet pays; on a multi-seller host that is somebody else's wallet;
- **second `note deploy` after a crash instead of `note recover`** -- the deposit is paid twice;
- **process killed without cancelling orders** -- the offer rests to its deadline, a buyer takes it,
  and the deal opens with no gateway behind it. On a subscription that buyer has just reserved this
  seller's capacity for four weeks;
- **a subscription seller stopped mid-term** -- the deal stays open with a weekly quota the buyer is
  entitled to draw; the capacity is reserved for the whole term whether or not anyone is serving it;
- **`--max-ticks` that does not fit the term** -- not divisible by four, or above 40 320: the offer is
  refused at posting, after `provision` has already deployed the deal contract and spent its gas;
- **model name not in the registry** -- `provision` refuses after the note is already deployed;
- **binary from another contract generation** -- everything looks fine until money has already moved;
  `dexdo doctor` says so in seconds;
- **secret files copied around** -- the note owner secret in `pn_pool.json` and the wallet key are
  plain text. Whoever has them can spend the note and the wallet.

---

## 13. Handover checklist

Per seller, before calling it deployed:

1. `dexdo --version` and `dexdo doctor` -- binary matches the deployed generation;
2. `--data-dir` exists, is `0700`, and belongs to this seller alone;
3. `wallet/active/<network>.json` names the intended wallet, and the key file it points at exists;
4. `pn_pool.json` holds this seller's note; `dexdo note balance` shows the trading record and the
   gas pocket separately;
5. the market manifest from `dexdo provision` is inside this state directory, and its `--max-ticks`
   fits the four-week term: divisible by four, no greater than 40 320;
6. `--gateway-advertise` reachable from outside the host -- verified, not assumed;
7. `dexdo orders --model <NAME> ... list` shows exactly the offers this seller intends to have resting,
   and `--subscription` is on if this seller sells subscriptions;
8. logs of the process are captured somewhere that survives a restart.

---

## 14. Turning this into a script

The point of doing it by hand once is to be able to automate it afterwards. Four things decide
whether the script works on the day the machine is rebuilt.

**Pass every flag explicitly, rely on no default.** A default is resolved from the environment the
command happens to run in: `--data-dir` falls back to the machine's platform directory, `--models`
to `models.json` in the current working directory, `--policy` and the note pool to whatever the
state directory turns out to be. Under a service manager the working directory is not what it was in
the shell, and that is where money goes to the wrong wallet. Write out `--data-dir`, `--models`,
`--policy`, `--market` on every invocation. The manifest is the exception, and it is not a flag at
all: `DEXDO_MANIFEST` holds the full path to the manifest file, and the manifest names the network
and the endpoint. Unset, the client reads the manifest installed beside the `dexdo` binary -- that
directory and no other -- so on a host where the binary was installed some other way, or replaced,
the network can move without the unit file changing. Export it once for the unit and that stops
being possible: the variable wins wherever it is set, and a path it names that does not exist is
refused against that path rather than falling back to the installed manifest.

**Say `--non-interactive`, and answer what it then refuses.** A script has nobody to answer a
question, and the client already treats a run whose input or whose screen is not a terminal exactly
as if the flag were there -- so under a service manager the behaviour is the same either way. A run
emitting `--json` is treated the same way again. Saying it makes
that explicit and makes the failure readable: instead of a hang, the run stops and names the flag
that carries the answer. Two answers have to be supplied that way, because on a terminal they are
asked for rather than typed:

- **the note** -- `--note-addr`, on `provision` and `seller`. Read it out of the state directory's
  `pn_pool.json`. The owner key is not one of them: it comes from the pool entry for that address;
- **the rules of engagement** (`policy.json`) -- there is no flag for these. They are asked once,
  as situations in words, on the first `dexdo seller` run that has a terminal. Answer them by hand on
  the machine before the service unit ever starts, or copy in a file that already carries them; a
  seller that meets them for the first time under systemd refuses to start.

**Assert the observable, not the exit code.** A zero exit says the command returned, not that the
state you wanted exists. After each step there is something to read:

| Step | What the script checks |
| --- | --- |
| wallet binding | `wallet/active/<network>.json` exists and names the provider and network expected |
| note deploy | `pn_pool.json` gained an entry with the note's address, and `dexdo note balance <ADDRESS>` answers |
| market provision | the file named by `--output` exists and parses, and carries the deal contract address |
| offer posted | `dexdo orders --model <NAME> --note-addr <NOTE> list` shows the order |
| gateway up | `dexdo gateway-check --endpoint <ADDRESS> --tls-fingerprint <FINGERPRINT>` passes |
| deal running | `dexdo status <HANDLE> --json` -- state and accounting in machine form |

Machine-readable output exists on `dexdo status`, `dexdo markets`, `dexdo quote` and
`dexdo subscription place` (`--json`). The rest is human text: read what you need from the files the
commands write, not by parsing the console.

**Know what a re-run does before you loop.** The difference is money:

| Command | What a re-run does |
| --- | --- |
| `dexdo doctor`, `dexdo orders ... list`, `dexdo note balance`, `dexdo markets` | change nothing; call them as often as you like |
| `dexdo deploy-market` | does nothing if the book is already deployed: the address is derived from the model |
| `dexdo wallet onboard` | refuses when a binding already exists; replacing one is `dexdo wallet rebind` and nothing else |
| `dexdo note deploy` | **pays the deposit again.** After a broken deploy use `dexdo note recover` on the `pn_pool.json.recovery.json` record |
| `dexdo provision` | deploys another deal contract and spends the note's gas |
| `dexdo destroy` | does nothing on an already destroyed deal, and that is not a failure |

**Branch on the named refusals, not on the message text.** The client names them with codes, and
codes survive a rewording:

| Code | What happened |
| --- | --- |
| `E_WALLET_NOT_CONFIGURED` | this state directory has no wallet binding; nothing reached the chain |
| `E_ADVERTISE_NOT_PUBLIC` | the advertised address is one a remote buyer cannot reach |
| `E_ADVERTISE_UNREACHABLE` | the advertised address did not answer its own check |
| `E_ADVERTISE_WRONG_GATEWAY` | something other than this gateway answers at the advertised address |
| `E_GATEWAY_UNREACHABLE` | the buyer could not reach the seller's gateway |
| `E_GATEWAY_WRONG_ENDPOINT` | the certificate at that address is not the one written into the handover |
| `E_SELLER_POOL_FAILED`, `E_POOL_UNKNOWN_OWNER_FILL` | the note pool disagrees with what the book says |

The first three are caught before the offer is posted, which is before the first spend: in a script
that means a seller's startup can be rehearsed without money moving.

**The order a script repeats for every seller:**

```
directory -> wallet binding -> note -> market -> offer -> service
```

Each step depends only on the one before it and on its own state directory, so looping over a list
of sellers is the same order with the directory, the port and the advertised address substituted in.
Nothing in that loop may be shared between sellers except the model descriptions and the contracts
manifest, which are only ever read.
