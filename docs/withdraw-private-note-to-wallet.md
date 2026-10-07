# Withdraw PrivateNote funds to a wallet

The command `dexdo note withdraw` moves the PrivateNote's complete spendable token map and its physical SHELL
`ECC[2]` pocket to the wallet specified by the required `--to` option. It does not move the PN's
native `vmshell` gas. A successful operation is irreversible and `withdrawTokens` can be executed only once for that PN.

Withdrawal links the PrivateNote to the destination wallet and therefore deanonymizes it. It also
permanently prevents that note from posting another sell offer. To continue selling after a
withdrawal, deploy and use a new PrivateNote.

> **Points & Rewards Program participants**
>
> **A mandatory requirement for receiving a reward** under the
> [Points & Rewards Program](https://seasons.dex.do/) is to withdraw funds to a wallet registered
> in the Program and linked to your account at [app.dex.do](https://app.dex.do/). Upon withdrawal,
> the Participant's activity across all PrivateNotes is linked to their account.

This guide works with any Acki Nacki wallet on the selected network. The bound Hot wallet is the
easiest example because `dexdo wallet show` can provide authoritative before/after SHELL readings.
For the broader wallet and PrivateNote fund flow, see
[Wallets, PrivateNotes, and funds](wallets-and-funds.md).

Choose one procedure and follow it from beginning to end:

- **Human operator:** follow the [Human procedure](#human-procedure).
- **Agent or automation:** follow the
  [Agent and automation procedure](#agent-and-automation-procedure).

The procedures deliberately repeat commands and safety checks so that each one is complete. Do not
combine their steps or jump from one procedure to the other.

All shell snippets use Bash/Zsh syntax. Translate variable assignments before using them in Fish.

## Human procedure

### H1. Fix the instance and network

Record the client version and use an absolute instance directory:

```bash
dexdo --version
DATA_DIR='/absolute/path/to/.dexdo-quickstart'
```

The deployment manifest, not `DATA_DIR`, selects the network. Without `DEXDO_MANIFEST`, `dexdo`
uses the installed per-user manifest. To select one explicitly, set it once to an absolute path and
do not change it during this procedure:

```bash
export DEXDO_MANIFEST='/absolute/path/to/manifest.json'
```

Whether you use the installed per-user manifest or select one explicitly, always run:

```bash
dexdo --data-dir "$DATA_DIR" doctor --json
```

Proceed only if the command succeeds and returns one JSON document with:

- `schema == "dexdo.doctor.v1"`;
- the intended `network`;
- `verdict == "pass"`.

Keep the same binary, `DATA_DIR`, and manifest path and contents for every read and write below. A
failed doctor, unexpected schema, missing field, or network mismatch is a stop condition.

### H2. Choose the exact PN and destination

List the PNs recorded in the instance:

```bash
dexdo --data-dir "$DATA_DIR" note list
```

This list is discovery only. A PN whose owner key is stored separately may not appear in this
instance's local pool; its absence does not block the explicit `--note-key` path. Obtain the complete
canonical PN address from the list or another authoritative owner record, then set it explicitly:

```bash
PN='paste-the-full-note-address-here'
```

Do not copy a shortened value from a menu or log.

The destination must be on the same network and must use canonical
`<dapp_id>::<account_id>` form. For the Hot wallet bound to this instance, run:

```bash
dexdo --data-dir "$DATA_DIR" wallet show
```

For any other destination, obtain its complete address from an authoritative wallet or chain view.
On mainnet, the [Acki Nacki Explorer](https://mainnet.ackinacki.org/) is a useful human-facing
example. Set the address explicitly:

```bash
DEST='paste-the-full-destination-address-here'
```

Verify every character of `$PN` and `$DEST`. `note withdraw` does not infer or default either
address. If you participate in the Points & Rewards Program, confirm before continuing that
`$DEST` is registered in the Program and linked to your account at
[app.dex.do](https://app.dex.do/).

### H3. Perform the read-only preflight

Read the PN:

```bash
dexdo --data-dir "$DATA_DIR" note balance --note-addr "$PN"
```

Record:

- `PN_trading_before`: the raw number on the `ECC[2] SHELL` line under
  `PrivateNote.getDetails spendable token balance (trading money)`;
- `PN_ECC2_before`: the raw number on the separate `SHELL gas ECC[2]` line;
- the complete spendable token map, including any non-SHELL currencies;
- native `vmshell`, busy state, and the `withdrawTokens` gate result.

Do not take either recorded value from the `account ECC balances (deployment gas)` section. Its
`ECC[2] SHELL` entry repeats the physical account pocket, but `PN_ECC2_before` is deliberately tied
to the unambiguous `SHELL gas ECC[2]` line.

Proceed only when the busy latch was read and is free. A successful fail-closed `withdrawTokens`
STATE-gate read is printed exactly as:

```text
  all 11 withdrawTokens STATE gates read and closed -- the eleven are note STATE only; gas and amounts are settled when the message is sent and are NOT among them, so this is NOT a statement that a withdrawal will succeed
```

Here `closed` means that none of the decoded STATE gates holds the PN; it does not mean that
withdrawal is blocked. Gas and transfer amounts are checked later, when the message is sent. A
blocking gate is printed in this form:

```text
  held by <gate> (<code>): <what it holds> -- <next step> [PrivateNote.sol:<line>, exit_code=<code>]
```

If a blocker is reported, `note outstanding` can provide human diagnostics:

```bash
dexdo --data-dir "$DATA_DIR" note outstanding --note-addr "$PN"
```

It may find recoverable resting orders or independently validated deal leads, but its output can be
incomplete. Resolve the reported condition and rerun both commands. An empty `note outstanding`
result never authorizes a withdrawal and never overrides a held or unreadable `note balance` gate.

Record the destination's authoritative raw SHELL balance as `destination_before`. For the bound
Hot wallet:

```bash
dexdo --data-dir "$DATA_DIR" wallet show
```

For another wallet, use the same authoritative wallet or chain view before and after the write.
Allow no other incoming or outgoing destination transfer until verification is complete.

For the current SHELL workflow, calculate with exact integer arithmetic:

```text
expected_SHELL_raw = PN_trading_before + PN_ECC2_before
```

This formula verifies only currency `2` (SHELL). `withdrawTokens` transfers the entire spendable
token map. If that map contains another currency, proceed only when an authoritative per-currency
source can provide before/after readings and an exact expected-delta method for every such currency.
Otherwise stop before the irreversible write.

The current CLI has a mandatory pre-submit contract-generation guard. When H4 runs, if it reports
`PREVIOUS contract generation`, no withdrawal was submitted. Stop; do not use an older CLI or call
the contract manually. Preserve the PN address, owner key, client version, manifest identity, and
the output of:

```bash
dexdo --data-dir "$DATA_DIR" note balance --note-addr "$PN" --json
```

Report the case in the [DEX.DO Tech Talks - Chat](https://t.me/c/3819653418/1) for a generation-specific recovery
procedure, but never send the owner key or its contents to the chat. A current-generation PN created
for new activity does not recover the old balance.

### H4. Submit exactly once

The CLI repeats the deanonymization and permanent selling consequences before submission. Type
`YES` only after checking the complete note and destination above. Any other answer cancels before
the transaction and leaves local state unchanged.

When the PN owner key is in the current pool:

```bash
dexdo --data-dir "$DATA_DIR" note withdraw \
  --note-addr "$PN" \
  --to "$DEST"
```

When the owner key is stored separately:

`--note-key` takes a file path, not the secret itself. After surrounding whitespace is trimmed, the
file must contain exactly 64 hexadecimal characters: the 32-byte SDK secret of this PN's on-chain
owner key.

If the only authoritative source is another PN pool, select the one `notes[]` entry whose complete
`address` equals `$PN` and extract its plaintext `owner_secret_key_hex`. Write only that field
directly into a new file in an owner-only directory, with mode `0600` set before the secret is
written. Do not print the field, copy it through the terminal, or first write it to a less private
file. Verify the exact PN match, the 64-hex format, and mode `0600` before continuing. Then set:

```bash
NOTE_KEY='/secure/path/note-owner.key'
```

```bash
dexdo --data-dir "$DATA_DIR" note withdraw \
  --note-addr "$PN" \
  --note-key "$NOTE_KEY" \
  --to "$DEST"
```

Keep the owner key private. Do not print its contents, paste them on the command line, include them
in logs or messages, or commit them. The command always withdraws the whole spendable token map and
physical SHELL `ECC[2]`; it does not accept a partial amount and does not withdraw native
`vmshell`.

Preserve the command's complete stdout and stderr and its start and finish times. The current command
has no JSON result surface and may provide no message or transaction identity. Preserve such an
identity if a future or platform-specific output includes one, but its absence is expected and is
never permission to retry. The line `withdrawTokens submitted` proves only that the transaction was
submitted; delivery is established only by the read-back in H5.

### H5. Verify the result by fact

Choose a finite deadline for verification. Repeat only the read-only PN and destination checks until
every predicate below passes or that deadline expires. Do not submit another write while polling.

```bash
dexdo --data-dir "$DATA_DIR" note balance --note-addr "$PN"
```

For the bound Hot wallet, also run:

```bash
dexdo --data-dir "$DATA_DIR" wallet show
```

For a non-Hot destination, read it through the same authoritative source used for the baseline.

In the human output, an entirely empty successfully read balance section is printed as
`none reported`; that is the `absent` case below. A missing currency-`2` line in an otherwise
nonempty successfully read section also means that currency `2` is absent. An `unknown` or
unreadable section is never absence.

A confirmed current-SHELL result requires all of these post-submit facts:

- every spendable currency is absent or has raw value `0`, and in particular currency `2` is
  absent or zero;
- physical `ECC[2]` is absent or has raw value `0`;
- the exact held `withdrawTokens` gate is `_hasWithdrawn`;
- `destination_after - destination_before == expected_SHELL_raw` exactly;
- every non-SHELL spendable currency recorded before submission has its expected authoritative
  destination delta.

A fully reverted withdrawal is a separate terminal result only when authoritative chain evidence or
the polling history proves that this submission reached the PN and `revertWithdraw` returned it. The
complete spendable token map and physical `ECC[2]` must match their pre-submit values exactly, the
gate must be readable and clear again, and every destination delta must be zero. An unchanged
snapshot alone may mean the message is still pending and remains `UNRESOLVED`. Otherwise classify
the result as `REVERTED`, resolve the RootPN liquidity shortfall, and repeat H3 before a new manual
attempt. Never retry automatically.

A failed command, unreadable value, mismatched address or network, arithmetic error, unexpected
delta that is neither the confirmed nor fully reverted result above, or expired verification
deadline makes the outcome `UNRESOLVED`.

`UNRESOLVED` does not mean failed. Preserve the exact command, complete stdout and stderr, command
start and finish times, PN, destination, raw before/after readings, manifest identity, chosen
verification deadline, and any message or transaction identity that was actually emitted. Continue
only read-only reconciliation. Never repeat `note withdraw` automatically.

Any native `vmshell` remaining on the PN is expected and is outside the withdrawal.
Withdrawn SHELL arrives at the destination as ECC[2], the traded asset, not as spendable native gas.
A destination with zero native `vmshell` remains gas-starved after the withdrawal.

### H6. Collect SHELL that arrives after withdrawal

An old order cancellation or deal refund can send physical SHELL `ECC[2]` to the PN after the
one-shot withdrawal. `note sweep` moves only this late physical pocket; it does not move the
trading balance or native `vmshell` and is not a second withdrawal.

Before a sweep, re-read the PN:

```bash
dexdo --data-dir "$DATA_DIR" note balance --note-addr "$PN"
```

Proceed only when `_hasWithdrawn` is the exact held gate and a successfully read `ECC[2]` value is
greater than zero. For the bound Hot destination, record a new authoritative baseline with:

```bash
dexdo --data-dir "$DATA_DIR" wallet show
```

For a non-Hot destination, record the baseline through the same authoritative wallet or chain view
used in H3. Use a complete PN and destination supplied for this operation, and set the expected raw
SHELL delta to the pre-sweep `ECC[2]` value. Verify the network, addresses, amount, and command before
submitting.

```bash
dexdo --data-dir "$DATA_DIR" note sweep \
  --note-addr "$PN" \
  --to "$DEST"
```

With a separately stored owner key:

```bash
dexdo --data-dir "$DATA_DIR" note sweep \
  --note-addr "$PN" \
  --note-key /secure/path/note-owner.key \
  --to "$DEST"
```

Interpret the CLI verdict by its complete prefix:

- `SWEEP CONFIRMED` means the PN physical pocket was observed to decrease;
- `SWEEP UNVERIFIED` means it did not decrease within the confirmation window and the transfer may
  still arrive.

Swept SHELL also arrives as ECC[2], not as spendable native gas, and does not unblock a gas-starved
destination wallet.

Never classify either result by matching the substring `CONFIRMED`. Neither prefix alone proves
that the destination was credited. Re-read both sides and require the exact authoritative
destination delta. On any ambiguity, mark the sweep `UNRESOLVED` and never automatically retry it.

## Agent and automation procedure

### A1. Pin the instance, binary, manifest, and network

Record the client version and use an absolute instance directory:

```bash
dexdo --version
DATA_DIR='/absolute/path/to/.dexdo-quickstart'
```

The deployment manifest, not `DATA_DIR`, selects the network. Without `DEXDO_MANIFEST`, `dexdo`
uses the installed per-user manifest. To select one explicitly, set it once to an absolute path:

```bash
export DEXDO_MANIFEST='/absolute/path/to/manifest.json'
```

Whether the installed per-user manifest is used or one is selected explicitly, always run:

```bash
dexdo --data-dir "$DATA_DIR" doctor --json
```

Record the binary identity and the manifest's path and content identity. Keep the same binary,
`DATA_DIR`, and manifest path and contents for every read and write below.

Proceed only if doctor succeeds and returns one JSON document with:

- `schema == "dexdo.doctor.v1"`;
- the intended `network`;
- `verdict == "pass"`.

A failed doctor, unexpected schema, missing field, changed manifest, or network mismatch is a stop
condition.

### A2. Obtain the exact PN and destination from the user

An agent may assist with discovery:

```bash
dexdo --data-dir "$DATA_DIR" note list --json
dexdo --data-dir "$DATA_DIR" wallet show --json
```

It must never choose a PN or destination for the user. In particular, it must not select the first
`note list` entry, infer a destination, or use Hot by default. It may submit a write only with a
complete canonical PN and a complete canonical `<dapp_id>::<account_id>` destination explicitly
supplied by the user for this operation:

```bash
PN='the-user-supplied-full-note-address'
DEST='the-user-supplied-full-destination-address'
```

Both addresses must belong to the doctor network. `note withdraw` does not infer or default either
address. If the user participates in the Points & Rewards Program, obtain explicit confirmation
that this exact `$DEST` is registered in the Program and linked to the user's account at
[app.dex.do](https://app.dex.do/). Do not infer Program registration from the wallet address or
from local dexdo data.

### A3. Perform and validate the structured preflight

Use structured reads:

```bash
dexdo --data-dir "$DATA_DIR" note balance --note-addr "$PN" --json
dexdo --data-dir "$DATA_DIR" wallet show --json
```

Treat every raw amount as an unsigned decimal string. Parse and add it with an exact integer type
(`u128`, arbitrary-precision integer, or `BigInt` after rejecting signs); never use floating point
or JavaScript `Number`. Reject malformed, negative, fractional, or overflowing values.

The `note list --json` command in A2 is discovery only. If used, validate
`schema == "dexdo.note_list.v1"` and the `notes` array before presenting its entries, but do not
require the selected PN to appear: a PN used with an explicit external `--note-key` may be absent
from this instance's local pool. The selected PN is checked separately and fail-closed with
`note balance --json`.

For `note balance --json`, require:

- `schema == "dexdo.note_balance.v1"` and `note == PN`;
- `balance.read == true` with a `currencies` array;
- `locked_in_orders.read == true` with a `currencies` array;
- an `ecc` array;
- `busy.read == true` and `busy.busy == false`;
- `withdraw_gate.read == true` and `withdraw_gate.held_by == null`.

`withdraw_gate` is the aggregate result of all 11 contract gates, decoded from one account-storage
snapshot and evaluated in the same order as `withdrawTokens`. `read == true` with
`held_by == null` corresponds to the human CLI line:

```text
  all 11 withdrawTokens STATE gates read and closed -- the eleven are note STATE only; gas and amounts are settled when the message is sent and are NOT among them, so this is NOT a statement that a withdrawal will succeed
```

Here `closed` means that no decoded STATE gate holds the PN. Gas and transfer amounts are not among
these 11 gates. A non-null `held_by` names the first blocker and corresponds to this human CLI form:

```text
  held by <gate> (<code>): <what it holds> -- <next step> [PrivateNote.sol:<line>, exit_code=<code>]
```

`read == false` means the aggregate is unknown and must never authorize submission. Automation
must not expect a separate 11-element array or parse either human-readable line.

Only after a map or array was successfully read may an absent currency-`2` entry mean zero. A
missing, malformed, or unreadable map is unknown, never zero. Reject duplicate currency IDs.

Record:

- `PN_trading_before`: the `raw` value for currency `2` in `balance.currencies`;
- `PN_ECC2_before`: the `raw` value for currency `2` in the root `ecc` array;
- the complete `balance.currencies` spendable token map, including any non-SHELL currencies.

These JSON paths correspond respectively to the human `ECC[2] SHELL` line under
`PrivateNote.getDetails spendable token balance (trading money)` and the separate
`SHELL gas ECC[2]` line. Do not derive either value by parsing the human output or use
`locked_in_orders.currencies` as a balance.

For a bound Hot destination, `wallet show --json` currently has no `schema` field. Require its
root `bindings` and `unreadable` arrays and exactly one binding whose `network` equals the doctor
network and whose complete `hot` value equals `$DEST`. Require `balance_unread == null` and a
valid `shell_raw` string, reject an `unreadable` entry for the target network, and record
`shell_raw` as `destination_before`. A different-network binding or merely the first binding is
not a match.

For a non-Hot destination, unattended automation requires a configured authoritative adapter. One
read must return the selected network, exact canonical destination address, and raw `ECC[2]` SHELL
as an unsigned decimal string. Before and after readings must come from the same adapter and must
match the doctor network and full `$DEST`. A web explorer is a human interface, not a machine
contract. Without such an adapter, stop before submission.

Allow no other incoming or outgoing destination transfer until verification is complete. Calculate
with exact integer arithmetic:

```text
expected_SHELL_raw = PN_trading_before + PN_ECC2_before
```

This formula verifies only currency `2` (SHELL). `withdrawTokens` transfers the entire spendable
token map. If that map contains another currency, submit only when an authoritative per-currency
source can provide before/after readings and an exact expected-delta method for every such currency.
Without those pre-submit verification inputs, stop before the irreversible write.

`note outstanding` has no JSON surface and is human-only diagnostics. An agent must not parse its
text as authorization. Its empty output never overrides a held or unreadable structured gate.

The current CLI has a mandatory pre-submit contract-generation guard. When A5 runs, if it reports
`PREVIOUS contract generation`, no withdrawal was submitted. Stop; do not use an older CLI or call
the contract manually. Preserve the PN address, owner key, client version, manifest identity, and
the output of the A3 `note balance --json` read. Report the case in the
[DEX.DO Tech Talks - Chat](https://t.me/c/3819653418/1) for a generation-specific recovery procedure, but never
send the owner key or its contents to the chat. A current-generation PN created for new activity
does not recover the old balance.

### A4. Obtain the required confirmation

Obtain a positive finite `verification_timeout` from the operator or a fixed host policy. It is the
maximum elapsed time for post-submit read-only polling, measured from the moment `note withdraw`
returns. It does not cancel a submitted message and must not be reset or extended between polls. If
neither the operator nor host policy supplies this value, stop before the write; this guide defines
no default.

After every pre-submit predicate passes, show the user all of the following together:

- doctor network;
- complete PN address;
- complete destination address;
- `expected_SHELL_raw`;
- `verification_timeout`;
- the exact expanded `note withdraw` command proposed for execution, without key contents.
- the fact that withdrawal links the note to the destination wallet, deanonymizes it, and
  permanently prevents that note from posting another sell offer; continuing to sell requires a
  new note.

Receive explicit confirmation of those exact values and that exact command. A prior request to
inspect, explain, or prepare the withdrawal is not confirmation to submit it. If any value or
command changes, obtain confirmation again.

### A5. Submit exactly once

When the PN owner key is in the current pool:

```bash
dexdo --data-dir "$DATA_DIR" note withdraw \
  --note-addr "$PN" \
  --to "$DEST" \
  --acknowledge-note-retirement
```

When the owner key is stored separately:

`--note-key` is a path to a private file whose trimmed contents must match `^[0-9A-Fa-f]{64}$`.
Obtain that path from the operator's external custody record. If the source is another PN pool,
parse it as JSON, require exactly one `notes[]` entry whose complete `address` equals `PN`, and take
only that entry's string `owner_secret_key_hex`. Require it to match the same 64-hex pattern.

Write the secret directly to a newly created owner-only file without sending it to stdout, stderr,
logs, shell history, or an intermediate file. On Unix, set mode `0600` before writing and verify the
resulting mode. Set `NOTE_KEY` to that file's absolute path; never set it to the secret value.

```bash
dexdo --data-dir "$DATA_DIR" note withdraw \
  --note-addr "$PN" \
  --note-key "$NOTE_KEY" \
  --to "$DEST" \
  --acknowledge-note-retirement
```

Keep the owner key private. Never print its contents, paste them on the command line, include them in
logs or messages, or commit them.

The acknowledgement flag is mandatory here because an agent or automation run cannot answer the
terminal prompt. Add it only after the explicit confirmation in A4; it is not a way to infer or
bypass the operator's consent.

Capture the command's complete stdout and stderr and its start and finish times. The current command
has no JSON result surface and may provide no message or transaction identity. Preserve such an
identity if a future or platform-specific output includes one, but its absence is expected and is
never permission to retry. The line `withdrawTokens submitted` proves only submission. Never
automatically repeat `note withdraw`.

### A6. Verify within the confirmed deadline

When `note withdraw` returns, derive one fixed monotonic deadline from the confirmed
`verification_timeout`. Poll only the read-only PN and destination checks until every predicate
below passes or that deadline expires. Do not reset or extend the deadline and do not submit another
write while polling.

```bash
dexdo --data-dir "$DATA_DIR" note balance --note-addr "$PN" --json
```

For the bound Hot wallet, also run:

```bash
dexdo --data-dir "$DATA_DIR" wallet show --json
```

For a non-Hot destination, read it through the same authoritative adapter used for the baseline.

A confirmed current-SHELL result requires all of these post-submit predicates:

- `note balance --json` again has `schema == "dexdo.note_balance.v1"` and `note == PN`;
- `balance.read == true`, every spendable currency is absent or has raw value `0`, and in
  particular currency `2` is absent or zero;
- `ecc` is present and valid, and currency `2` is absent or has raw value `0`;
- `withdraw_gate.read == true` and `withdraw_gate.held_by == "_hasWithdrawn"`;
- the authoritative destination read succeeds and
  `destination_after - destination_before == expected_SHELL_raw` exactly;
- every non-SHELL spendable currency recorded before submission has its expected authoritative
  destination delta.

A fully reverted withdrawal is a separate terminal result only when authoritative chain evidence or
the polling history proves that this submission reached the PN and `revertWithdraw` returned it.
Require the complete spendable token map and physical `ECC[2]` to equal their pre-submit values,
`withdraw_gate.read == true` with `withdraw_gate.held_by == null`, and zero authoritative destination
delta for every currency. An unchanged snapshot alone remains `UNRESOLVED` because the message may
still be pending. Otherwise report `REVERTED`; a later attempt is allowed only after the RootPN
liquidity shortfall is resolved and A3 and A4 are repeated with new explicit confirmation. Never
retry automatically.

An absent currency entry means zero only inside a successfully read and validated map. A missing or
unreadable map, failed command, malformed document, mismatched address or network, arithmetic error,
unexpected delta that is neither the confirmed nor fully reverted result above, or expired polling
deadline makes the outcome `UNRESOLVED`.

`UNRESOLVED` does not mean failed. Preserve the exact command, complete stdout and stderr, command
start and finish times, PN, destination, raw before/after readings, manifest identity, confirmed
`verification_timeout`, and any message or transaction identity that was actually emitted. The
current command may emit no such identity; the other evidence remains sufficient for read-only
reconciliation. Never automatically repeat `note withdraw`.

Any native `vmshell` remaining on the PN is expected and is outside the withdrawal.
Withdrawn SHELL arrives at the destination as ECC[2], the traded asset, not as spendable native gas.
A destination with zero native `vmshell` remains gas-starved after the withdrawal.

### A7. Handle late SHELL after withdrawal

An old order cancellation or deal refund can send physical SHELL `ECC[2]` to the PN after the
one-shot withdrawal. `note sweep` moves only this late physical pocket; it does not move the
trading balance or native `vmshell` and is not a second withdrawal.

Perform a new structured preflight. Proceed only when `withdraw_gate.read == true`,
`withdraw_gate.held_by == "_hasWithdrawn"`, and a successfully read currency-`2` `ECC[2]` value
is greater than zero. The PN and destination must again be complete values explicitly supplied by
the user for this operation.

Record the authoritative destination baseline, set the exact expected raw SHELL delta to the
pre-sweep `ECC[2]` value, and show the user the network, complete addresses, amount, and exact
expanded command. Obtain explicit confirmation for the sweep; confirmation of the withdrawal does
not authorize this separate write.

```bash
dexdo --data-dir "$DATA_DIR" note sweep \
  --note-addr "$PN" \
  --to "$DEST"
```

With a separately stored owner key:

```bash
dexdo --data-dir "$DATA_DIR" note sweep \
  --note-addr "$PN" \
  --note-key /secure/path/note-owner.key \
  --to "$DEST"
```

Interpret the CLI verdict by its complete prefix:

- `SWEEP CONFIRMED` means the PN physical pocket was observed to decrease;
- `SWEEP UNVERIFIED` means it did not decrease within the confirmation window and the transfer may
  still arrive.

Swept SHELL also arrives as ECC[2], not as spendable native gas, and does not unblock a gas-starved
destination wallet.

Never classify either result by matching the substring `CONFIRMED`. Neither prefix alone proves
that the destination was credited. Re-read both sides and require the exact authoritative
destination delta. On any ambiguity, mark the sweep `UNRESOLVED` and never automatically retry it.
