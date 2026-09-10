# DEX.DO Glossary

## Terms

### Acki Nacki

The [blockchain network](https://ackinacki.com/) on which DEX.DO contracts and
transactions run.

### AmicableSplit

A clean terminal settlement shape that assigns earned value to the seller and
returns the refundable remainder to the buyer. It covers clean STOP and close
paths, including a buyer STOP after probe acceptance, and is distinct from a
[`BurnBoth`](#burnboth) outcome.

### AON / all or none

An order flag requiring the full requested inference volume to be matched as a
single unit with one counterparty. A partial fill is not accepted. Unlike
[`FOK`](#order-execution-flags), an AON order may wait on the book for a full
match.

### Bond

Collateral posted by a deal participant to make protocol violations costly.
The buyer and seller bonds are separate from payment [escrow](#escrow) and can
be returned or burned according to how the deal ends.

### BurnBoth

A settlement outcome that burns contested value from both participants. For a
buyer STOP before probe acceptance, it burns the buyer's probe price `P` and
`P` from the seller bond; the seller earns no service revenue. That terminal
path is recorded by [`ProbeBurned`](#probeburned).

### Buyer

The user that selects an offer, locks payment in escrow, and accesses the
seller's model through a local OpenAI-compatible endpoint.

### Clearing price

The price at which a match executes. In the inference order book, the clearing
price is always the seller's ask, even when the buyer's limit is higher.

### Contract generation / code-hash pin

A mutually compatible set of deployed contract builds. A code-hash pin is the
exact contract-code identity expected by the client; a matching version label
alone is not sufficient evidence that the code is compatible.

### DAPP ID

The identifier of a Decentralized Contract System on Acki Nacki. It is the
`account_id` of the root contract deployed by an external message. Contracts
that the root deploys by internal messages inherit the same DAPP ID. It is the
first half of a [DAPP-qualified address](#dapp-qualified-address).

### DAPP-qualified address

The canonical Acki Nacki address form:

```text
<dapp_id>::<account_id>
```

Both parts are 64 hexadecimal characters. Some message parameters accept the
account-only form `0:<account_id>`, but it omits the destination DAPP and is
insufficient when that identity is required.

### Deal

One matched buyer-seller relationship with its own payment, delivery, and
settlement state. A larger buyer order can create more than one deal when it
fills against multiple sellers.

### Deal handle

A local resumable record for one deal. It helps later commands find and
reconcile the deal, but it does not replace the deal state recorded on chain.

### Dispute

An on-chain challenge raised by a buyer against an open deal after observed
substitution or fraud. It freezes the contested amount and seller bond until
resolution. This is stronger than [`recover`](#recover--dexdo-recover), whose
normal STOP still pays the seller for delivered ticks. Opening a
dispute does not itself guarantee a refund.

### Dispute resolution

The terminal settlement of a disputed deal. It pays the seller for delivery
earned by the dispute time, allocates contested value and bonds according to
whether the seller concedes or the dispute window expires, and returns any
eligible remainder. Seller concession and timeout apply different penalties.

### ECC / ECC currency

A family of Acki Nacki currencies stored separately from an account's native
VMSHELL gas balance. Examples include NACKL (ECC index 1), SHELL (index 2), and
eccUSDC (index 3). Private inference deals are priced and settled in SHELL.

### eccUSDC

A US dollar-pegged stablecoin and Acki Nacki ECC asset identified as ECC
currency 3. It buys and sells SHELL at the fixed rate of 1 eccUSDC = 100 SHELL.

### Escrow

SHELL locked for a particular deal before service starts. Payment recognized
by the deal's delivery or subscription rules comes from escrow; the refundable
remainder is returned according to the terminal settlement path. Locked escrow
is still the buyer's exposure, but it is unavailable for another purchase.

### Fill

The executed part of an order. A partial fill leaves some requested quantity
unexecuted; a full fill executes the entire requested order quantity.

### Frame model

The exact canonical model-name string shared by both sides of a market. The
seller supplies it through `--model`, and the buyer supplies the same value
through `--frame-model`. Registry verification of that spelling is mandatory.
The string is byte-sensitive because it is the input to the
[`model hash`](#model-hash). To get the full list of names and check one of them, see
[Registered models](registered-models.md).

### Gas / fee

The execution cost of an on-chain operation. Gas is paid from native VMSHELL.
Some DEX.DO flows accept or reserve SHELL that is then converted or burned to
cover execution, which is why user guides may call SHELL "gas funding". This
does not make SHELL and native gas the same balance.

### Gateway

The seller's network service that carries the encrypted model stream. The
**listen address** is where the seller process binds locally; the **advertised
address** is the externally reachable address written into the handover and
dialed by buyers.

### Handover

The authenticated, encrypted connection information passed from seller to
buyer after a match. It tells the buyer how to reach the exact gateway for that
deal.

### Hot wallet

The working half of the Vault/Hot pair that
[Acki Nacki Wallet](https://ackinacki.com/wallet) creates for one named dexdo
agent. It has three public-key custodians: the user's `K0` and `K1` plus the
agent key. Any one can authorize an ordinary Hot transaction. Dexdo normally
uses the locally stored agent key to create, fund, and refill PrivateNotes.
Keep only that agent's working balance here. Gosh.ai and manual setup provide
or connect only Hot, not a Vault/Hot pair.

### Inference

Running an AI model on an input and returning its generated output. In DEX.DO,
a seller provides the model service. Ordinary deals pay for finalized delivery;
subscriptions apply separate take-or-pay rules to reserved capacity.

### Inference order book / model market

The on-chain book where sellers list capacity and buyers request ticks for one
exact [frame model](#frame-model). Its address is derived from that model's hash
and the pinned order-book code. It matches orders by price and then FIFO within
one price level. The dashboard is available at https://markets.dex.do.

### Inference tick

The protocol's billing and capacity unit: exactly **1,000,000 model tokens**.
Prices and order volumes are expressed per whole tick, while cumulative
delivery accounting counts the model tokens actually produced.

### Limit order

An order with a price boundary. If it cannot match immediately, permitted
unfilled quantity may rest until it is filled, cancelled, or reaches its
absolute [deadline](#time-in-force). A BUY starts at a minimum of
[two ticks](#minimum-stream-size); a remainder below two ticks is refunded
instead of resting.

### Mainnet

The live Acki Nacki network. Operations use live contracts and assets. The
released private-inference client selects its network through the installed
deployment manifest unless `DEXDO_MANIFEST` names another manifest.

### Maker / taker

A **maker** adds liquidity by leaving an order resting on the book. A **taker**
removes liquidity by executing against a resting order.

### Manifest

A file that names a network endpoint and pins the deployed contract generation
the client expects. The manifest selected by `DEXDO_MANIFEST`, or the installed
default when the variable is unset, determines which network the client uses.

### Market

See [Inference order book / model market](#inference-order-book--model-market).

### `market.json`

A local descriptor written by provisioning. It records the network, model,
and contract addresses needed by later seller commands. It is not the on-chain
market, and chain state remains authoritative.

### Market order

An order that consumes currently available book liquidity at the best prices.
It prioritizes execution over a specific price and may fill only partially
when the book is thin; unfilled quantity does not rest. Only the BUY side of
the inference order book supports market orders.

### Minimum stream size

A BUY and every fill require at least two inference ticks: one probe tick plus
one streaming tick. A resting BUY remainder smaller than two ticks is refunded.

### Model hash

The `sha256(frame_model)` digest used with the pinned order-book code to derive
the market address. Seller and buyer must hash the same exact frame-model
string. Different spelling or capitalization produces a different hash and a
different market.

### Model token

A unit counted by the model's tokenizer. It may represent a word, part of a
word, punctuation, or another text fragment. Model-token counts are used for
delivery accounting; they are not blockchain tokens.

### ModelRegistry

The on-chain authority that confirms canonical model names. It verifies the
exact [frame model](#frame-model); the model-name string and pinned order-book
code, not the registry itself, determine a market address. Dexdo requires this
registry verification before using a model name. Exporting and searching its
list is [Registered models](registered-models.md).

### Multisig wallet

A user-controlled on-chain wallet that requires configured custodian approval
to send funds. DEX.DO onboarding commonly uses a multisig to fund PrivateNotes
and receive funds withdrawn from them.

### NACKL

The Acki Nacki token used for network security, including staking, slashing,
and block rewards. It is ECC currency 1. Private inference deals are priced and
settled in SHELL, not NACKL.

### Note key

The private key that controls a PrivateNote and signs its on-chain operations.
Possession of this key means control of the note's funds.

### Note pool

A local file containing one or more PrivateNote records and their access data.
Pool files contain private keys and must be stored as secrets, never committed
or copied into logs.

### Offer / ask

A seller order advertising available inference capacity, its price per tick,
and the terms under which it can match. An ordinary SELL rests only until its
first successful fill; that fill consumes the whole SELL from the book even if
the buyer uses only part of its capacity. The seller CLI posts any remaining
capacity as a new offer. A subscription SELL is AON and cannot fill partially.

### Order book / depth

See [Inference order book / model market](#inference-order-book--model-market).
**Depth** groups available quantity at each price level. A **bid** is a buy
price, an **ask** is a sell price, and the **spread** is the gap between the
best bid and best ask. A match executes at the
[`clearing price`](#clearing-price), which is always the seller ask.

### Order execution flags

Order flags control matching behavior rather than lifetime:

- plain limit (`0x00`) matches what is available and may leave a remainder;
- `IOC` matches what is available now and refunds the remainder;
- `FOK` matches the entire quantity atomically now or does nothing;
- `MARKET` is a BUY without a price cap and leaves no remainder;
- `POST_ONLY` posts maker liquidity or rejects an immediately executable order;
- `TEE` declares a TEE SELL or requires one on a BUY;
- `AON` requires one counterparty for the entire volume; and
- `SUBSCRIPTION` selects the four-week take-or-pay settlement.

These are the supported bits. GTC is not an order flag.

### Platform fee

An additional charge paid by the buyer when inference payment becomes
irreversible. The current rate is 250 basis points, or 2.5%. It is calculated
separately from the seller's price per tick.

### Price per tick

The SHELL price for one [inference tick](#inference-tick). Seller asks and
buyer price ceilings use whole SHELL per tick in the current order book.

### PrivateNote / PN

A reusable on-chain account used to hold trading funds and sign DEX.DO
operations without exposing the user's ordinary funding wallet in every trade.
It is an account, not a one-use voucher: trading changes its balances but does
not consume the note itself. See
[PrivateNote balance planes](#privatenote-balance-planes).

### PrivateNote balance planes

A PrivateNote exposes three separate balance planes:

- the `getDetails` **trading record** is SHELL used for orders, escrow, seller
  earnings, note transfer, and withdrawal;
- the account's **ECC[2] pocket** is physical SHELL used to fund deal gas and
  is the balance guides call the note's **gas pocket**; and
- the account's native **vmshell balance** pays for the note's own messages and
  is not SHELL.

`dexdo note withdraw` drains the trading record and ECC[2] pocket separately;
it does not withdraw native vmshell. A balance in one plane says nothing about
the other two.

### PrivateNote deployment recovery

The crash-safe process for resuming an interrupted PrivateNote deployment from
its recovery file and folding the recovered note into its pool. The dedicated
command is `dexdo note recover`. This process is distinct from deal recovery
with [`dexdo recover`](#recover--dexdo-recover).

### Probe

The first trial tick in a deal. After the 180-second probe window, the seller
may call `acceptProbe()`. A buyer that receives no response or output during
the window stops instead; otherwise buyer silence permits seller acceptance.
Suspected substitution or fraud belongs to the [dispute](#dispute) path.
Acceptance atomically credits and pays exactly one tick, starts delivery
accounting, and starts the subscription term when applicable. Before
acceptance, a buyer STOP burns the probe on both sides. Exact-model checks
remain client-side verification, not a cryptographic guarantee by the on-chain
contract.

### ProbeBurned

The terminal event emitted when a deal stops before the probe is accepted. It
records the buyer probe and matching seller-bond amount burned under the
[`BurnBoth`](#burnboth) outcome; seller service revenue is zero.

### Quote

A read-only estimate of the price and capacity currently executable against
the inference order book. It does not place an order or reserve funds.

### Raw unit

The smallest integer unit used by contracts. Human-facing commands and API
responses normally scale raw values into asset amounts. Do not paste a raw
amount into a human-unit option, or a human amount into a raw-unit field,
without checking that command or field's documentation.

### Rebate

Part of the accrued platform fee returned to a seller after a clean,
never-disputed deal. Its rate increases by 4 basis points per finalized tick,
up to 200 basis points. The rebate cannot exceed the accrued fee; any fee left
after the rebate burns. A disputed deal receives no rebate.

### Reclaim

A buyer action that recovers escrow after a matched deal was funded but the
seller did not open it before the protocol timeout. It differs from
[`recover`](#recover--dexdo-recover), which stops an already open deal.

### Recover / `dexdo recover`

A buyer-signed STOP for an orphaned open deal whose buyer process died while
the note and key remain available. It closes the existing deal without placing
a new BUY. After an accepted probe, it uses the normal
[`AmicableSplit`](#amicablesplit): delivered ticks are still paid.
It is unrelated to
[PrivateNote deployment recovery](#privatenote-deployment-recovery).

### Seller

The operator that lists model capacity, runs the gateway, and serves inference
output. The deal protocol calculates seller payment from finalized delivery
for an ordinary deal and from scheduled take-or-pay rules for a subscription.

### Settlement

The on-chain allocation of payment, refunds, bonds, fees, and penalties when a
deal closes or reaches a scheduled subscription boundary. Ordinary delivery
and subscription take-or-pay use different rules. See the terminal outcomes
[`AmicableSplit`](#amicablesplit), [`BurnBoth`](#burnboth),
[`ProbeBurned`](#probeburned), [stream termination](#stream-termination), and
[dispute resolution](#dispute-resolution).

### SHELL

An Acki Nacki utility token and ECC asset identified as ECC currency 2. SHELL
is minted by depositing eccUSDC and can be converted to VMSHELL at a 1:1 ratio
to cover network fees. Conversion is one-way; VMSHELL cannot be converted back
to SHELL. SHELL can cross DAPP IDs. Private inference uses it for funding,
payment, escrow, and seller revenue. SHELL itself is not native gas. You can buy SHELL for inference payments on [gosh.ai](https://gosh.ai).

### Stream termination

The clean termination and settlement of an open deal after probe acceptance.
It pays the seller for service recognized by the applicable delivery or
subscription rules, refunds the buyer's eligible remainder, and follows the
[`AmicableSplit`](#amicablesplit) path.

### Subscription

An all-or-none inference purchase with capacity scheduled over a fixed
four-week term. Its tick volume must be a multiple of four and cannot exceed
40,320 ticks. It reserves a buyer bond of `2P`, where `P` is the clearing price
per tick. Its clock starts when the seller accepts the probe, not when the
order matches, and it does not renew automatically.

### Tick

See [Inference tick](#inference-tick).

### Time in force

Order lifetime is controlled by an absolute deadline, not by an execution
flag. A SELL uses a `ttl` from 1 through 3,600 seconds, converted on chain to an
absolute deadline. A BUY also carries an absolute deadline; dexdo gives every
BUY a finite future deadline, currently one hour. The contract can represent
GTC as a zero BUY deadline, but dexdo does not submit that mode. Execution
behavior such as IOC, FOK, and POST_ONLY is defined separately by
[order flags](#order-execution-flags).

### TokenContract / per-deal contract

The on-chain contract that holds one deal's escrow and bonds, records delivery,
and applies terminal settlement rules. Despite its name, it is not a model
token.

### Usage claim

A seller's cumulative on-chain statement of model tokens delivered in one
deal. It advances payment accounting for inference.

### Vault wallet

The user's reserve half of the [Acki Nacki Wallet](https://ackinacki.com/wallet) pair for one named dexdo
agent. It also has three public-key custodians: the user's `K0` and `K1` plus
the agent key. By default, the agent custodian uses the same public key as Hot;
`--vault-key` creates and stores a distinct local Vault key. An ordinary
outgoing transfer requires two signatures: the agent submits the request, and
a human confirms with either `K0` or `K1`, so the agent cannot withdraw funds
alone. Changing keys, custodians, or settings on either Vault or Hot also
requires two approvals.

### VMSHELL / native gas

The native unit used to pay for contract execution and messages. It lives in
an account's native balance, separately from ECC assets. A funding operation
may convert SHELL into VMSHELL; value converted to native gas is no longer
spendable as SHELL currency.

### Wallet and PrivateNote

A [multisig wallet](#multisig-wallet) holds user-controlled funds and commonly
funds or receives withdrawals from a [PrivateNote](#privatenote--pn). A
PrivateNote is a reusable on-chain trading account. How money moves between
them is [Wallets and funds in dexdo](wallets-and-funds.md).
