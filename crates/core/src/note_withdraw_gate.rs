//! which of `withdrawTokens`'s eleven gates is holding a note's money.

//! `PrivateNote.withdrawTokens` (`contracts/dex/PrivateNote.sol:2466`) refuses on eleven separate
//! `require`s. `getDetails()` exposes two of them. An operator whose money is locked therefore reads
//! `busyAddress: not busy`, concludes the note is free, and an hour later the chain answers
//! `exit_code=121`. That is not a wording problem: the operator was shown a complete-looking answer
//! to two of eleven questions, and the missing nine were not marked missing.

//! The nine are readable today without touching the contract. Every one of them is a storage field,
//! and the shipped `PrivateNote.abi.json` carries a `fields` section, so `decode_storage_fields`
//! over the note's own account BOC yields all eleven from the account snapshot the caller already
//! holds -- no getter, no second chain read, no contract change.

//! Two properties this module exists to hold, both of them safety rather than presentation:

//! 1. **The answer is the FIRST unclosed gate, in contract order.** The contract evaluates its
//! `require`s top to bottom, so the first unclosed one is the gate that actually fired. Reporting
//! all eleven readings would answer a question the operator did not ask -- they asked what is
//! holding their money, not for eleven numbers.
//! 2. **A field that could not be read is never reported as closed.** `Unreadable` is a third answer
//! for the same reason `NoteBusyLatch::Unknown` is: a gate that was not read is not evidence that
//! it is open, and rendering it as closed would rebuild the exact defect one level down --
//! another complete-looking answer covering less than it appears to.

use serde_json::Value;

/// One `require` in `PrivateNote.withdrawTokens`, carrying the reading that closed nothing.

/// The variants are in contract order. Each carries the measured value rather than a bare name,
/// because "held by `_liveDeals`" and "held by `_liveDeals`: 3 deals" are different answers to the
/// operator's next question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WithdrawGate {
    /// `require(!_hasWithdrawn, ERR_INVALID_STATE)` -- the note already paid out once.
    HasWithdrawn,
    /// `require(!_busy.hasValue(), ERR_NOTE_BUSY)` -- an in-flight operation holds the latch.
    Busy { with: String },
    /// `require(_stakes.empty(), ERR_NOTE_BUSY)` -- the gate with no getter at all.
    Stakes { count: usize },
    /// `require(_debt == 0, ERR_DEBT_NON_ZERO)`.
    Debt { raw: u128 },
    /// `require(locked == 0, ERR_NON_ZERO_BALANCE)` for one `_lockedInOrders` entry.
    LockedInOrders { token_type: u32, locked: u128 },
    /// `require(_pendingPlaceBuyLock == 0, ERR_NON_ZERO_BALANCE)`.
    PendingPlaceBuyLock { raw: u128 },
    /// `require(_pendingBatchBuyLock == 0, ERR_NON_ZERO_BALANCE)`.
    PendingBatchBuyLock { raw: u128 },
    /// `require(_openOrderCount == 0, ERR_OPEN_ORDERS_EXIST)`.
    OpenOrders { count: u32 },
    /// `require(_restingInf.empty(), ERR_OPEN_ORDERS_EXIST)`.
    RestingInference { count: usize },
    /// `require(_pendingInf.empty(), ERR_OPEN_ORDERS_EXIST)`.
    PendingInference { count: usize },
    /// `require(_liveDeals.empty(), ERR_OPEN_ORDERS_EXIST)`.
    LiveDeals { count: usize },
}

