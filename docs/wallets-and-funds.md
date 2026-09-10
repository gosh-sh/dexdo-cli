# Wallets and funds in dexdo

Dexdo uses several accounts because keeping long-term funds, signing routine
operations, paying transaction gas, and trading privately are different jobs.
The shortest useful model is:

```text
Acki Nacki Wallet:  funding source -> Vault -> Hot -> PrivateNote -> trade
Gosh.ai or manual:  funding source --------> Hot -> PrivateNote -> trade
```

The arrows move or lock value; they do not all mean "money was spent." This
page explains what each account does, who authorizes every move, and which
amounts can come back.

## The five names

- **Funding source** is the account where you already hold funds. It is outside
  the dexdo contracts; with a manual setup, it can be any wallet you control.
- **Vault** is the user's reserve wallet in a pair that Acki Nacki Wallet creates
  for one specifically named dexdo agent. It has exactly three public-key
  custodians: two user keys (`K0` and `K1`) and the agent key. By default, the Hot
  public key is also used for Vault; `--vault-key` creates a separate local Vault
  key. An ordinary transfer requires two signatures: the agent submits and signs
  it first, and a human confirms with either user key, so the agent cannot
  withdraw funds alone. Changing the keys, custodians, or settings on either
  Vault or Hot also requires two approvals.
- **Hot** is the working wallet for that specific agent in the same pair. It also
  has exactly three public-key custodians: `K0`, `K1`, and the agent key; dexdo
  generates and stores the Hot secret locally. An ordinary transaction requires
  one signature from any of the three custodians. In normal operation dexdo uses
  the local agent key, so it can pay from Hot to create and fund a PrivateNote
  without another approval. Keep only that agent's working balance here. Gosh.ai
  and manual setup provide or connect only Hot, not a Vault/Hot pair.
- **PrivateNote** is the private on-chain account used for trading. It owns its
  own key and has separate trading and gas balances. It is reusable; one trade
  does not consume or destroy it.
- **Trade** is the order/deal layer. A buyer temporarily moves deposit and bond
  out of the PrivateNote's trading record; settlement pays for finalized ticks
  and returns or disposes of the rest according to the terminal outcome.

## What happens at every arrow