impl WithdrawGate {
    /// The storage field this gate reads, spelled as the contract spells it.
    pub fn field(&self) -> &'static str {
        match self {
            Self::HasWithdrawn => "_hasWithdrawn",
            Self::Busy { .. } => "_busy",
            Self::Stakes { .. } => "_stakes",
            Self::Debt { .. } => "_debt",
            Self::LockedInOrders { .. } => "_lockedInOrders",
            Self::PendingPlaceBuyLock { .. } => "_pendingPlaceBuyLock",
            Self::PendingBatchBuyLock { .. } => "_pendingBatchBuyLock",
            Self::OpenOrders { .. } => "_openOrderCount",
            Self::RestingInference { .. } => "_restingInf",
            Self::PendingInference { .. } => "_pendingInf",
            Self::LiveDeals { .. } => "_liveDeals",
        }
    }

    /// The line in `contracts/dex/PrivateNote.sol` this gate is written on.

    /// Carried so the operator -- and the next reader of a refusal in a log -- can go and read the
    /// condition themselves rather than take this module's word for it.

    /// THESE NUMBERS ARE NOT MAINTAINED BY HAND, and the previous set is why. All eleven
    /// were correct once and all eleven were wrong together -- every one short by exactly 181,
    /// because the contract grew that much above `withdrawTokens` and nothing here was watching. A
    /// pointer that is quietly off by 181 is worse than no pointer: `_liveDeals` sent its reader to
    /// a doc comment about `eccAmount`, which reads like an answer.

    /// They stay constants in this function because the shipped binary must not carry and parse a
    /// 3700-line contract to render one refusal. What changed is that they are no longer ASSERTED
    /// by hand: `note_withdraw_gate_contract_line_1744_tests` derives each one from the vendored
    /// source and fails the moment the contract moves, so the next shift is caught by a red test
    /// rather than by an operator following a pointer into the wrong function.
    pub fn contract_line(&self) -> u32 {
        match self {
            Self::HasWithdrawn => 2649,
            Self::Busy { .. } => 2650,
            Self::Stakes { .. } => 2651,
            Self::Debt { .. } => 2652,
            Self::LockedInOrders { .. } => 2659,
            Self::PendingPlaceBuyLock { .. } => 2661,
            Self::PendingBatchBuyLock { .. } => 2662,
            Self::OpenOrders { .. } => 2663,
            Self::RestingInference { .. } => 2677,
            Self::PendingInference { .. } => 2682,
            Self::LiveDeals { .. } => 2683,
        }
    }

    /// The exit code this gate raises, taken from `contracts/dex/modifiers/errors.sol`.

    /// This is what ties the reading to the refusal the operator already has in their terminal: the
    /// first unclosed gate is the one that fired, so its code must be the code they saw.
    pub fn exit_code(&self) -> u16 {
        match self {
            Self::HasWithdrawn => 151,
            Self::Busy { .. } | Self::Stakes { .. } => 121,
            Self::Debt { .. } => 150,
            Self::LockedInOrders { .. }
            | Self::PendingPlaceBuyLock { .. }
            | Self::PendingBatchBuyLock { .. } => 144,
            Self::OpenOrders { .. }
            | Self::RestingInference { .. }
            | Self::PendingInference { .. }
            | Self::LiveDeals { .. } => 167,
        }
    }

    /// What is holding the money, in the operator's units and with the number that was measured.
    pub fn holds(&self) -> String {
        match self {
            Self::HasWithdrawn => "the note has already withdrawn once".to_string(),
            Self::Busy { with } => format!("an in-flight operation with {with}"),
            Self::Stakes { count } => format!("{count} PMP stake(s)"),
            Self::Debt { raw } => format!("a debt of {raw}"),
            Self::LockedInOrders { token_type, locked } => {
                format!("{locked} locked in orders for token type {token_type}")
            }
            Self::PendingPlaceBuyLock { raw } => format!("a pending place-buy lock of {raw}"),
            Self::PendingBatchBuyLock { raw } => format!("a pending batch-buy lock of {raw}"),
            Self::OpenOrders { count } => format!("{count} open order(s)"),
            Self::RestingInference { count } => format!("{count} resting inference order(s)"),
            Self::PendingInference { count } => format!("{count} pending inference buy(s)"),
            Self::LiveDeals { count } => format!("{count} live deal(s)"),
        }
    }

    /// What the operator does about it, as a command where this client has one.

    /// this arm said "nothing further can be withdrawn", which was a claim about the
    /// CONTRACT made out of an inventory of OUR commands -- and false, because `sweepShell`
    /// requires exactly this state. Part one narrowed it to what the client lacked. Part two built
    /// the command, so the arm now names it: the gap it reported is closed, and an arm that still
    /// reported it would be the same substitution running the other way.

    /// Saying "retry" would be the same lie in the other direction. What this client can do is ours
    /// to state; what the note can do is the contract's.
    pub fn next_step(&self) -> &'static str {
        match self {
            Self::HasWithdrawn => {
                "the trading record is spent and `withdrawTokens` is one-shot, so no further \
                 withdrawal is possible -- but SHELL that ARRIVES after the withdrawal is not \
                 lost: `dexdo note sweep --note-addr '<note-addr>' --to '<dapp_id>::<account_id>'` \
                 moves the note's physical ECC[2] pocket out through the contract's own \
                 `sweepShell`. It lands there as ECC[2], not as spendable gas"
            }
            Self::Busy { .. } => {
                "the latch clears only on the acknowledgement of the operation that set it, or when \
                 that message bounces; resolve that counterparty -- waiting does not clear it"
            }
            // the previous wording said the run's artefacts were "the only key" and that
            // `_stakes` had "no getter". Both are false, and an owner who believes the first writes
            // the money off. Two thirds of the triple are inside the record this very module has
            // already decoded -- `StakeInfo` carries `tokenType` and `oracleListHash`
            // (`contracts/dex/modifiers/modifiers.sol:367-370`), and `client.rs` reads them from
            // exactly that field. The third is recoverable by recomputing the map key over the
            // oracle's events, so the one thing the owner must still supply is a NAME.

            // No command does that walk today, so this deliberately does not promise one: it names
            // what is recoverable and what the owner has to bring, and stops there.
            Self::Stakes { .. } => {
                "cancel the stake with `dexdo oracle cancel-stake`, which needs the (eventId, \
                 oracleListHash, tokenType) triple. The run's artefacts are the quickest source but \
                 NOT the only one: this note's own stake record already carries oracleListHash and \
                 tokenType, and eventId is recoverable by recomputing the stake key over the \
                 oracle's event list until it matches -- so what you must still supply is the \
                 oracle NAME, not a 256-bit hash. If cancel-stake refuses because the market cannot \
                 be cancelled, `dexdo oracle forfeit-stake` is the way out that has no lifecycle \
                 gate -- it ABANDONS the stake, so read its refusal before using it"
            }
            Self::Debt { .. } => "the note owes; the debt must be settled before it can pay out",
            Self::LockedInOrders { .. } => {
                "cancel the orders holding the escrow (`dexdo orders cancel-all`)"
            }
            Self::PendingPlaceBuyLock { .. } | Self::PendingBatchBuyLock { .. } => {
                "a buy is in flight; the lock clears when the book answers, so re-read before acting"
            }
            Self::OpenOrders { .. } => "cancel them (`dexdo orders cancel-all`)",
            Self::RestingInference { .. } => {
                "cancel the resting inference orders (`dexdo orders cancel-all`)"
            }
            Self::PendingInference { .. } => {
                "a buy is in flight; it clears when the book answers, so re-read before acting"
            }
            Self::LiveDeals { .. } => {
                "close them: `dexdo note outstanding --note-addr '<note-addr>'` names them from \
                 the same note address this reading was taken from, then stop or finalize each"
            }
        }
    }
}

/// What one storage snapshot says about the note's ability to withdraw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteWithdrawGate {
    /// All eleven gates were read and all eleven are closed.
    Clear,
    /// The first gate, in contract order, that is not closed.
    Held(WithdrawGate),
    /// A gate could not be read, so no verdict is available -- and none is invented.
    Unreadable { field: &'static str, reason: String },
}

/// The eleven gates, in the order `withdrawTokens` evaluates them.

/// **The guarantee this list carries is internal, and this is the whole of it.** It keeps the
/// reader, the renderer and the regression from drifting apart -- and all three of those live in
/// this repository, so the length assertion in the tests compares them only against each other.

/// It says NOTHING about the contract. A twelfth `require` added to `PrivateNote.sol` and not
/// added here would be missed in silence, and this module would go on printing a complete-looking
/// answer covering eleven of twelve gates: a smaller copy of the defect it was written to remove.

/// That limit is NAMED rather than closed, deliberately. The check that would close it -- counting
/// `require(` in a `.sol` function body as text -- breaks on reformatting, and a guard that cries
/// wolf teaches every reader to ignore it, which leaves the tree worse defended than an honest
/// sentence does. The real close is the contract answering for its own gates, which is's
/// first point and is not client-side work.
pub const WITHDRAW_GATE_FIELDS: [&str; 11] = [
    "_hasWithdrawn",
    "_busy",
    "_stakes",
    "_debt",
    "_lockedInOrders",
    "_pendingPlaceBuyLock",
    "_pendingBatchBuyLock",
    "_openOrderCount",
    "_restingInf",
    "_pendingInf",
    "_liveDeals",
];