| Arrow | What happens and who authorizes it | How much must be available | What can return, and what is actually spent |
| --- | --- | --- | --- |
| Funding source -> Vault | Acki Nacki Wallet moves funds into the user's cold Vault. The user authorizes this in the wallet; dexdo does not sign it. | Enough for the Hot working balance and future wallet fees. Dexdo does not prescribe a long-term Vault balance. | The transferred balance remains under the user's Vault policy. Outgoing message fees are spent. SHELL converted to native vmshell is gas and does not automatically convert back into trading SHELL. |
| Vault -> Hot | Acki Nacki Wallet provider only. Dexdo's agent signs and queues the exact shortfall in the two-confirmation Vault; the human supplies the second confirmation in Acki Nacki Wallet. Nothing leaves the Vault while the request is merely pending. | The Hot is brought up to the command's target: for N100 note deployment, `350 SHELL` as ECC[2], plus a native balance of at least `0.507002 vmshell` (`FUNDING_WALLET_NATIVE_FLOOR_RAW`). Existing Hot balances reduce the shortfall. | After confirmation the value belongs to the user's Hot, so the transfer itself is not a dexdo purchase. The Vault pays its message fee. An expired or unconfirmed request transfers nothing. |
| Funding source -> Hot | Gosh.ai and manual providers have no Vault queue. The user tops the Hot up directly; dexdo waits for the on-chain balance. For a fresh Hot, `wallet onboard manual` asks for a user-authorized, non-bounceable flag-16 transfer of `2 SHELL` (`MANUAL_DEPLOY_REQUEST_RAW`), followed by wallet deployment. | The 2 SHELL request is independent of note nominal. Deployment can start once native gas reaches the `1 SHELL` threshold (`OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE`). Before an N100 deployment, the active Hot needs `350 SHELL` ECC[2] and at least `0.507002 vmshell` native (`FUNDING_WALLET_NATIVE_FLOOR_RAW`). | Flag 16 converts the transferred SHELL into native vmshell, so it is irreversibly no longer trading currency. It is not all burned immediately: the measured wallet deployment cost, also used as `WALLET_SUBMIT_NATIVE_FEE_BOUND_RAW`, is `0.153501 vmshell`; the remaining native stays on Hot for later messages. Direct ECC[2] top-ups remain currency until spent or moved. |
| Hot -> PrivateNote | `dexdo note deploy` signs automatically with the bound Hot key. Acki Nacki Wallet's Hot is one-confirmation, so no second human approval is needed after the command is started. Gosh.ai/manual use the local key bound to their Hot. The command creates a new note and stores its owner secret in the private pool file. | N100 requires exactly `350 SHELL` ECC[2]: `100 SHELL` nominal plus the contract's `250 SHELL` gas deposit (`ROOT_PN_GAS_DEPOSIT_RAW`). The Hot must also pass the `0.507002 vmshell` native preflight floor (`FUNDING_WALLET_NATIVE_FLOOR_RAW`). | The 350 SHELL is allocated to the new note, not charged as a 350 SHELL fee: it becomes `100 SHELL` in the trading record and `250 SHELL` in the physical ECC[2] gas pocket. Those balances can later leave through `note transfer` or `note withdraw`, subject to contract gates. The current deploy's wallet submit attaches `0.1 vmshell` (`NOTE_DEPLOY_SUBMIT_NATIVE_VALUE`); that call value and the transaction's actual fee leave Hot. `0.507002` is a conservative balance bound, not the observed fee, and unused native stays on Hot. |
| PrivateNote -> trade | The buyer or seller command signs with the note owner key. The human authorizes the action by starting the command and accepting its displayed terms. A buyer moves fee-inclusive deposit plus a buyer bond from the note's trading record into the order/deal contracts. | Depends on price and tick count. The worked example below needs `14.25 SHELL` temporarily debited/locked. The note also needs enough physical ECC[2]/native gas to send and fund contract messages. | Finalized service and applicable fees are spent. Under normal settlement, undistributed deposit and the refundable bond return to the note. Refusal, dispute, timeout, probe-burn, or other terminal paths may burn or distribute some amounts differently; read the settlement receipt. Message gas is spent in every path that sends messages. |

The `0.507002 vmshell` figure (`FUNDING_WALLET_NATIVE_FLOOR_RAW`) is a **Hot
preflight balance floor**. The shared funding guard derives it as two bounded
submit budgets of `0.1 vmshell` attached value
(`NOTE_DEPLOY_SUBMIT_NATIVE_VALUE`) plus at most `0.153501 vmshell` fee each
(`WALLET_SUBMIT_NATIVE_FEE_BOUND_RAW`). The current deploy path itself has one
deposit-voucher wallet submit; the larger floor remains conservative.
It does not mean that `note deploy` immediately burns `0.507002 vmshell`. Chain
receipts show the actual debit; any unused native balance remains on Hot.

## Wallet providers: which accounts you get

### Acki Nacki Wallet: Vault and Hot

```bash
dexdo wallet onboard ackinacki-wallet --agent-name "My dexdo agent"
```

The wallet supplies a pair:

- Vault requires two transaction confirmations. The agent creates a precise
  Vault-to-Hot request with the first signature; the user reviews and confirms
  it in Acki Nacki Wallet.
- Hot requires one transaction confirmation. Once funded, dexdo can sign its
  operational transactions without asking the Vault to approve each one.

The command decides that funding is complete only after it reads the required
balance on Hot. A queued request, a wallet screen, or a submit response alone is
not proof that the money arrived.

### Gosh.ai and manual: Hot only

Gosh.ai and manual bindings have no Vault and no server-side funding request.
When Hot is short, dexdo prints the exact shortfall and waits while the user
tops it up.

A Hot must have `requiredTxnConfirms = 1`. Manual onboarding rejects a wallet
with a higher threshold because that wallet is a Vault; bind the one-confirmation
Hot that it funds instead.