/// The distinct exit codes the eleven gates raise, from `contracts/dex/modifiers/errors.sol`.

/// A caller deciding whether a refusal is worth explaining needs the set, not eleven `match` arms,
/// and retyping it at the call site is how it drifts from the gates. The test beside this asserts
/// both directions: every gate's code is in here, and every code in here is raised by some gate.
pub const WITHDRAW_GATE_EXIT_CODES: [u16; 5] = [121, 144, 150, 151, 167];

/// Whether a submit refusal carries an exit code one of the eleven gates raises.

/// The predicate exists so the diagnostic read is not spent on refusals no gate can explain. A
/// withdrawal that failed on gas is not a note-state problem, and printing a gate reading beside it
/// -- "all eleven closed" most of all -- would answer a question the operator did not ask with a
/// fact that does not apply.

/// Keyed on the code, exactly as `exit_code_fragment` writes it, never on the wording around it.
pub fn refusal_carries_a_withdraw_gate_code(error_text: &str) -> bool {
    WITHDRAW_GATE_EXIT_CODES
        .iter()
        .any(|code| error_text.contains(&format!("exit_code={code} (")))
}

/// Read a `uint`-shaped storage field, in either the decimal or `0x` rendering.
fn storage_u128(fields: &Value, name: &'static str) -> Result<u128, String> {
    let raw = fields
        .get(name)
        .ok_or_else(|| format!("{name} is absent from the decoded storage"))?;
    if let Some(n) = raw.as_u64() {
        return Ok(u128::from(n));
    }
    let text = raw
        .as_str()
        .ok_or_else(|| format!("{name} is not a number"))?
        .trim();
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u128::from_str_radix(hex, 16).ok(),
        None => text.parse::<u128>().ok(),
    };
    parsed.ok_or_else(|| format!("{name} does not parse as an integer: {text}"))
}

/// Count the entries of a `map`-shaped storage field, accepting both renderings.

/// Both shapes are accepted for the reason `uint32_map_entry` accepts both: the decoder has been
/// observed to emit a map as an object and as an array of pairs, and a reader that knows only one
/// of them reports a full map as empty -- which is this module's whole failure mode.
fn storage_map_len(fields: &Value, name: &'static str) -> Result<usize, String> {
    match fields.get(name) {
        None | Some(Value::Null) => Err(format!("{name} is absent from the decoded storage")),
        Some(Value::Object(entries)) => Ok(entries.len()),
        Some(Value::Array(entries)) => Ok(entries.len()),
        Some(_) => Err(format!("{name} is neither an object nor an array")),
    }
}