```bash
dexdo wallet onboard gosh-ai
```

For an already controlled Hot and local secret file:

```bash
dexdo wallet onboard manual \
  --multisig-address <DAPP-ID>::<ACCOUNT-ID> \
  --multisig-private-key /path/to/hot.key
```

To derive and fund a fresh manual Hot from a local key, omit the address:

```bash
dexdo wallet onboard manual \
  --multisig-private-key /path/to/hot.key
```

`dexdo wallet show` reports the active binding and the Hot's native and ECC
balances. The explicit wallet arguments on `note deploy` and `note topup` are
overrides; the normal path uses the active binding.

## Choosing a PrivateNote nominal

The contract's `ALLOWED_NOMINALS` are `N100`, `N1000`, `N10000`, `N100000`, and
`N1000000`. The nominal is the initial trading balance, not the total funding
required from Hot. Every new note also receives the fixed 250 SHELL physical
ECC[2] gas pocket (`ROOT_PN_GAS_DEPOSIT_RAW`):

| Nominal | Initial trading record | Initial physical ECC[2] pocket | Required Hot ECC[2] |
| --- | ---: | ---: | ---: |
| N100 | 100 SHELL | 250 SHELL | 350 SHELL |
| N1000 | 1,000 SHELL | 250 SHELL | 1,250 SHELL |
| N10000 | 10,000 SHELL | 250 SHELL | 10,250 SHELL |

The same `nominal + 250 SHELL` rule applies to the two larger accepted
nominals. Start with **N100** unless you already know that you need more trading
capacity. It limits the amount placed in a new private account while preserving
the same gas pocket and all normal functionality.

With an active wallet binding, the minimal command is:

```bash
dexdo note deploy --nominal N100
```

The pool file written by this command contains the note owner secret. Keep it
private and never commit or paste it.

## A note has three visible balance planes

Run the read-only command:

```bash
dexdo note balance --note-addr <DAPP-ID>::<ACCOUNT-ID>
```

Its important lines mean:

1. **Spendable token balance / trading record** (`getDetails().balance`) is the
   SHELL used to buy inference, post trading value, and receive earnings or
   refunds.
2. **SHELL gas ECC[2] / account ECC balance** is the physical 250 SHELL pocket
   created with the note. Contracts draw from it for deployments and deal gas.
3. **VMSHELL native gas** pays the note account's own messages. It is not SHELL
   trading balance.

Value locked in an inference order or deal has already left the spendable
trading record, even though settlement has not spent all of it. Use
`dexdo note outstanding` and `dexdo status <DEAL>` to inspect those obligations.

## Refill, consolidate, withdraw, and sweep

These commands move different balances. Their `--to` arguments are target
levels or destinations, not interchangeable amounts.

### Refill the physical gas pocket

```bash
dexdo note topup \
  --note-addr <DAPP-ID>::<ACCOUNT-ID> \
  --to 250
```

`note topup` sends only the shortfall needed to bring the note's physical
ECC[2] pocket up to the target. It spends from the active Hot and is safe to
repeat unchanged after an uncertain result. It does **not** increase the
note's trading record.

### Move trading balance between notes

```bash
dexdo note transfer \
  --note-addr <FROM-DAPP>::<FROM-ACCOUNT> \
  --note-key /path/to/from-note.key \
  --to-note-addr <TO-DAPP>::<TO-ACCOUNT> \
  --to 100
```

`note transfer` brings the destination's trading record up to the target by
moving the shortfall from the sender. It does not move the physical gas pocket
and does not involve Hot. Re-running the same target is idempotent once two
reads agree it was reached.

The nonzero shortfall must be at least `0.01 SHELL`
(`MIN_NOTE_TRANSFER_SHELL_RAW`). The client checks this before submitting because
the contract would reject a smaller transfer only after accepting the message,
which would still spend the sender note's gas.

⚠️ If no other suitable PN is available, you need to create a new PN using dexdo note deploy: you cannot add additional trading funds directly from a multisig wallet to an existing PN.