/// The `_lockedInOrders` entries as `(token_type, locked)`, in either rendering.
fn locked_in_orders(fields: &Value) -> Result<Vec<(u32, u128)>, String> {
    let name = "_lockedInOrders";
    let parse_u = |v: &Value| -> Option<u128> {
        if let Some(n) = v.as_u64() {
            return Some(u128::from(n));
        }
        let s = v.as_str()?.trim();
        match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            Some(hex) => u128::from_str_radix(hex, 16).ok(),
            None => s.parse::<u128>().ok(),
        }
    };
    match fields.get(name) {
        None | Some(Value::Null) => Err(format!("{name} is absent from the decoded storage")),
        Some(Value::Object(entries)) => entries
            .iter()
            .map(|(key, value)| {
                let id = parse_u(&Value::String(key.clone()))
                    .and_then(|id| u32::try_from(id).ok())
                    .ok_or_else(|| format!("{name} key {key} is not a token type"))?;
                let locked =
                    parse_u(value).ok_or_else(|| format!("{name}[{key}] is not an amount"))?;
                Ok((id, locked))
            })
            .collect(),
        Some(Value::Array(entries)) => entries
            .iter()
            .map(|entry| {
                let (key, value) = if let Some(object) = entry.as_object() {
                    (
                        object.get("key").or_else(|| object.get("0")),
                        object.get("value").or_else(|| object.get("1")),
                    )
                } else if let Some(pair) = entry.as_array().filter(|pair| pair.len() == 2) {
                    (Some(&pair[0]), Some(&pair[1]))
                } else {
                    (None, None)
                };
                let key = key.ok_or_else(|| format!("{name} row carries no key"))?;
                let value = value.ok_or_else(|| format!("{name} row carries no value"))?;
                let id = parse_u(key)
                    .and_then(|id| u32::try_from(id).ok())
                    .ok_or_else(|| format!("{name} key is not a token type"))?;
                let locked =
                    parse_u(value).ok_or_else(|| format!("{name} value is not an amount"))?;
                Ok((id, locked))
            })
            .collect(),
        Some(_) => Err(format!("{name} is neither an object nor an array")),
    }
}

/// The first `withdrawTokens` gate that is not closed, read from one decoded storage snapshot.

/// Evaluated in the contract's own order, and stopping at the first unclosed gate exactly as the
/// contract stops at the first failed `require`.
pub fn note_withdraw_gate_from_storage(fields: &Value) -> NoteWithdrawGate {
    macro_rules! read {
        ($field:expr, $expr:expr) => {
            match $expr {
                Ok(value) => value,
                Err(reason) => {
                    return NoteWithdrawGate::Unreadable {
                        field: $field,
                        reason,
                    }
                }
            }
        };
    }

    // 1. `_hasWithdrawn` -- bool, and absent is not false.
    let has_withdrawn = read!(
        "_hasWithdrawn",
        match fields.get("_hasWithdrawn") {
            Some(Value::Bool(value)) => Ok(*value),
            Some(Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => Ok(true),
                "false" | "0" => Ok(false),
                other => Err(format!("_hasWithdrawn is not a bool: {other}")),
            },
            Some(_) => Err("_hasWithdrawn is not a bool".to_string()),
            None => Err("_hasWithdrawn is absent from the decoded storage".to_string()),
        }
    );
    if has_withdrawn {
        return NoteWithdrawGate::Held(WithdrawGate::HasWithdrawn);
    }

    // 2. `_busy` -- `optional(address)`, so null is the CLOSED reading and a missing key is not.
    let busy = read!(
        "_busy",
        match fields.get("_busy") {
            None => Err("_busy is absent from the decoded storage".to_string()),
            Some(Value::Null) => Ok(None),
            Some(Value::String(address)) if address.trim().is_empty() =>
                Err("_busy decoded as an empty string".to_string()),
            Some(Value::String(address)) => Ok(Some(address.trim().to_string())),
            Some(Value::Object(object)) => match object
                .get("value")
                .or_else(|| object.get("0"))
                .and_then(Value::as_str)
            {
                Some(address) if !address.trim().is_empty() => Ok(Some(address.trim().to_string())),
                _ => Ok(None),
            },
            Some(_) => Err("_busy is not an address".to_string()),
        }
    );
    if let Some(with) = busy {
        return NoteWithdrawGate::Held(WithdrawGate::Busy { with });
    }

    // 3. `_stakes` -- the gate with no getter at all, which is why this module reads storage.
    let stakes = read!("_stakes", storage_map_len(fields, "_stakes"));
    if stakes > 0 {
        return NoteWithdrawGate::Held(WithdrawGate::Stakes { count: stakes });
    }

    // 4. `_debt`
    let debt = read!("_debt", storage_u128(fields, "_debt"));
    if debt != 0 {
        return NoteWithdrawGate::Held(WithdrawGate::Debt { raw: debt });
    }

    // 5. `_lockedInOrders` -- the contract iterates and requires EVERY entry to be zero, so a
    // present-but-zero entry closes the gate and only a non-zero one holds.
    let locked = read!("_lockedInOrders", locked_in_orders(fields));
    if let Some((token_type, locked)) = locked.into_iter().find(|(_, locked)| *locked != 0) {
        return NoteWithdrawGate::Held(WithdrawGate::LockedInOrders { token_type, locked });
    }

    // 6-7. the two buy locks
    let place = read!(
        "_pendingPlaceBuyLock",
        storage_u128(fields, "_pendingPlaceBuyLock")
    );
    if place != 0 {
        return NoteWithdrawGate::Held(WithdrawGate::PendingPlaceBuyLock { raw: place });
    }
    let batch = read!(
        "_pendingBatchBuyLock",
        storage_u128(fields, "_pendingBatchBuyLock")
    );
    if batch != 0 {
        return NoteWithdrawGate::Held(WithdrawGate::PendingBatchBuyLock { raw: batch });
    }

    // 8. `_openOrderCount`
    let open = read!("_openOrderCount", storage_u128(fields, "_openOrderCount"));
    if open != 0 {
        let count = u32::try_from(open).unwrap_or(u32::MAX);
        return NoteWithdrawGate::Held(WithdrawGate::OpenOrders { count });
    }

    // 9-11. inference: resting, pending, live -- three separate maps on purpose (the contract keeps
    // no counter beside them, so their own emptiness is the only truth).
    let resting = read!("_restingInf", storage_map_len(fields, "_restingInf"));
    if resting > 0 {
        return NoteWithdrawGate::Held(WithdrawGate::RestingInference { count: resting });
    }
    let pending = read!("_pendingInf", storage_map_len(fields, "_pendingInf"));
    if pending > 0 {
        return NoteWithdrawGate::Held(WithdrawGate::PendingInference { count: pending });
    }
    let live = read!("_liveDeals", storage_map_len(fields, "_liveDeals"));
    if live > 0 {
        return NoteWithdrawGate::Held(WithdrawGate::LiveDeals { count: live });
    }

    NoteWithdrawGate::Clear
}

/// The one line both the balance report and the 121/167 refusal print.

/// One line, one call, two places: the operator asked what is holding their money, and a report that
/// answered it differently depending on which command they reached it through would be two answers
/// to one question.
pub fn withdraw_gate_line(reading: &NoteWithdrawGate) -> String {
    match reading {
        // The limit is stated where the claim is, in the same form `Unreadable` states its own.

        // An operator this very report has just taught that "not busy" meant less than it looked
        // must not now be asked to notice one unstressed word. "Nothing blocks a withdrawal" is
        // read as "the withdrawal will go through", and the gas refusal a minute later is the
        // second false answer in a row -- after which the line stops being read at all, which
        // costs more than never having printed it.
        NoteWithdrawGate::Clear => format!(
            "all {} withdrawTokens STATE gates read and closed -- the eleven are note STATE only; \
             gas and amounts are settled when the message is sent and are NOT among them, so this \
             is NOT a statement that a withdrawal will succeed",
            WITHDRAW_GATE_FIELDS.len()
        ),
        NoteWithdrawGate::Held(gate) => format!(
            "held by {} ({}): {} -- {} [PrivateNote.sol:{}, exit_code={}]",
            gate.field(),
            gate.exit_code(),
            gate.holds(),
            gate.next_step(),
            gate.contract_line(),
            gate.exit_code()
        ),
        // The honest half of the requirement: an unreadable gate is reported as an unread gate, and
        // the count says how much of the question went unanswered rather than leaving the reader to
        // assume it was all of it.
        NoteWithdrawGate::Unreadable { field, reason } => format!(
            "unknown -- {field} could not be read ({reason}); fewer than {} gates were checked, so \
             this is NOT a statement that the note can withdraw",
            WITHDRAW_GATE_FIELDS.len()
        ),
    }
}

/// what a note's `withdrawTokens` must deposit at its destination, and whether it did.