❗️ A transfer may also be blocked by the PN state, for example if there are active locks or unfinished operations.

### Withdraw a finished note

```bash
dexdo note withdraw \
  --note-addr <DAPP-ID>::<ACCOUNT-ID> \
  --to <HOT-DAPP-ID>::<HOT-ACCOUNT-ID>
```

`note withdraw` is owner-signed. A successful withdrawal is one-shot and
irreversible: when all contract gates are clear, it drains both the trading
record and the physical ECC[2] pocket to the canonical destination wallet. It
does not withdraw the note's native vmshell. Outstanding orders, live deals,
pending operations, stakes, or debt must be cleared first; `note balance`
reports the withdrawal gate it can read. If `RootPN` lacks the required
liquidity, `PrivateNote.revertWithdraw` restores the trading balance and
physical ECC[2] and clears the withdrawal latch, so the failed withdrawal does
not permanently disable the note.

This CLI withdraws only a PrivateNote whose on-chain `code_hash` matches the
current contract generation. It refuses a previous-generation or unknown note
before any on-chain write because withdrawing it with the current CLI could
zero the note without crediting the destination.

### Collect a late refund after withdrawal

```bash
dexdo note sweep \
  --note-addr <DAPP-ID>::<ACCOUNT-ID> \
  --to <HOT-DAPP-ID>::<HOT-ACCOUNT-ID>
```

An order refund or returned bond can arrive as physical ECC[2] after the
one-shot withdrawal. `note sweep` moves that late physical SHELL to the chosen
wallet. It only works after withdrawal, does not move a trading record, and
does not turn the received SHELL into native gas. Re-read `note balance` before
retrying an unverified sweep because the first transfer may still land.

## Tick economics: a complete worked example

One tick is exactly **1,000,000 model tokens** (`TICK_SIZE`). It is a billing
quantum, not one stream chunk, one request, or one generated token.

For a price of **2 SHELL per tick** and **5 ticks**, with the buyer's ceiling
equal to that price:

| Component | Calculation | Amount |
| --- | ---: | ---: |
| Service price | 2 x 5 | 10 SHELL |
| 2.5% fee-inclusive deposit (`PLATFORM_FEE_BPS = 250`) | 10 x 1.025 | 10.25 SHELL |
| Refundable buyer bond (`SUBSCRIPTION_BUYER_BOND_TICKS = 2`) | 2 x price | 4 SHELL |
| Total temporarily debited/locked | 10.25 + 4 | **14.25 SHELL** |

The 10.25 SHELL deposit and 4 SHELL bond are separate components on the current
contract generation. A displayed **2.05 SHELL** can therefore be correct when
it names the fee-inclusive deposit for **two ticks at 1 SHELL per tick**:
`2 x 1 x 1.025 = 2.05`. It is not, by itself, the total current-generation
debit; the separate `2P` buyer bond is another 2 SHELL in that example.

The final spend is determined by chain facts, not by the initial lock:

- service value is paid for delivered and finalized ticks;
- applicable platform fees and message gas are spent;
- under normal settlement, undistributed escrow and the buyer bond return to
  the note;
- probe rejection/burn, dispute, timeout, fraud, or another terminal outcome
  can burn or distribute specific amounts differently.

Use the deal status and settlement receipt to distinguish **paid**, **still
locked**, **returned**, and **burned**. Do not infer the final cost from the
largest balance that was temporarily debited.

## When do you really need a new note?

Do not deploy a new note merely because one trade finished or a balance is
below its starting value. A PrivateNote is an account and can support repeated
trades:

- refill its physical gas pocket with `note topup`;
- consolidate trading balance from another live note with `note transfer`;
- cancel or settle outstanding work before reusing it.

A new note is needed when no suitable current-generation note exists, the old
note has been withdrawn, or its trading record cannot be replenished from
another note you control. This CLI neither reuses nor withdraws an unsupported
old-generation note: the `code_hash` safety check refuses withdrawal before any
on-chain write because that operation could otherwise destroy the balance
without crediting the destination.