/// `PrivateNote.withdrawTokens` moves BOTH of a note's SHELL planes in one message: the trading
/// record `_balance` travels as the first ABI argument of `RootPN.withdrawTokens(_balance,
/// destWalletAddr,..)`, and the account's physical pocket
/// (`address(this).currencies[CURRENCIES_ID_SHELL]`) rides the same call as attached `currencies`.
/// So the figure that must appear at the destination is the SUM of the two, and after the call the
/// note holds zero ECC[2] and is dead.

/// The destination is an ordinary account -- our funding multisig -- not a note, so it has only one
/// plane: the account's raw ECC[2] (`balance_other` with `currency: 2`). Its native `balance` is gas
/// and is deliberately not part of this: the call carries its own `value: 0.1 vmshell`, which never
/// touches ECC[2].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoteWithdrawalArrival {
    /// Trading record `_balance` the note held before the call.
    pub note_trading_record: u128,
    /// Physical ECC[2] on the note ACCOUNT before the call.
    pub note_ecc_pocket: u128,
    /// Destination account ECC[2] before the call.
    pub destination_before: u128,
    /// Destination account ECC[2] after the call settled.
    pub destination_after: u128,
}

impl NoteWithdrawalArrival {
    /// Everything the note gave up: both planes, checked.
    pub fn expected(&self) -> Option<u128> {
        self.note_trading_record.checked_add(self.note_ecc_pocket)
    }

    /// What the destination actually gained.
    pub fn observed(&self) -> Option<u128> {
        self.destination_after.checked_sub(self.destination_before)
    }
}

/// Did the withdrawal ARRIVE, to the unit?

/// This is the assertion was missing. The live proof of `withdrawTokens` submitted the call
/// and then checked only that the NOTE had flipped to `hasWithdrawn` -- a fact that is equally true
/// when the money reached our wallet and when it was sent to an account with no code and no key,
/// where it cannot be recovered. `withdrawTokens` runs at most once per note
/// (`require(!_hasWithdrawn)`), so a wrong destination has no second attempt: this check is the only
/// thing between a return and an irreversible send.

/// Deliberately EXACT, not `>=`. A shortfall means something took a cut on the way and that is a
/// finding to report, not a tolerance to widen; a surplus means the reading caught someone else's
/// credit and the measurement is not about this withdrawal at all.
pub fn check_withdrawal_arrival(arrival: &NoteWithdrawalArrival) -> Result<u128, String> {
    let expected = arrival.expected().ok_or_else(|| {
        format!(
            "note withdrawal: trading record {} + ECC pocket {} overflows u128",
            arrival.note_trading_record, arrival.note_ecc_pocket
        )
    })?;
    let observed = arrival.observed().ok_or_else(|| {
        format!(
            "note withdrawal: destination ECC[2] FELL from {} to {} across the withdrawal",
            arrival.destination_before, arrival.destination_after
        )
    })?;
    if observed != expected {
        return Err(format!(
            "note withdrawal did not arrive: destination ECC[2] gained {observed} raw, expected              {expected} raw (= trading record {} + ECC pocket {}); destination went {} -> {}",
            arrival.note_trading_record,
            arrival.note_ecc_pocket,
            arrival.destination_before,
            arrival.destination_after,
        ));
    }
    Ok(observed)
}

#[cfg(test)]
#[path = "note_withdraw_gate_1515_tests.rs"]
mod note_withdraw_gate_1515_tests;

#[cfg(test)]
#[path = "note_withdraw_arrival_1608_tests.rs"]
mod note_withdraw_arrival_1608_tests;

#[cfg(test)]
#[path = "note_withdraw_gate_stake_advice_1587_tests.rs"]
mod note_withdraw_gate_stake_advice_1587_tests;

#[cfg(test)]
#[path = "note_withdraw_gate_advice_1742_tests.rs"]
mod note_withdraw_gate_advice_1742_tests;

#[cfg(test)]
#[path = "note_withdraw_gate_contract_line_1744_tests.rs"]
mod note_withdraw_gate_contract_line_1744_tests;
