//! `dexdo note deploy`: deploy a wallet-funded `PrivateNote` on the chain in-process through
//! `gosh.ackinacki`, then fold the CLI-compatible result into a `DEXDO_PN_POOL` pool the `seller`/`buyer`
//! already consume. The chain call lives in `note_cmd.rs::run_note_deploy`; the pure schema adapters
//! live here.

use anyhow::{anyhow, bail, Result};
use dexdo_core::params::{NOTE_DEPLOY_PROOF_LAYER_MAX, SHELL_CURRENCY_ID};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use zeroize::Zeroizing;

const UNIT_SCALE: u128 = 1_000_000_000;
const NOTE_DEPLOY_RECOVERY_VERSION: u32 = 1;

/// Note denominations accepted by the in-tree RootPN contract. The pinned SDK's `Nominal` enum is
/// only a parsing convenience in this flow: dexdo constructs the wallet and RootPN messages from
/// the raw value itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoteNominal {
    N100,
    N1000,
    N10000,
    N100000,
    N1000000,
}

impl NoteNominal {
    pub(crate) const ALL: [Self; 5] = [
        Self::N100,
        Self::N1000,
        Self::N10000,
        Self::N100000,
        Self::N1000000,
    ];

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value.to_ascii_lowercase().as_str() {
            "n100" | "100" => Ok(Self::N100),
            "n1000" | "1000" => Ok(Self::N1000),
            "n10000" | "10000" => Ok(Self::N10000),
            "n100000" | "100000" => Ok(Self::N100000),
            "n1000000" | "1000000" => Ok(Self::N1000000),
            other => bail!("unknown nominal `{other}` (use N100|N1000|N10000|N100000|N1000000)"),
        }
    }

    pub(crate) fn count(self) -> u64 {
        match self {
            Self::N100 => 100,
            Self::N1000 => 1_000,
            Self::N10000 => 10_000,
            Self::N100000 => 100_000,
            Self::N1000000 => 1_000_000,
        }
    }

    pub(crate) fn raw_value(self, token_decimals: u64) -> u64 {
        self.count() * token_decimals
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::N100 => "N100",
            Self::N1000 => "N1000",
            Self::N10000 => "N10000",
            Self::N100000 => "N100000",
            Self::N1000000 => "N1000000",
        }
    }
}

/// `RootPN.GAS_DEPOSIT` -- mirrored from the contract's own constant, never bisected:
/// `contracts/dex/modifiers/modifiers.sol` declares `uint128 constant GAS_DEPOSIT = 250_000_000_000;`
/// (250 SHELL). `note_deploy_gas_deposit_mirrors_the_contract_constant` re-derives this number from
/// that source file rather than restating it.

/// From 4.0.33 `RootPN.generateVoucher(skUCommit, isFee)` takes this out of EVERY non-gas deposit
/// before the remainder is checked against `ALLOWED_NOMINALS` (`contracts/dex/RootPN.sol`), because
/// the note it will deploy is created cross-dapp where only ECC[2] crosses -- without the collection
/// the note would be born unable to do anything. A gas voucher (`isFee = true`) pays nothing:
/// charging gas for buying gas would be circular, and that leg's ECC is handed straight back to the
/// same note by `sendEccShellToPrivateNote`.

/// Stated once, in `dexdo_core::params`: the wallet refill campaign reads it too, and there it used
/// to be a written-out sum.
// Brought in by name so the doc links to it in this module resolve; the code paths reach it
// through `dexdo_core::params`.
#[allow(unused_imports)]
pub(crate) use dexdo_core::params::ROOT_PN_GAS_DEPOSIT_RAW;

/// Raw ECC[2] SHELL the operator wallet must hold before `note deploy` will spend anything for
/// `nominal` -- the figure `note wallet`'s stage-two funding recipe prints, taken from the deploy's
/// own requirement rather than restated beside it.

/// Keeping this beside [`ROOT_PN_GAS_DEPOSIT_RAW`] prevents the attached figure from entering
/// `note_cmd.rs`, where the prover and deploy value must remain the nominal.
pub(crate) fn operator_wallet_funding_raw(nominal: NoteNominal) -> u128 {
    note_deploy_shell_ecc_required_raw(
        nominal.raw_value(dexdo_core::private_note::proof::TokenType::Shell.decimals()),
    )
}

/// Raw NATIVE balance the same address must hold before dexdo submits the canonical state-init --
/// stage ONE of the recipe, and deliberately not [`operator_wallet_funding_raw`].

/// The two stages fund different things, and only ONE of them has anything to do with the nominal.
/// probed the flag-16 non-bounceable leg on-chain and read back `balance` in full with
/// `balance_other[2]` at zero: that leg becomes the wallet's own native gas, never ECC[2], and
/// native gas can never be spent as currency again. So this stage is sized to the one thing it
/// buys -- deploying the wallet, and leaving it able to send -- from
/// [`dexdo_core::params::OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE`], which carries the live deploy
/// receipt that measures it. It takes no nominal, because asking for a nominal-sized amount of
/// permanent gas is what this used to do.

/// Stage two is the ECC[2] `note deploy` actually spends, and that is the stage the nominal belongs
/// to. That is why the recipe prints two different amounts rather than one number twice.
pub(crate) fn operator_wallet_predeploy_native_raw() -> u128 {
    dexdo_core::params::OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE
}

/// The two named summands of [`operator_wallet_funding_raw`], so a recipe can say what the user is
/// paying for without restating the sum.

/// Only the nominal is named: the rest is whatever the single source of truth has left over, so a
/// printed breakdown can never add up to something other than the figure `note deploy` checks.
pub(crate) fn operator_wallet_funding_summands_raw(nominal: NoteNominal) -> (u128, u128) {
    let nominal_raw =
        u128::from(nominal.raw_value(dexdo_core::private_note::proof::TokenType::Shell.decimals()));
    let gas_deposit_raw = operator_wallet_funding_raw(nominal) - nominal_raw;
    (nominal_raw, gas_deposit_raw)
}

/// Everything a SHELL `note deploy` must already find in the funding wallet's ECC[2] before it
/// submits its first wallet POST, as two summands:

/// - the NOMINAL (`raw_value`), the money the note will be worth;
/// - `RootPN.GAS_DEPOSIT` ([`ROOT_PN_GAS_DEPOSIT_RAW`]), which `generateVoucher` takes out of every
/// non-gas deposit before the remainder is matched against `ALLOWED_NOMINALS`, and which the
/// contract hands to the new note as its ECC[2] gas.

/// There is no third summand. `note deploy` used to buy a second, `isFee = true` voucher of
/// 100 SHELL on top, so the wallet was charged for gas the note already had: `RootPN` credits every
/// note it creates with the whole `GAS_DEPOSIT`, which is 250 SHELL.

/// Measured, not assumed. The acceptance suite reads the seller note's ECC[2] before each scenario
/// and reports 350 -> 330 -> 310 -> 290 -> 270 across five of them -- 20 SHELL per full scenario, so the
/// birth deposit alone carries about a dozen. Dropping the voucher took one note deploy on live
/// the test chain from 252 s to 113 s, the second halo2 proof being the whole difference. A note that
/// needs more gas takes it later through `dexdo note topup`, which is the command for exactly that
/// and does not cost a deploy its second proof.

/// The result is `u128` and the argument `u64`, so the sum cannot overflow.
pub(crate) fn note_deploy_shell_ecc_required_raw(raw_value: u64) -> u128 {
    note_deploy_voucher_wire_raw(false, raw_value)
}

/// What the funding wallet must attach for a voucher whose NOMINAL is `raw_value` -- since 4.0.33 the
/// two are no longer the same figure, and keeping them one variable is what makes every nominal fail.

/// `generateVoucher` computes `nominal = attached - GAS_DEPOSIT` on the non-gas (`isFee = false`)
/// path, then checks THAT against `ALLOWED_NOMINALS`: sending a bare N10000 leaves 9 750 and is
/// refused with `ERR_NOT_ALLOWED` (141), and a bare N100 does not even reach the list
/// (`ERR_BELOW_GAS_DEPOSIT`, 408). The gas voucher (`isFee = true`) is deducted nothing.

/// The nominal must NOT follow this number anywhere else: the halo2 prover, the persisted checkpoint
/// and `deployPrivateNote`'s `value` all keep the nominal, because `VoucherGenerated` emits the
/// POST-deduction figure and a proof built over the attached amount is a public-input mismatch
/// (`ERR_INVALID_ZKPROOF`, 137) discovered only after the wallet has already spent.
/// `GAS_DEPOSIT` as the CONTRACT declares it -- the source of truth for every money expectation in
/// the tests below, and deliberately NOT `ROOT_PN_GAS_DEPOSIT_RAW`, which is the value under test.
/// Anchoring an expectation to that constant would measure the client against a second copy of
/// itself: changing it to 251 would leave every assertion green while the wallet
/// attached `N+251`, the contract derived `N+1`, and RootPN rejected it.

/// This used to fall back to a literal when the in-tree sources were the generation before 4.0.33
/// and declared no `GAS_DEPOSIT` at all -- a fallback guarded by the manifest still reading 4.0.32.
/// The 4.0.33 sources have merged, that generation is withdrawn, and the fallback is gone with it:
/// the declaration is now the only oracle, and a tree that does not carry one is a broken tree
/// rather than an older one. This is what closes's fourth question -- if the CONTRACT moves to
/// another figure, nothing in this repository can stay quietly consistent with a stale copy.
#[cfg(test)]
pub(crate) fn contract_gas_deposit_raw() -> u64 {
    const MODIFIERS: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../contracts/dex/modifiers/modifiers.sol"
    ));

    MODIFIERS
        .lines()
        .find(|line| line.contains("constant GAS_DEPOSIT"))
        .expect("modifiers.sol declares GAS_DEPOSIT as a constant")
        .split('=')
        .nth(1)
        .expect("GAS_DEPOSIT value")
        .split(';')
        .next()
        .expect("GAS_DEPOSIT terminator")
        .trim()
        .replace('_', "")
        .parse()
        .expect("numeric GAS_DEPOSIT")
}

pub(crate) fn note_deploy_voucher_wire_raw(is_fee: bool, raw_value: u64) -> u128 {
    if is_fee {
        u128::from(raw_value)
    } else {
        dexdo_core::params::note_deploy_wallet_funding_raw(raw_value)
    }
}

#[derive(Debug)]
pub(crate) struct NoteAccountSnapshot {
    pub(crate) address: String,
    pub(crate) status: String,
    pub(crate) native_raw: u128,
    pub(crate) ecc: Vec<(u32, u128)>,
    pub(crate) code_hash: Option<String>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum NoteBalanceMap {
    Known(Vec<(u32, u128)>),
    Unknown(String),
}

#[derive(Debug)]
pub(crate) struct NoteGetterBalanceMaps {
    pub(crate) balance: NoteBalanceMap,
    pub(crate) locked_in_orders: NoteBalanceMap,
}

#[derive(Debug)]
pub(crate) struct NoteBalanceView {
    pub(crate) account: NoteAccountSnapshot,
    pub(crate) getters: NoteGetterBalanceMaps,
}

pub(crate) fn build_note_balance_view(
    note_addr: &str,
    account: Option<NoteAccountSnapshot>,
    getters: NoteGetterBalanceMaps,
) -> Result<NoteBalanceView> {
    let account = account.ok_or_else(|| {
        anyhow!(
            "cannot read PrivateNote account {}: account not found/null; refusing to report zero balance",
            dexdo_core::address::display(note_addr)
        )
    })?;
    Ok(NoteBalanceView { account, getters })
}

pub(crate) fn note_getter_balance_maps(details: Option<&Value>) -> NoteGetterBalanceMaps {
    let Some(details) = details else {
        return NoteGetterBalanceMaps {
            balance: NoteBalanceMap::Unknown("getDetails returned no data".to_string()),
            locked_in_orders: NoteBalanceMap::Unknown("getDetails returned no data".to_string()),
        };
    };
    NoteGetterBalanceMaps {
        balance: parse_balance_map(&details["balance"], "balance"),
        locked_in_orders: parse_balance_map(&details["lockedInOrders"], "lockedInOrders"),
    }
}

pub(crate) fn unknown_note_getter_balance_maps(reason: impl Into<String>) -> NoteGetterBalanceMaps {
    let reason = reason.into();
    NoteGetterBalanceMaps {
        balance: NoteBalanceMap::Unknown(reason.clone()),
        locked_in_orders: NoteBalanceMap::Unknown(reason),
    }
}

/// The note's `_busy` latch as `PrivateNote.getDetails()` renders it: an `optional(address)` that is
/// null while the note holds no latch and carries the counterparty's address while it does.

/// `dex::ERR_NOTE_BUSY (121)` tells the operator to check what the note is busy with on
/// `dexdo note balance`, and that command printed no such line -- a latched note rendered exactly
/// like a free one, `status: Active` included. The latch is not a wait-and-retry state: the contract
/// clears it only on the acknowledgement of the operation that set it, or when that message bounces
/// (`contracts/dex/PrivateNote.sol`), so a missing line was not something the operator could sit out.

/// `Unknown` is a third answer on purpose. `getDetails` not being readable, or answering without the
/// field at all, is not evidence that the note is free, and rendering it as "not busy" would be the
/// same defect one level down.
#[derive(Debug, PartialEq)]
pub(crate) enum NoteBusyLatch {
    /// `getDetails` answered and the note holds no latch.
    Free,
    /// `getDetails` answered and the note is latched to this counterparty.
    BusyWith(String),
    /// The latch was not read, so neither answer may be reported.
    Unknown(String),
}

/// Read the latch off the same `getDetails()` response the balance maps are read from -- no second
/// chain read: `run_note_balance` calls `private_note_details` once and both parsers see that value.
pub(crate) fn note_busy_latch(details: Option<&Value>) -> NoteBusyLatch {
    let Some(details) = details else {
        return NoteBusyLatch::Unknown("getDetails returned no data".to_string());
    };
    // Both spellings, to match `client.rs::field`, which is what `busy_with` reads this same field
    // through when it builds the 121 error. The pinned decoder emits the ABI's camelCase and I have
    // not observed the snake_case form on the wire; it is accepted so that the reader the operator is
    // sent to and the reader that sends them cannot disagree about which spellings count.
    match details
        .get("busyAddress")
        .or_else(|| details.get("busy_address"))
    {
        None => NoteBusyLatch::Unknown("busyAddress field unavailable".to_string()),
        Some(Value::Null) => NoteBusyLatch::Free,
        Some(Value::String(address)) if address.trim().is_empty() => {
            // `optional(address)` decodes to null or to an address, never to "". An empty string is a
            // decoding fault, and a fault is not evidence that the note is free.
            NoteBusyLatch::Unknown("busyAddress decoded as an empty string".to_string())
        }
        Some(Value::String(address)) => NoteBusyLatch::BusyWith(address.trim().to_string()),
        Some(_) => NoteBusyLatch::Unknown("busyAddress is not an address".to_string()),
    }
}

#[cfg(test)]
#[path = "note_balance_render_1714_tests.rs"]
mod note_balance_render_1714_tests;

pub(crate) fn render_note_balance(view: &NoteBalanceView) -> String {
    let mut out = String::new();
    let account = &view.account;
    writeln!(
        &mut out,
        "PrivateNote {}",
        dexdo_core::address::display(&account.address)
    )
    .unwrap();
    writeln!(&mut out, "status: {}", account.status).unwrap();
    if let Some(code_hash) = account.code_hash.as_deref() {
        writeln!(&mut out, "code_hash: {code_hash}").unwrap();
    } else {
        writeln!(&mut out, "code_hash: unknown").unwrap();
    }
    writeln!(
        &mut out,
        "SHELL gas ECC[2]: {} SHELL (raw {})",
        decimal_units(account.ecc_value(SHELL_CURRENCY_ID)),
        account.ecc_value(SHELL_CURRENCY_ID)
    )
    .unwrap();
    writeln!(
        &mut out,
        "VMSHELL native gas: {} vmshell (raw {})",
        decimal_units(account.native_raw),
        account.native_raw
    )
    .unwrap();
    render_ecc_map(
        &mut out,
        "account ECC balances (deployment gas)",
        &NoteBalanceMap::Known(account.ecc.clone()),
    );
    render_ecc_map(
        &mut out,
        "PrivateNote.getDetails spendable token balance (trading money)",
        &view.getters.balance,
    );
    render_locked_in_orders(&mut out, &view.getters.locked_in_orders);
    out
}

/// The `lockedInOrders` section, titled by what the field actually measures.

/// The field is honest; the heading over it was not. `_lockedInOrders` is PMP `OrderBook`
/// collateral -- the contract's own comment says incremented on order placement, decremented on
/// fill or cancel -- and an inference buy never touches it: `placeInferenceBuy` debits
/// `_balance[CURRENCIES_ID_SHELL]` directly. So a note with 6.1 SHELL locked in a standing inference
/// order printed `none reported` under a heading an operator reads as "what is holding my money".

/// Measured on the chain: the trading record dropped by exactly 6.100000000 while this said nothing.

/// The direction of the error is what makes it worth a change rather than a note. It pointed at "all
/// clear", so the operator either plans a spend that will hit `ERR_LOW_VALUE` and looks for the
/// cause in the wrong place, or -- worse -- reads the reduced record as money GONE and opens an
/// investigation into a loss that is a standing order waiting to be cancelled.

/// Naming the field rather than adding the inference figure here: that figure is a second chain
/// read in a command that makes one, and the decision to widen it belongs to whoever owns the
/// command. What this fixes is that no wrong conclusion is available from the line any more.
fn render_locked_in_orders(out: &mut String, map: &NoteBalanceMap) {
    render_ecc_map(
        out,
        "PrivateNote.getDetails lockedInOrders (PMP OrderBook collateral)",
        map,
    );
    // Printed on every branch, empty or not: this file's rule is that no state is reported by
    // silence, and the caveat is exactly as true when the field has a figure in it.
    writeln!(
        out,
        "  inference-order escrow is NOT counted by this field -- a standing inference buy is paid \
         from the trading record itself. Use `dexdo note outstanding` to see standing orders."
    )
    .unwrap();
}

/// The `_busy` latch section of `dexdo note balance`, in the shape the getter sections above use: a
/// titled line and one indented line that is always emitted, so no state is reported by silence.
pub(crate) fn render_note_busy_latch(latch: &NoteBusyLatch) -> String {
    let mut out = String::new();
    writeln!(
        &mut out,
        "PrivateNote.getDetails busyAddress (in-flight operation latch):"
    )
    .unwrap();
    match latch {
        NoteBusyLatch::Free => writeln!(&mut out, "  not busy").unwrap(),
        // `_busy` holds a PMP, an order book, or the destination note of an outbound transfer -- all
        // shared-DApp accounts, which is the address form `display` renders.
        NoteBusyLatch::BusyWith(address) => writeln!(
            &mut out,
            "  busy with {}",
            dexdo_core::address::display(address)
        )
        .unwrap(),
        NoteBusyLatch::Unknown(reason) => writeln!(&mut out, "  unknown ({reason})").unwrap(),
    }
    out
}

/// The `withdrawTokens` gate section of `dexdo note balance`.

/// The section exists because the two lines above it are a COMPLETE-LOOKING answer to two of the
/// eleven questions `withdrawTokens` asks. An operator reading `not busy` and `none reported`
/// concluded the note was free and was refused `exit_code=121` an hour later -- not from
/// carelessness, but because the other nine gates were neither shown nor marked missing.

/// So this section is obliged to do one of exactly two things: name the gate that is holding the
/// money, or say plainly that it did not check them all. Silence, and a partial check that reads
/// like a full one, are the defect itself.
pub(crate) fn render_note_withdraw_gate(reading: &dexdo_core::NoteWithdrawGate) -> String {
    let mut out = String::new();
    writeln!(
        &mut out,
        "PrivateNote withdrawTokens gates (what holds the money):"
    )
    .unwrap();
    writeln!(&mut out, "  {}", dexdo_core::withdraw_gate_line(reading)).unwrap();
    out
}

#[cfg(test)]
#[path = "note_withdraw_gate_render_1515_tests.rs"]
mod note_withdraw_gate_render_1515_tests;

#[cfg(test)]
#[path = "note_busy_1391_tests.rs"]
mod note_busy_1391_tests;

impl NoteAccountSnapshot {
    fn ecc_value(&self, id: u32) -> u128 {
        self.ecc
            .iter()
            .find(|(currency, _)| *currency == id)
            .map(|(_, value)| *value)
            .unwrap_or(0)
    }
}

fn render_ecc_map(out: &mut String, title: &str, map: &NoteBalanceMap) {
    writeln!(out, "{title}:").unwrap();
    match map {
        NoteBalanceMap::Known(entries) if entries.is_empty() => {
            writeln!(out, "  none reported").unwrap();
        }
        NoteBalanceMap::Known(entries) => {
            let mut entries = entries.clone();
            entries.sort_by_key(|(id, _)| *id);
            for (id, value) in entries {
                if id == SHELL_CURRENCY_ID {
                    writeln!(
                        out,
                        "  ECC[2] SHELL: {} SHELL (raw {value})",
                        decimal_units(value)
                    )
                    .unwrap();
                } else {
                    writeln!(out, "  ECC[{id}]: raw {value}").unwrap();
                }
            }
        }
        NoteBalanceMap::Unknown(reason) => {
            writeln!(out, "  unknown ({reason})").unwrap();
        }
    }
}

fn decimal_units(raw: u128) -> String {
    format!("{}.{:09}", raw / UNIT_SCALE, raw % UNIT_SCALE)
}

fn parse_balance_map(value: &Value, name: &str) -> NoteBalanceMap {
    if value.is_null() {
        return NoteBalanceMap::Unknown(format!("{name} field unavailable"));
    }
    if let Some(object) = value.as_object() {
        let mut out = Vec::new();
        for (id, amount) in object {
            let Some(id) = parse_u32_key(id) else {
                return NoteBalanceMap::Unknown(format!("{name} contains non-numeric currency id"));
            };
            let Some(amount) = parse_u128_value(amount) else {
                return NoteBalanceMap::Unknown(format!("{name}[{id}] is not a u128"));
            };
            out.push((id, amount));
        }
        return NoteBalanceMap::Known(out);
    }
    if let Some(array) = value.as_array() {
        let mut out = Vec::new();
        for item in array {
            let Some(id) = item
                .get("currency")
                .or_else(|| item.get("id"))
                .and_then(parse_u32_value)
            else {
                return NoteBalanceMap::Unknown(format!("{name} array entry missing currency id"));
            };
            let Some(amount) = item
                .get("value")
                .or_else(|| item.get("amount"))
                .and_then(parse_u128_value)
            else {
                return NoteBalanceMap::Unknown(format!("{name}[{id}] is not a u128"));
            };
            out.push((id, amount));
        }
        return NoteBalanceMap::Known(out);
    }
    NoteBalanceMap::Unknown(format!("{name} has unexpected JSON shape"))
}

fn parse_u32_key(raw: &str) -> Option<u32> {
    raw.parse::<u32>().ok().or_else(|| {
        raw.strip_prefix("0x")
            .or_else(|| raw.strip_prefix("0X"))
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
    })
}

fn parse_u32_value(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .or_else(|| value.as_str().and_then(parse_u32_key))
}

fn parse_u128_value(value: &Value) -> Option<u128> {
    value.as_u64().map(u128::from).or_else(|| {
        let raw = value.as_str()?.trim();
        raw.parse::<u128>().ok().or_else(|| {
            raw.strip_prefix("0x")
                .or_else(|| raw.strip_prefix("0X"))
                .and_then(|hex| u128::from_str_radix(hex, 16).ok())
        })
    })
}

pub(crate) fn normalize_owner_pubkey_hex(raw: &str, label: &str) -> Result<String> {
    let key = raw.trim().trim_start_matches("0x").trim_start_matches("0X");
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{label} must be a 32-byte hex public key, got `{raw}`");
    }
    Ok(key.to_ascii_lowercase())
}

pub(crate) fn derive_owner_pubkey_from_secret_hex(secret_hex: &str) -> Result<String> {
    let secret = secret_hex
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    let bytes = hex::decode(secret)
        .map_err(|e| anyhow!("owner_secret_key_hex must be 32-byte hex: {e}"))?;
    let seed: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("owner_secret_key_hex must be exactly 32 bytes"))?;
    let signing = SigningKey::from_bytes(&seed);
    Ok(hex::encode(signing.verifying_key().as_bytes()))
}

pub(crate) fn ensure_pool_note_keypair_matches(
    note_addr: &str,
    owner_public_key_hex: &str,
    owner_secret_key_hex: &str,
) -> Result<String> {
    let recorded = normalize_owner_pubkey_hex(owner_public_key_hex, "owner_public_key_hex")?;
    let derived = derive_owner_pubkey_from_secret_hex(owner_secret_key_hex)?;
    if recorded != derived {
        bail!(
            "note deploy aborted before writing DEXDO_PN_POOL: stored owner_secret_key_hex derives pubkey \
             0x{derived}, but the pool entry for PrivateNote {} records owner_public_key_hex \
             0x{recorded}. That note would later fail owner-signed writes with ERR_INVALID_SENDER 101 \
             because --note-key does not match the note owner. Deploy into a fresh --pool <new_file> or use \
             the correct pool/key material.",
            dexdo_core::address::display(note_addr)
        );
    }
    Ok(derived)
}

pub(crate) fn ensure_onchain_owner_matches_pool_key(
    role: &str,
    note_addr: &str,
    onchain_owner_pubkey: Option<&str>,
    derived_owner_pubkey: &str,
) -> Result<()> {
    let derived = normalize_owner_pubkey_hex(derived_owner_pubkey, "derived owner pubkey")?;
    let Some(onchain_raw) = onchain_owner_pubkey else {
        bail!(
            "{role} aborted before writing DEXDO_PN_POOL: PrivateNote {} getDetails exposes no \
             ephemeralPubkey. Refusing to leave a pool entry that may fail later with ERR_INVALID_SENDER 101. \
             Deploy a fresh note with --pool <new_file> after verifying the chain's contracts.",
            dexdo_core::address::display(note_addr)
        );
    };
    let onchain =
        normalize_owner_pubkey_hex(onchain_raw, "PrivateNote.getDetails().ephemeralPubkey")?;
    if onchain != derived {
        bail!(
            "{role} aborted before writing DEXDO_PN_POOL: PrivateNote {} on-chain owner key \
             _ephemeralPubkey 0x{onchain} does not match the stored owner_secret_key_hex-derived pubkey \
             0x{derived}. The --note-key would not match this note's owner and provision/sell/withdraw would \
             fail with ERR_INVALID_SENDER 101. Deploy a fresh note with --pool <new_file> and do not reuse the \
             stale/mismatched pool.",
            dexdo_core::address::display(note_addr)
        );
    }
    Ok(())
}

/// crash-safe state for wallet-funded note deploy. This file carries the randomly generated note owner
/// secret and is written before any wallet spend. Later deploy steps add the on-chain note identifiers so
/// `dexdo note recover` can finalize the pool without repeating an already completed deploy.
#[derive(Serialize, Deserialize)]
pub(crate) struct NoteDeployRecoveryState {
    pub version: u32,
    pub endpoint: String,
    pub nominal: String,
    pub token_type: u32,
    pub raw_value: u64,
    #[serde(serialize_with = "dexdo_core::address::serde_self_dapp::serialize")]
    pub funding_multisig_address: String,
    pub owner_public_key_hex: String,
    pub owner_secret_key_hex: Zeroizing<String>,
    #[serde(with = "dexdo_core::address::serde_canonical_opt")]
    pub pn_address: Option<String>,
    pub deposit_identifier_hash: Option<String>,
    pub deployed_at_unix: Option<u64>,
    #[serde(default)]
    pub deposit_voucher: Option<NoteDeployVoucherCheckpoint>,
    pub shell_funded: bool,
    pub sanity_checked: bool,
}

impl std::fmt::Debug for NoteDeployRecoveryState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoteDeployRecoveryState")
            .field("owner_public_key_hex", &self.owner_public_key_hex)
            .field("owner_secret_key_hex", &"<redacted>")
            .field("pn_address", &self.pn_address)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct NoteDeployRecoveryRequest<'a> {
    pub endpoint: &'a str,
    pub nominal: &'a str,
    pub token_type: u32,
    pub raw_value: u64,
    pub funding_multisig_address: &'a str,
}

/// Normalize persisted funding provenance without inventing a DApp for legacy data. Canonical
/// input retains both halves; account-only input remains account-only so old recovery/pool files
/// stay honest about the identity information they actually contain.
pub(crate) fn normalize_funding_multisig_identity(value: &str) -> Result<String> {
    let value = value.trim();
    let address = dexdo_core::CanonicalAddress::parse(value).map_err(|error| anyhow!("{error}"))?;
    if value.starts_with("0:") {
        Ok(address.legacy())
    } else {
        Ok(address.to_string())
    }
}

/// Canonical identities compare both halves. A legacy identity has no DApp half, so compatibility
/// can prove only that the account component matches; it must neither reject the old file nor
/// silently substitute a DApp into it.
fn funding_multisig_identities_match(left: &str, right: &str) -> bool {
    let Ok(left_address) = dexdo_core::CanonicalAddress::parse(left) else {
        return false;
    };
    let Ok(right_address) = dexdo_core::CanonicalAddress::parse(right) else {
        return false;
    };
    left_address.account_id() == right_address.account_id()
        && (left.trim().starts_with("0:")
            || right.trim().starts_with("0:")
            || left_address.dapp_id() == right_address.dapp_id())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoteDeployVoucherKind {
    Deposit,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct NoteDeployVoucherCheckpoint {
    pub sk_u_hex: Zeroizing<String>,
    pub sk_u_commit_hex: String,
    pub recipient_ephemeral_pubkey_hex: String,
    pub token_type: u32,
    pub raw_value: u64,
    pub is_fee: bool,
    #[serde(default)]
    pub submit_maybe_sent: bool,
    #[serde(default)]
    pub event: Option<NoteDeployVoucherEvent>,
    #[serde(default)]
    pub proof: Option<NoteDeployVoucherProof>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_rejected_proof_layer: Option<u8>,
}

impl std::fmt::Debug for NoteDeployVoucherCheckpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoteDeployVoucherCheckpoint")
            .field("sk_u_hex", &"<redacted>")
            .field("sk_u_commit_hex", &self.sk_u_commit_hex)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct NoteDeployVoucherEvent {
    pub id: String,
    pub boc: String,
    pub body: String,
    #[serde(with = "dexdo_core::address::serde_canonical")]
    pub dst: String,
    pub created_at: u64,
    pub block_id: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct NoteDeployVoucherProof {
    pub proof: String,
    pub deposit_identifier_hash_hex: String,
    pub final_layer_historical_hash_root_hex: String,
    pub voucher_nominal_fr_hex: String,
    pub token_type_fr_hex: String,
    pub ephemeral_pubkey_hex: String,
    pub voucher_value: u64,
    pub voucher_token_type: u32,
    pub layer_number: u8,
    pub sk_u_hex: Zeroizing<String>,
    pub sk_u_commit_hex: String,
}

impl std::fmt::Debug for NoteDeployVoucherProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoteDeployVoucherProof")
            .field("sk_u_hex", &"<redacted>")
            .field("sk_u_commit_hex", &self.sk_u_commit_hex)
            .finish_non_exhaustive()
    }
}

impl NoteDeployVoucherKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Deposit => "deposit",
        }
    }

    fn field_name(self) -> &'static str {
        match self {
            Self::Deposit => "deposit_voucher",
        }
    }
}

impl NoteDeployVoucherCheckpoint {
    pub(crate) fn new(
        recipient_ephemeral_pubkey_hex: &str,
        token_type: u32,
        raw_value: u64,
        is_fee: bool,
        sk_u_hex: String,
        sk_u_commit_hex: String,
    ) -> Result<Self> {
        let checkpoint = Self {
            sk_u_hex: normalize_secret_like_hex(&sk_u_hex, "sk_u_hex")?.into(),
            sk_u_commit_hex: normalize_secret_like_hex(&sk_u_commit_hex, "sk_u_commit_hex")?,
            recipient_ephemeral_pubkey_hex: normalize_secret_like_hex(
                recipient_ephemeral_pubkey_hex,
                "recipient_ephemeral_pubkey_hex",
            )?,
            token_type,
            raw_value,
            is_fee,
            submit_maybe_sent: false,
            event: None,
            proof: None,
            last_rejected_proof_layer: None,
        };
        checkpoint.validate("voucher checkpoint")?;
        Ok(checkpoint)
    }

    /// The `msg.currencies` map the funding wallet attaches to `RootPN.generateVoucher` for this
    /// checkpoint. `raw_value` stays the NOMINAL everywhere else in the checkpoint -- the wire figure
    /// exists only here, where the message is built.
    pub(crate) fn voucher_currency_map(&self) -> serde_json::Map<String, Value> {
        let mut cc = serde_json::Map::new();
        cc.insert(
            self.token_type.to_string(),
            Value::String(note_deploy_voucher_wire_raw(self.is_fee, self.raw_value).to_string()),
        );
        cc
    }

    pub(crate) fn validate(&self, label: &str) -> Result<()> {
        normalize_secret_like_hex(&self.sk_u_hex, "sk_u_hex")
            .map_err(|e| anyhow!("{label}: {e}"))?;
        normalize_secret_like_hex(&self.sk_u_commit_hex, "sk_u_commit_hex")
            .map_err(|e| anyhow!("{label}: {e}"))?;
        normalize_secret_like_hex(
            &self.recipient_ephemeral_pubkey_hex,
            "recipient_ephemeral_pubkey_hex",
        )
        .map_err(|e| anyhow!("{label}: {e}"))?;
        if self.raw_value == 0 {
            bail!("{label}: raw_value must be positive");
        }
        if !self.submit_maybe_sent && (self.event.is_some() || self.proof.is_some()) {
            bail!("{label}: event/proof cannot exist before voucher submit is marked uncertain");
        }
        if let Some(event) = &self.event {
            event.validate(label)?;
        }
        if let Some(proof) = &self.proof {
            proof.validate(label)?;
            if proof.sk_u_hex != self.sk_u_hex {
                bail!("{label}: proof sk_u_hex does not match checkpoint");
            }
            if proof.sk_u_commit_hex != self.sk_u_commit_hex {
                bail!("{label}: proof sk_u_commit_hex does not match checkpoint");
            }
            if proof.ephemeral_pubkey_hex != self.recipient_ephemeral_pubkey_hex {
                bail!("{label}: proof ephemeral_pubkey_hex does not match checkpoint");
            }
            if proof.voucher_value != self.raw_value {
                bail!("{label}: proof voucher_value does not match checkpoint");
            }
            if proof.voucher_token_type != self.token_type {
                bail!("{label}: proof voucher_token_type does not match checkpoint");
            }
        }
        if let Some(last_rejected) = self.last_rejected_proof_layer {
            if !(1..=NOTE_DEPLOY_PROOF_LAYER_MAX).contains(&last_rejected) {
                bail!(
                    "{label}: rejected proof layer is outside canonical plan 1..={NOTE_DEPLOY_PROOF_LAYER_MAX}"
                );
            }
            let proof = self
                .proof
                .as_ref()
                .ok_or_else(|| anyhow!("{label}: rejected layer has no persisted proof"))?;
            if !(1..=NOTE_DEPLOY_PROOF_LAYER_MAX).contains(&proof.layer_number) {
                bail!(
                    "{label}: current proof layer {} is outside canonical plan 1..={NOTE_DEPLOY_PROOF_LAYER_MAX}",
                    proof.layer_number,
                );
            }
            if proof.layer_number != last_rejected && proof.layer_number != last_rejected + 1 {
                bail!(
                    "{label}: current proof layer {} must be rejected layer {last_rejected} or its \
                     immediate successor {}",
                    proof.layer_number,
                    last_rejected + 1
                );
            }
        }
        Ok(())
    }

    pub(crate) fn current_proof_is_rejected(&self) -> bool {
        self.proof
            .as_ref()
            .is_some_and(|proof| self.last_rejected_proof_layer == Some(proof.layer_number))
    }

    pub(crate) fn reject_current_proof(&mut self) -> Result<u8> {
        let layer = self
            .proof
            .as_ref()
            .ok_or_else(|| anyhow!("voucher checkpoint has no proof to reject"))?
            .layer_number;
        if let Some(previous) = self.last_rejected_proof_layer {
            if layer != previous && layer != previous.saturating_add(1) {
                bail!(
                    "rejected proof layer {layer} is not monotonic after rejected layer {previous}"
                );
            }
        }
        self.last_rejected_proof_layer = Some(layer);
        self.validate("voucher checkpoint after exact 403")?;
        Ok(layer)
    }

    pub(crate) fn next_sdk_proof_layer(&self) -> Option<u32> {
        let last_rejected = self.last_rejected_proof_layer.unwrap_or_default();
        (last_rejected < NOTE_DEPLOY_PROOF_LAYER_MAX).then(|| u32::from(last_rejected))
    }

    pub(crate) fn replace_rejected_proof(
        &mut self,
        replacement: NoteDeployVoucherProof,
    ) -> Result<()> {
        let rejected = self
            .proof
            .as_ref()
            .filter(|_| self.current_proof_is_rejected())
            .ok_or_else(|| anyhow!("voucher checkpoint has no rejected proof to replace"))?;
        if replacement.deposit_identifier_hash_hex != rejected.deposit_identifier_hash_hex
            || replacement.voucher_nominal_fr_hex != rejected.voucher_nominal_fr_hex
            || replacement.token_type_fr_hex != rejected.token_type_fr_hex
            || replacement.ephemeral_pubkey_hex != rejected.ephemeral_pubkey_hex
            || replacement.voucher_value != rejected.voucher_value
            || replacement.voucher_token_type != rejected.voucher_token_type
            || replacement.sk_u_hex != rejected.sk_u_hex
            || replacement.sk_u_commit_hex != rejected.sk_u_commit_hex
        {
            bail!("replacement proof changed paid voucher identity");
        }
        if replacement.layer_number != rejected.layer_number.saturating_add(1) {
            bail!(
                "replacement proof layer {} is not the next layer after rejected layer {}",
                replacement.layer_number,
                rejected.layer_number
            );
        }
        self.proof = Some(replacement);
        self.validate("voucher checkpoint with replacement proof")
    }

    pub(crate) fn ensure_matches(
        &self,
        kind: NoteDeployVoucherKind,
        recipient_ephemeral_pubkey_hex: &str,
        token_type: u32,
        raw_value: u64,
        is_fee: bool,
    ) -> Result<()> {
        self.validate(kind.field_name())?;
        let recipient_ephemeral_pubkey_hex = normalize_secret_like_hex(
            recipient_ephemeral_pubkey_hex,
            "recipient_ephemeral_pubkey_hex",
        )?;
        if self.recipient_ephemeral_pubkey_hex != recipient_ephemeral_pubkey_hex
            || self.token_type != token_type
            || self.raw_value != raw_value
            || self.is_fee != is_fee
        {
            bail!(
                "note deploy recovery {} does not match this {} voucher request; refusing to mix \
                 voucher recovery state with a different owner/value/token/isFee.",
                kind.field_name(),
                kind.label()
            );
        }
        Ok(())
    }
}

impl NoteDeployVoucherEvent {
    pub(crate) fn validate(&self, label: &str) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("{label}: VoucherGenerated event id is empty");
        }
        if self.boc.trim().is_empty() {
            bail!("{label}: VoucherGenerated event boc is empty");
        }
        if self.body.trim().is_empty() {
            bail!("{label}: VoucherGenerated event body is empty");
        }
        if self.dst.trim().is_empty() {
            bail!("{label}: VoucherGenerated event dst is empty");
        }
        Ok(())
    }
}

impl NoteDeployVoucherEvent {
    pub(crate) fn from_sdk(
        event: dexdo_core::private_note::voucher_event::VoucherExtoutMessage,
    ) -> Self {
        Self {
            id: event.id,
            boc: event.boc,
            body: event.body,
            dst: event.dst,
            created_at: event.created_at,
            block_id: event.block_id,
        }
    }

    pub(crate) fn to_sdk(&self) -> dexdo_core::private_note::voucher_event::VoucherExtoutMessage {
        dexdo_core::private_note::voucher_event::VoucherExtoutMessage {
            id: self.id.clone(),
            boc: self.boc.clone(),
            body: self.body.clone(),
            dst: self.dst.clone(),
            created_at: self.created_at,
            block_id: self.block_id.clone(),
        }
    }
}

impl NoteDeployVoucherProof {
    pub(crate) fn validate(&self, label: &str) -> Result<()> {
        if self.proof.trim().is_empty() {
            bail!("{label}: halo2 proof is empty");
        }
        validate_hex_u256(
            &self.deposit_identifier_hash_hex,
            "deposit_identifier_hash_hex",
        )
        .map_err(|e| anyhow!("{label}: {e}"))?;
        validate_hex_u256(
            &self.final_layer_historical_hash_root_hex,
            "final_layer_historical_hash_root_hex",
        )
        .map_err(|e| anyhow!("{label}: {e}"))?;
        validate_hex_u256(&self.voucher_nominal_fr_hex, "voucher_nominal_fr_hex")
            .map_err(|e| anyhow!("{label}: {e}"))?;
        validate_hex_u256(&self.token_type_fr_hex, "token_type_fr_hex")
            .map_err(|e| anyhow!("{label}: {e}"))?;
        normalize_secret_like_hex(&self.ephemeral_pubkey_hex, "ephemeral_pubkey_hex")
            .map_err(|e| anyhow!("{label}: {e}"))?;
        normalize_secret_like_hex(&self.sk_u_hex, "sk_u_hex")
            .map_err(|e| anyhow!("{label}: {e}"))?;
        normalize_secret_like_hex(&self.sk_u_commit_hex, "sk_u_commit_hex")
            .map_err(|e| anyhow!("{label}: {e}"))?;
        if self.voucher_value == 0 {
            bail!("{label}: voucher_value must be positive");
        }
        if self.layer_number == 0 {
            bail!("{label}: layer_number must be positive");
        }
        Ok(())
    }
}

impl NoteDeployVoucherProof {
    pub(crate) fn from_halo2(proof: &dexdo_core::private_note::halo2::live::Halo2Proof) -> Self {
        Self {
            proof: proof.proof.clone(),
            deposit_identifier_hash_hex: proof.deposit_identifier_hash_hex.clone(),
            final_layer_historical_hash_root_hex: proof
                .final_layer_historical_hash_root_hex
                .clone(),
            voucher_nominal_fr_hex: proof.voucher_nominal_fr_hex.clone(),
            token_type_fr_hex: proof.token_type_fr_hex.clone(),
            ephemeral_pubkey_hex: proof.ephemeral_pubkey_hex.clone(),
            voucher_value: proof.voucher_value,
            voucher_token_type: proof.voucher_token_type,
            layer_number: proof.layer_number,
            sk_u_hex: proof.sk_u_hex.clone().into(),
            sk_u_commit_hex: proof.sk_u_commit_hex.clone(),
        }
    }

    pub(crate) fn to_halo2(&self) -> dexdo_core::private_note::halo2::live::Halo2Proof {
        dexdo_core::private_note::halo2::live::Halo2Proof {
            proof: self.proof.clone(),
            deposit_identifier_hash_hex: self.deposit_identifier_hash_hex.clone(),
            final_layer_historical_hash_root_hex: self.final_layer_historical_hash_root_hex.clone(),
            voucher_nominal_fr_hex: self.voucher_nominal_fr_hex.clone(),
            token_type_fr_hex: self.token_type_fr_hex.clone(),
            ephemeral_pubkey_hex: self.ephemeral_pubkey_hex.clone(),
            voucher_value: self.voucher_value,
            voucher_token_type: self.voucher_token_type,
            layer_number: self.layer_number,
            sk_u_hex: self.sk_u_hex.to_string(),
            sk_u_commit_hex: self.sk_u_commit_hex.clone(),
        }
    }
}

impl NoteDeployRecoveryState {
    pub(crate) fn new(
        request: NoteDeployRecoveryRequest<'_>,
        owner_public_key_hex: &str,
        owner_secret_key_hex: &str,
    ) -> Result<Self> {
        let funding_multisig_address =
            normalize_funding_multisig_identity(request.funding_multisig_address)?;
        let owner_public_key_hex =
            normalize_owner_pubkey_hex(owner_public_key_hex, "owner_public_key_hex")?;
        let owner_secret_key_hex = normalize_secret_hex(owner_secret_key_hex)?;
        let state = Self {
            version: NOTE_DEPLOY_RECOVERY_VERSION,
            endpoint: request.endpoint.to_string(),
            nominal: request.nominal.to_string(),
            token_type: request.token_type,
            raw_value: request.raw_value,
            funding_multisig_address,
            owner_public_key_hex,
            owner_secret_key_hex: owner_secret_key_hex.into(),
            pn_address: None,
            deposit_identifier_hash: None,
            deployed_at_unix: None,
            deposit_voucher: None,
            shell_funded: false,
            sanity_checked: false,
        };
        state.validate()?;
        Ok(state)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.version != NOTE_DEPLOY_RECOVERY_VERSION {
            bail!(
                "note deploy recovery file version {} is unsupported; expected {}",
                self.version,
                NOTE_DEPLOY_RECOVERY_VERSION
            );
        }
        if self.endpoint.trim().is_empty() {
            bail!("note deploy recovery file has empty endpoint");
        }
        if self.nominal.trim().is_empty() {
            bail!("note deploy recovery file has empty nominal");
        }
        ensure_shell_currency_id(self.token_type, "note deploy recovery file")?;
        let normalized_wallet = normalize_funding_multisig_identity(&self.funding_multisig_address)
            .map_err(|e| anyhow!("note deploy recovery funding_multisig_address: {e}"))?;
        if normalized_wallet != self.funding_multisig_address {
            bail!(
                "note deploy recovery funding_multisig_address must be normalized as {normalized_wallet}"
            );
        }
        ensure_pool_note_keypair_matches(
            self.pn_address.as_deref().unwrap_or("pending"),
            &self.owner_public_key_hex,
            &self.owner_secret_key_hex,
        )?;
        if self.pn_address.is_some() && self.deposit_identifier_hash.is_none() {
            bail!(
                "note deploy recovery file has pn_address but no deposit_identifier_hash; refusing to guess"
            );
        }
        if let Some(voucher) = &self.deposit_voucher {
            voucher.ensure_matches(
                NoteDeployVoucherKind::Deposit,
                &self.owner_public_key_hex,
                self.token_type,
                self.raw_value,
                false,
            )?;
        }
        Ok(())
    }

    pub(crate) fn ensure_matches_request(
        &self,
        request: NoteDeployRecoveryRequest<'_>,
    ) -> Result<()> {
        let funding_multisig_address =
            normalize_funding_multisig_identity(request.funding_multisig_address)?;
        if self.endpoint != request.endpoint
            || self.nominal != request.nominal
            || self.token_type != request.token_type
            || self.raw_value != request.raw_value
            || !funding_multisig_identities_match(
                &self.funding_multisig_address,
                &funding_multisig_address,
            )
        {
            bail!(
                "note deploy recovery file does not match this deploy request. Refusing to mix recovery state \
                 with a different wallet/endpoint/nominal/token-type; pass the matching --recovery file or \
                 deploy into a fresh --pool/--recovery pair."
            );
        }
        Ok(())
    }

    pub(crate) fn mark_private_note_deployed(
        &mut self,
        pn_address: String,
        deposit_identifier_hash: String,
        deployed_at_unix: u64,
    ) -> Result<()> {
        self.pn_address = Some(pn_address);
        self.deposit_identifier_hash = Some(deposit_identifier_hash);
        self.deployed_at_unix = Some(deployed_at_unix);
        self.validate()
    }

    pub(crate) fn voucher_checkpoint(
        &self,
        kind: NoteDeployVoucherKind,
    ) -> Option<&NoteDeployVoucherCheckpoint> {
        match kind {
            NoteDeployVoucherKind::Deposit => self.deposit_voucher.as_ref(),
        }
    }

    pub(crate) fn set_voucher_checkpoint(
        &mut self,
        kind: NoteDeployVoucherKind,
        checkpoint: NoteDeployVoucherCheckpoint,
    ) -> Result<()> {
        let (token_type, raw_value, is_fee) = match kind {
            NoteDeployVoucherKind::Deposit => (self.token_type, self.raw_value, false),
        };
        checkpoint.ensure_matches(
            kind,
            &self.owner_public_key_hex,
            token_type,
            raw_value,
            is_fee,
        )?;
        match kind {
            NoteDeployVoucherKind::Deposit => self.deposit_voucher = Some(checkpoint),
        }
        self.validate()
    }

    pub(crate) fn mark_shell_funded_and_checked(&mut self) -> Result<()> {
        self.shell_funded = true;
        self.sanity_checked = true;
        self.validate()
    }

    pub(crate) fn ensure_ready_for_pool(&self) -> Result<()> {
        if self.pn_address.is_none() || self.deposit_identifier_hash.is_none() {
            bail!(
                "note deploy recovery state contains the owner key but no deployed PrivateNote address yet; \
                 refusing to write a pool entry or guess. Re-run the original `dexdo note deploy` command \
                 unchanged -- it resumes from this file -- to continue with the persisted owner key."
            );
        }
        if !self.shell_funded || !self.sanity_checked {
            bail!(
                "note deploy recovery state is not finalized for pooling (shell_funded={}, sanity_checked={}); \
                 re-run the original `dexdo note deploy` command unchanged -- it resumes from this file -- \
                 before using `dexdo note recover`.",
                self.shell_funded,
                self.sanity_checked
            );
        }
        Ok(())
    }

    pub(crate) fn to_onboard_state(&self) -> Result<OnboardPnState> {
        self.validate()?;
        Ok(OnboardPnState {
            endpoint: self.endpoint.clone(),
            nominal: self.nominal.clone(),
            token_type: self.token_type,
            raw_value: self.raw_value,
            pn_address: self.pn_address.clone(),
            deposit_identifier_hash: self.deposit_identifier_hash.clone(),
            owner_public_key_hex: Some(self.owner_public_key_hex.clone()),
            owner_secret_key_hex: Some(self.owner_secret_key_hex.clone()),
            deployed_at_unix: self.deployed_at_unix,
            shell_funded: self.shell_funded,
            sanity_checked: self.sanity_checked,
        })
    }
}

pub(crate) fn default_note_deploy_recovery_path(pool: &Path) -> PathBuf {
    let mut path = pool.as_os_str().to_os_string();
    path.push(".recovery.json");
    PathBuf::from(path)
}

pub(crate) fn resolve_private_file_path(path: &Path, label: &str) -> Result<PathBuf> {
    let resolved = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let parent = std::fs::canonicalize(parent).map_err(|e| {
                anyhow!(
                    "resolve parent directory for {label} {}: {e}",
                    path.display()
                )
            })?;
            let name = path
                .file_name()
                .ok_or_else(|| anyhow!("{label} path {} has no file name", path.display()))?;
            parent.join(name)
        }
        Err(e) => bail!("resolve {label} {}: {e}", path.display()),
    };

    match std::fs::symlink_metadata(&resolved) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(resolved),
        Ok(_) => bail!("{label} {} must resolve to a regular file", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(resolved),
        Err(e) => bail!("inspect {label} {}: {e}", path.display()),
    }
}

pub(crate) fn load_note_deploy_recovery(path: &Path) -> Result<Option<NoteDeployRecoveryState>> {
    let path = resolve_private_file_path(path, "note deploy recovery")?;
    // the recovery file holds the note's `owner_secret_key_hex` -- it is written at 0600 by
    // `write_private_atomic` and was read back with no check that it still is. A recovery that has
    // not been written yet is the ordinary case and stays fine.
    crate::cli::support::refuse_exposed_secret_file_if_present(&path, "note deploy recovery")?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => bail!("read note deploy recovery {}: {e}", path.display()),
    };
    let state: NoteDeployRecoveryState = serde_json::from_slice(&bytes).map_err(|e| {
        anyhow!(
            "note deploy recovery {} is not valid JSON: {e}",
            path.display()
        )
    })?;
    state.validate()?;
    Ok(Some(state))
}

struct NoteDeployRecoveryWriteLock {
    path: PathBuf,
}

impl Drop for NoteDeployRecoveryWriteLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn acquire_note_deploy_recovery_write_lock(
    recovery_path: &Path,
) -> Result<NoteDeployRecoveryWriteLock> {
    use std::io::Write;

    let mut lock_name = recovery_path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&lock_path) {
        Ok(mut lock) => {
            if let Err(e) = writeln!(lock, "{}", std::process::id()) {
                let _ = std::fs::remove_file(&lock_path);
                return Err(anyhow!(
                    "write note deploy recovery lock {}: {e}",
                    lock_path.display()
                ));
            }
            Ok(NoteDeployRecoveryWriteLock { path: lock_path })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!(
            "note deploy recovery {} is being updated by another process; refusing a concurrent overwrite. \
             Retry after that deploy exits; remove lock {} only after confirming no note deploy is running.",
            recovery_path.display(),
            lock_path.display()
        ),
        Err(e) => bail!(
            "create note deploy recovery lock {}: {e}",
            lock_path.display()
        ),
    }
}

/// What the client tells an operator about a recovery file it refuses to touch.

/// Every refusal that leaves such a file where it is says this same sentence. It is stated once so
/// that a second site cannot drift into a second formula: `note deploy` grew a refusal to overwrite
/// an unreadable recovery long before it grew a refusal to delete one, and the two have to give the
/// operator the same instruction because they are protecting the same file for the same reason.
pub(crate) const NOTE_DEPLOY_RECOVERY_PRESERVE_INSTRUCTION: &str =
    "Preserve it and pass --recovery <different-file>.";

fn load_existing_note_deploy_recovery_for_write(
    path: &Path,
) -> Result<Option<NoteDeployRecoveryState>> {
    // same file, same secret, read on the write path to decide whether to overwrite.
    crate::cli::support::refuse_exposed_secret_file_if_present(path, "note deploy recovery")?;
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => bail!("read existing note deploy recovery {}: {e}", path.display()),
    };
    let state: NoteDeployRecoveryState = serde_json::from_slice(&bytes).map_err(|e| {
        anyhow!(
            "existing note deploy recovery {} is invalid JSON; refusing to overwrite it: {e}. {}",
            path.display(),
            NOTE_DEPLOY_RECOVERY_PRESERVE_INSTRUCTION
        )
    })?;
    state.validate().map_err(|e| {
        anyhow!(
            "existing note deploy recovery {} is invalid; refusing to overwrite it: {e}. {}",
            path.display(),
            NOTE_DEPLOY_RECOVERY_PRESERVE_INSTRUCTION
        )
    })?;
    Ok(Some(state))
}

fn same_note_deploy_owner(left: &NoteDeployRecoveryState, right: &NoteDeployRecoveryState) -> bool {
    left.owner_public_key_hex == right.owner_public_key_hex
        && left.owner_secret_key_hex == right.owner_secret_key_hex
}

fn note_deploy_request_fields_match(
    left: &NoteDeployRecoveryState,
    right: &NoteDeployRecoveryState,
) -> bool {
    left.endpoint == right.endpoint
        && left.nominal == right.nominal
        && left.token_type == right.token_type
        && left.raw_value == right.raw_value
        && funding_multisig_identities_match(
            &left.funding_multisig_address,
            &right.funding_multisig_address,
        )
}

pub(crate) fn note_deploy_recovery_has_no_possible_spend(state: &NoteDeployRecoveryState) -> bool {
    fn voucher_has_no_possible_spend(voucher: Option<&NoteDeployVoucherCheckpoint>) -> bool {
        voucher.is_none_or(|voucher| {
            !voucher.submit_maybe_sent && voucher.event.is_none() && voucher.proof.is_none()
        })
    }

    state.pn_address.is_none()
        && state.deposit_identifier_hash.is_none()
        && state.deployed_at_unix.is_none()
        && !state.shell_funded
        && !state.sanity_checked
        && voucher_has_no_possible_spend(state.deposit_voucher.as_ref())
}

fn ensure_same_recovery_can_advance(
    path: &Path,
    existing: &NoteDeployRecoveryState,
    next: &NoteDeployRecoveryState,
) -> Result<()> {
    if !same_note_deploy_owner(existing, next) || !note_deploy_request_fields_match(existing, next)
    {
        bail!(
            "note deploy recovery {} belongs to a different deploy owner or request; refusing to overwrite it. \
             Resume the existing state or pass --recovery <different-file>.",
            path.display()
        );
    }
    if existing
        .pn_address
        .as_ref()
        .is_some_and(|address| next.pn_address.as_ref() != Some(address))
        || existing
            .deposit_identifier_hash
            .as_ref()
            .is_some_and(|hash| next.deposit_identifier_hash.as_ref() != Some(hash))
    {
        bail!(
            "note deploy recovery {} already holds a different deployed PrivateNote identity; refusing to \
             clobber its recovery key. Pass --recovery <different-file>.",
            path.display()
        );
    }
    Ok(())
}

fn write_note_deploy_recovery_locked(path: &Path, state: &NoteDeployRecoveryState) -> Result<()> {
    let json = serde_json::to_vec_pretty(state)?;
    write_private_atomic(path, &json)
        .map_err(|e| anyhow!("write note deploy recovery {}: {e}", path.display()))
}

pub(crate) fn write_note_deploy_recovery(
    path: &Path,
    state: &NoteDeployRecoveryState,
) -> Result<()> {
    state.validate()?;
    let path = resolve_private_file_path(path, "note deploy recovery")?;
    let _lock = acquire_note_deploy_recovery_write_lock(&path)?;
    if let Some(existing) = load_existing_note_deploy_recovery_for_write(&path)? {
        ensure_same_recovery_can_advance(&path, &existing, state)?;
    }
    write_note_deploy_recovery_locked(&path, state)
}

/// Refresh the recovery file only after the deployed note's on-chain owner was validated.
/// A different recorded note or a different owner's possibly submitted spend is never overwritten.
pub(crate) fn refresh_note_deploy_recovery_after_success(
    path: &Path,
    state: &NoteDeployRecoveryState,
) -> Result<()> {
    state.validate()?;
    state.ensure_ready_for_pool()?;
    let path = resolve_private_file_path(path, "note deploy recovery")?;
    let _lock = acquire_note_deploy_recovery_write_lock(&path)?;
    if let Some(existing) = load_existing_note_deploy_recovery_for_write(&path)? {
        if same_note_deploy_owner(&existing, state) {
            ensure_same_recovery_can_advance(&path, &existing, state)?;
        } else if let Some(existing_note) = existing.pn_address.as_deref() {
            if state.pn_address.as_deref() != Some(existing_note) {
                bail!(
                    "note deploy recovery {} already holds recovery for different deployed PrivateNote \
                     {existing_note}; refusing to clobber its only recovery key. Keep this file and pass \
                     --recovery <different-file> for the successful deploy.",
                    path.display()
                );
            }
        } else if !note_deploy_recovery_has_no_possible_spend(&existing) {
            bail!(
                "note deploy recovery {} holds possible wallet-spend recovery material for a different owner; \
                 refusing to clobber it. Resume that attempt with this file, or pass \
                 --recovery <different-file> for the successful deploy.",
                path.display()
            );
        }
    }
    write_note_deploy_recovery_locked(&path, state)
}

pub(crate) fn ensure_recovery_owner_matches_target_note(
    path: &Path,
    state: &NoteDeployRecoveryState,
    onchain_owner_pubkey: Option<&str>,
) -> Result<()> {
    state.validate()?;
    let note_addr = state.pn_address.as_deref().ok_or_else(|| {
        anyhow!(
            "note recovery {} has no target PrivateNote address; refusing to guess",
            path.display()
        )
    })?;
    let derived_owner = derive_owner_pubkey_from_secret_hex(&state.owner_secret_key_hex)?;
    ensure_onchain_owner_matches_pool_key(
        "note recover",
        note_addr,
        onchain_owner_pubkey,
        &derived_owner,
    )
    .map_err(|e| {
        anyhow!(
            "{e} Recovery file {} was left unchanged because its owner key does not own target PrivateNote \
             {}; pass the recovery file that belongs to this note.",
            path.display(),
            dexdo_core::address::display(note_addr)
        )
    })
}

pub(crate) fn recovery_owner_key_written_message(path: &Path) -> String {
    // Printed in two states -- a recovery file this run just created, and one an earlier run left
    // that this run loaded and matched -- so it has to be true in both. It therefore says
    // "re-run your command unchanged" rather than "add `--recovery`": this file is already the
    // run's recovery path, whether it came from the flag or from the default beside `--pool`, and
    // an operator who pasted the flag a second time would be rejected by clap for a duplicate.
    // Nothing here is a command line with arguments, because a resume must reuse the funding
    // wallet, key source and `--nominal` this message does not know, and because a bare
    // `--pool <pool>` is a shell redirection rather than a value. The pool file is named by role,
    // not as a hardcoded `pn_pool.json`, since `--pool` is arbitrary.
    format!(
        "note deploy recovery: owner key persisted to {} (0600) before wallet spend. If interrupted before \
         recovery is finalized, re-run this same `dexdo note deploy` command unchanged -- same funding \
         wallet, same `--nominal`, same `--pool`, and the same `--recovery` path if you passed one: it \
         resumes from this file instead of spending again. If recovery is already finalized but the pool \
         file was never written, finalize it with `dexdo note recover`, passing this file to `--recovery` \
         and that pool path to `--pool`.",
        path.display()
    )
}

/// The one `note recover` line the CLI prints complete, because it is the one place both paths are
/// known. Values are shell-quoted: a recovery or pool path containing a space would
/// otherwise reach the parser as two arguments, after the operator's shell had already split it.

/// Its only caller is the chain `note deploy` path, so it exists exactly where that does --
/// the same boundary the settlement builders use -- rather than shipping behind a suppression.
pub(crate) fn note_recover_finalize_command(recovery: &Path, pool: &Path) -> String {
    use crate::cli::support::shell_arg;
    format!(
        "dexdo note recover --recovery {} --pool {}",
        shell_arg(&recovery.display().to_string()),
        shell_arg(&pool.display().to_string())
    )
}

pub(crate) fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let path = resolve_private_file_path(path, "secret file")?;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("secret.json");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| anyhow!("system clock before epoch: {e}"))?
        .as_nanos();
    let tmp = dir.join(format!(".{name}.tmp.{}.{nanos}", std::process::id()));
    write_private_atomic_via_temp(&path, &tmp, bytes)
}

pub(crate) fn write_private_atomic_via_temp(path: &Path, tmp: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows_sys::Win32::Foundation::GENERIC_WRITE;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, READ_CONTROL, WRITE_DAC};
        opts.access_mode(GENERIC_WRITE | READ_CONTROL | WRITE_DAC);
        opts.share_mode(FILE_SHARE_DELETE);
    }
    let mut f = opts
        .open(tmp)
        .map_err(|e| anyhow!("create temp secret file {}: {e}", tmp.display()))?;
    #[cfg(windows)]
    if let Err(error) = crate::cli::windows_secret_file::protect_owner_only(&f, tmp) {
        drop(f);
        return match std::fs::remove_file(tmp) {
            Ok(()) => Err(error),
            Err(cleanup_error) => Err(anyhow!(
                "{error}; remove empty temp secret file {} after ACL failure: {cleanup_error}",
                tmp.display()
            )),
        };
    }
    if let Err(e) = f.write_all(bytes).and_then(|()| f.sync_all()) {
        let _ = std::fs::remove_file(tmp);
        return Err(anyhow!("write temp secret file {}: {e}", tmp.display()));
    }
    std::fs::rename(tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(tmp);
        anyhow!("rename temp secret file into {}: {e}", path.display())
    })?;
    sync_parent_dir(path)?;
    Ok(())
}

pub(crate) fn sync_parent_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::File::open(dir)
            .and_then(|f| f.sync_all())
            .map_err(|e| anyhow!("fsync parent directory {}: {e}", dir.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn normalize_secret_hex(secret_hex: &str) -> Result<String> {
    let secret = secret_hex
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    let bytes = hex::decode(secret)
        .map_err(|e| anyhow!("owner_secret_key_hex must be 32-byte hex: {e}"))?;
    if bytes.len() != 32 {
        bail!("owner_secret_key_hex must be exactly 32 bytes");
    }
    Ok(secret.to_ascii_lowercase())
}

fn normalize_secret_like_hex(raw: &str, label: &str) -> Result<String> {
    let value = raw.trim().trim_start_matches("0x").trim_start_matches("0X");
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{label} must be a 32-byte hex value");
    }
    Ok(value.to_ascii_lowercase())
}

fn validate_hex_u256(raw: &str, label: &str) -> Result<()> {
    normalize_secret_like_hex(raw, label).map(|_| ())
}

/// CLI-compatible note deploy state. A subset of its fields -- exactly those the pool needs. **Carries the owner
/// secret key** -- never log it.
#[derive(Serialize, Deserialize)]
pub(crate) struct OnboardPnState {
    pub endpoint: String,
    pub nominal: String,
    pub token_type: u32,
    pub raw_value: u64,
    #[serde(with = "dexdo_core::address::serde_canonical_opt")]
    pub pn_address: Option<String>,
    pub deposit_identifier_hash: Option<String>,
    pub owner_public_key_hex: Option<String>,
    pub owner_secret_key_hex: Option<Zeroizing<String>>,
    pub deployed_at_unix: Option<u64>,
    pub shell_funded: bool,
    pub sanity_checked: bool,
}

impl std::fmt::Debug for OnboardPnState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnboardPnState")
            .field("endpoint", &self.endpoint)
            .field("pn_address", &self.pn_address)
            .field("owner_public_key_hex", &self.owner_public_key_hex)
            .field("owner_secret_key_hex", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl From<dexdo_core::private_note::DeployPrivateNoteResult> for OnboardPnState {
    fn from(s: dexdo_core::private_note::DeployPrivateNoteResult) -> Self {
        Self {
            endpoint: s.endpoint,
            nominal: s.nominal,
            token_type: s.token_type,
            raw_value: s.raw_value,
            pn_address: Some(s.pn_address),
            deposit_identifier_hash: Some(s.deposit_identifier_hash),
            owner_public_key_hex: Some(s.owner_public_key_hex),
            owner_secret_key_hex: Some(s.owner_secret_key_hex.into()),
            deployed_at_unix: Some(s.deployed_at_unix),
            shell_funded: s.shell_funded,
            sanity_checked: s.sanity_checked,
        }
    }
}

/// The one spelling a note address is RECORDED in: canonical `<dapp_id>::<account_id>`.

/// The pool is a file the operator reads, and it was holding two conventions side by side -- a
/// canonical `funding_multisig_address` next to notes spelled `0:<account_id>`, which names no DApp
/// at all. Downstream that legacy spelling is what makes `note list` print `<account>::<account>`:
/// [`dexdo_core::address::display_self_dapp`] reconstructs a SELF-DApp identity for anything that
/// arrives without one, and a `PrivateNote` is not a self-DApp account -- it lives in
/// [`dexdo_core::DEXDO_DAPP_ID`] (`crates/core/src/address.rs`), which is how `dexdo history` prints
/// the same note. One note had three spellings across one client.

/// Writing only. Reading stays tolerant of both forms, so pools written by earlier releases keep
/// working: every matcher here normalizes before it compares.

/// A supplied DApp id is authoritative and survives -- this upgrades the legacy form, it does not
/// re-scope an address that already names its DApp.

/// A value that is not an address at all is written unchanged, exactly as
/// [`dexdo_core::address::display`] passes one through: this is a rendering choice and it must not
/// become a new refusal on a path whose job is to record what a deploy just produced. Whether such
/// a value may reach the pool at all is a separate question, and answering it here would hide it.
pub(crate) fn pool_note_address_as_recorded(address: &str) -> String {
    dexdo_core::address::display(address)
}

/// output adapter: build a single DEXDO_PN_POOL **note** object from a fully deployed note state. Fails
/// loud if deploy did not complete (missing `pn_address`/keys, or not `shell_funded`/`sanity_checked`) -- folding a
/// half-deployed note into the pool would later strand the `seller`/`buyer` on an unusable note.
pub(crate) fn pn_state_to_pool_note(s: &OnboardPnState) -> Result<Value> {
    ensure_shell_currency_id(s.token_type, "note deploy state")?;
    let address = s.pn_address.as_deref().ok_or_else(|| {
        anyhow!("pn_state has no pn_address -- note deploy did not reach deployPrivateNote (step 1)")
    })?;
    let dih = s.deposit_identifier_hash.as_deref().ok_or_else(|| {
        anyhow!("pn_state has no deposit_identifier_hash -- incomplete note deploy")
    })?;
    let pubkey = s
        .owner_public_key_hex
        .as_deref()
        .ok_or_else(|| anyhow!("pn_state has no owner_public_key_hex -- incomplete note deploy"))?;
    let seckey = s
        .owner_secret_key_hex
        .as_deref()
        .ok_or_else(|| anyhow!("pn_state has no owner_secret_key_hex -- incomplete note deploy"))?;
    ensure_pool_note_keypair_matches(address, pubkey, seckey)?;
    if !s.shell_funded || !s.sanity_checked {
        bail!(
            "note deploy state not fully deployed (shell_funded={}, sanity_checked={}) -- the PN has no gas / failed its \
             getDetails check; re-run `dexdo note deploy` (idempotent at the step boundary) before pooling it.",
            s.shell_funded,
            s.sanity_checked
        );
    }
    Ok(json!({
        "address": pool_note_address_as_recorded(address),
        "deposit_identifier_hash": dih,
        "owner_public_key_hex": pubkey,
        "owner_secret_key_hex": seckey,
        "deployed_at_unix": s.deployed_at_unix.unwrap_or(0),
        "shell_funded": s.shell_funded,
        "native_funded": s.sanity_checked,
    }))
}

/// output adapter: append `note` to a `DEXDO_PN_POOL` JSON, creating the pool with the pool-level fields
/// from the deploy state (endpoint/nominal/token_type/raw_value/ecc) when it does not yet exist, or appending to an
/// existing matching pool. Refuses to mix nominals/token-types in one pool (the consumers assume a homogeneous
/// pool), and refuses to add a duplicate note `address`. Pure (takes the existing pool JSON, returns the new one).
pub(crate) fn pool_with_note_added(
    existing: Option<Value>,
    s: &OnboardPnState,
    note: Value,
    created_at_unix: u64,
    funding_multisig_address: &str,
) -> Result<Value> {
    ensure_shell_currency_id(s.token_type, "note deploy state")?;
    let funding_multisig_address = normalize_funding_multisig_identity(funding_multisig_address)?;
    // THE WALLET KEEPS THE FORM IT WAS GIVEN, and the note does not. That asymmetry is deliberate
    // and an earlier revision of this change got it wrong by making both canonical.

    // A note's DApp is knowable: `PrivateNote` lives in `DEXDO_DAPP_ID`, so writing it is recording
    // a fact. A multisig is a self-DApp account, so its canonical form is `<account>::<account>` --
    // which `display_self_dapp` RECONSTRUCTS from the account id rather than reads from anywhere.
    // Storing that in a provenance field would record a claim this client never verified on chain.

    // It also broke the identity comparison, which is what the field is for.
    // `funding_multisig_identities_match` compares by account alone whenever either side carries no
    // DApp; canonicalising on write removed that escape, so a pool created from `0:AAAA...` and a
    // later note deployed with `<dapp_id>::AAAA...` -- the SAME wallet -- compared as two different
    // multisigs and the run refused with "rewards provenance must not mix PrivateNotes funded by
    // different multisigs".
    let mut pool = match existing {
        Some(p) => p,
        None => json!({
            "endpoint": s.endpoint,
            "created_at_unix": created_at_unix,
            "nominal": s.nominal,
            "token_type": s.token_type,
            "raw_value_per_pn": s.raw_value,
            "funding_multisig_address": funding_multisig_address,
            "notes": [],
        }),
    };
    // Homogeneity: a pool is one nominal + token_type (the seller/buyer pick any note assuming uniform value).
    if pool["nominal"] != json!(s.nominal) || pool["token_type"] != json!(s.token_type) {
        bail!(
            "pool nominal/token_type ({}/{}) != this note's ({}/{}): the consumers assume a homogeneous pool -- \
             use a separate --pool file per nominal/token-type.",
            pool["nominal"],
            pool["token_type"],
            s.nominal,
            s.token_type
        );
    }
    match pool.get("funding_multisig_address").and_then(Value::as_str) {
        Some(existing) => {
            let existing = normalize_funding_multisig_identity(existing).map_err(|e| {
                anyhow!("--pool: malformed funding_multisig_address `{existing}`: {e}")
            })?;
            if !funding_multisig_identities_match(&existing, &funding_multisig_address) {
                bail!(
                    "pool funding_multisig_address {} != this note's {}: \
                     rewards provenance must not mix PrivateNotes funded by different multisigs. Use a separate \
                     --pool file for each funding multisig.",
                    dexdo_core::address::display_self_dapp(&existing),
                    dexdo_core::address::display_self_dapp(&funding_multisig_address)
                );
            }
            pool["funding_multisig_address"] = json!(existing);
        }
        None => {
            let has_existing_notes = pool["notes"]
                .as_array()
                .map(|notes| !notes.is_empty())
                .unwrap_or(false);
            if has_existing_notes {
                bail!(
                    "--pool has existing notes but no funding_multisig_address: refusing to attach new rewards \
                     provenance to older notes of unknown origin. Create a fresh --pool or migrate the old pool \
                     explicitly after verifying its funding multisig."
                );
            }
            pool["funding_multisig_address"] = json!(funding_multisig_address);
        }
    }
    let notes = pool["notes"]
        .as_array_mut()
        .ok_or_else(|| anyhow!("--pool: malformed (\"notes\" is not an array)"))?;
    let new_addr = note["address"].as_str().unwrap_or_default();
    if pool_notes_contain(notes, new_addr) {
        bail!(
            "note {} is already in the pool -- refusing to add a duplicate",
            dexdo_core::address::display(new_addr)
        );
    }
    notes.push(note);
    Ok(pool)
}

/// Is this note already recorded in these pool entries?

/// One implementation for both readers -- the refusal above, which is the last word before a pool
/// is written, and [`retire_a_finished_deploy`], which is the first word before a wallet is spent.
/// Two copies of an address comparison drift, and a drift here means one of them mints a duplicate
/// the other would have refused.

/// Raw string first, then the normalised form: a pool written by an older client may hold an
/// address in a different scoping, and that is the same note.
fn pool_notes_contain(notes: &[Value], address: &str) -> bool {
    let normalized = dexdo_core::normalize_wallet_address(address).ok();
    notes
        .iter()
        .filter_map(|note| note["address"].as_str())
        .any(|recorded| {
            recorded == address
                || normalized.as_ref().is_some_and(|address| {
                    dexdo_core::normalize_wallet_address(recorded).ok().as_ref() == Some(address)
                })
        })
}

/// What a loaded recovery file means for the run that loaded it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FinishedDeploy {
    /// The file describes work still to be done: resume it.
    Unfinished,
    /// The file describes a deploy that is finished AND recorded in the pool, and the pool holds
    /// the same owner key. It has been retired, and this run deploys a new note from scratch.
    Retired,
}

/// Retire a recovery file whose deploy is finished and safely recorded -- before anything is spent.

/// The file exists because the owner key of a note is created BEFORE the wallet spends on it: an
/// interruption in between would leave money on chain and no key to the note it bought. So the key
/// is written down first, and a rerun continues from it instead of spending twice.

/// Once the note is in the pool that reason is spent. The pool entry carries the same
/// `owner_secret_key_hex`, so the recovery file is a second copy, not the only one -- and keeping
/// it makes the next `note deploy` load a finished attempt, deploy nothing, and fail at the pool
/// write, after the operator has confirmed a Vault -> Hot transfer for a run that could never have
/// produced a note. Live, that cost a wallet confirmation, a wait, and a page of error.

/// Retiring is therefore the right answer, and the key comparison is what makes it safe: the file
/// goes only when the pool provably holds the same secret. A pool entry with a different key, or
/// none, means this file is still the only copy of something, and then the run refuses instead --
/// deleting it would destroy the only way to ever spend that note.
pub(crate) fn retire_a_finished_deploy(
    recovery_path: &Path,
    recovery: &NoteDeployRecoveryState,
    pool_path: &Path,
) -> Result<FinishedDeploy> {
    let Some(deployed) = recovery.pn_address.as_deref() else {
        // Nothing was deployed under this file yet: an interrupted attempt, which is exactly what
        // resuming is for.
        return Ok(FinishedDeploy::Unfinished);
    };
    let Ok(bytes) = std::fs::read(pool_path) else {
        // No pool, or one that cannot be read: the pool write itself will say so, with the path.
        return Ok(FinishedDeploy::Unfinished);
    };
    let Ok(pool) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(FinishedDeploy::Unfinished);
    };
    let Some(notes) = pool["notes"].as_array() else {
        return Ok(FinishedDeploy::Unfinished);
    };
    let Some(recorded) = pool_note_matching(notes, deployed) else {
        // Deployed but never recorded: finalizing it is the one thing this file is still for, and
        // retiring it here would strand a note nothing else can add.
        return Ok(FinishedDeploy::Unfinished);
    };
    let pooled_secret = recorded["owner_secret_key_hex"]
        .as_str()
        .unwrap_or_default();
    if pooled_secret != recovery.owner_secret_key_hex.as_str() {
        bail!(
            "note deploy: recovery file {} holds the deploy of note {}, which --pool {} records \
             under a DIFFERENT owner key. Refusing to touch either: this file may be the only copy \
             of the key to that note. Settle which key is right before deploying again.",
            recovery_path.display(),
            dexdo_core::address::display(deployed),
            pool_path.display()
        );
    }
    let resolved = resolve_private_file_path(recovery_path, "note deploy recovery")?;
    let _lock = acquire_note_deploy_recovery_write_lock(&resolved)?;
    match std::fs::remove_file(&resolved) {
        Ok(()) => Ok(FinishedDeploy::Retired),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FinishedDeploy::Retired),
        Err(error) => bail!(
            "retire finished note deploy recovery {}: {error}",
            resolved.display()
        ),
    }
}

/// The owner secret the pool recorded for `address`, if it holds that note at all.

/// The pool is where `note deploy` writes the key it generated, so this is the client reading back
/// its own record rather than a new place for a secret to live.
pub(crate) fn pool_owner_secret(pool: &Value, address: &str) -> Option<String> {
    let notes = pool["notes"].as_array()?;
    pool_note_matching(notes, address)?["owner_secret_key_hex"]
        .as_str()
        .map(str::to_string)
}

/// The pool entry for `address`, under the same matching rule the duplicate refusal uses.
fn pool_note_matching<'a>(notes: &'a [Value], address: &str) -> Option<&'a Value> {
    let normalized = dexdo_core::normalize_wallet_address(address).ok();
    notes.iter().find(|note| {
        note["address"].as_str().is_some_and(|recorded| {
            recorded == address
                || normalized.as_ref().is_some_and(|address| {
                    dexdo_core::normalize_wallet_address(recorded).ok().as_ref() == Some(address)
                })
        })
    })
}

pub(crate) fn ensure_shell_pool_currency(pool: &Value) -> Result<()> {
    let token_type = pool
        .get("token_type")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            anyhow!(
                "DEXDO_PN_POOL token_type is missing or malformed; dexdo markets require SHELL currency id {SHELL_CURRENCY_ID}"
            )
        })?;
    let token_type = u32::try_from(token_type)
        .map_err(|_| anyhow!("DEXDO_PN_POOL token_type {token_type} is out of range"))?;
    ensure_shell_currency_id(token_type, "DEXDO_PN_POOL")
}

fn ensure_shell_currency_id(token_type: u32, source: &str) -> Result<()> {
    if token_type != SHELL_CURRENCY_ID {
        bail!(
            "{source} token_type {token_type} is unsupported; dexdo markets require SHELL currency id {SHELL_CURRENCY_ID}"
        );
    }
    Ok(())
}

pub(crate) fn pool_with_note_token_contract_recorded(
    mut pool: Value,
    note_addr: &str,
    token_contract: &str,
    role: &str,
    updated_at_unix: u64,
) -> Result<Value> {
    if role != "buyer" && role != "seller" {
        bail!("token_contract_role must be buyer or seller, got `{role}`");
    }
    let note_addr = dexdo_core::normalize_wallet_address(note_addr)
        .map_err(|e| anyhow!("note address {note_addr}: {e}"))?;
    // Two values from one input, deliberately. The NORMALISED one is for comparing -- it drops the
    // DApp half, which is what makes two spellings of one account compare equal. The RECORDED one
    // keeps whatever DApp the caller named and supplies the self-DApp identity only when none was
    // named, because a per-deal TokenContract is a self-DApp account.

    // Rendering the normalised value would re-scope a TokenContract the caller named under some
    // other DApp -- the same hazard the `address` write below refuses, and `display_self_dapp`'s own
    // contract ("a supplied canonical address is authoritative and survives unchanged") is defeated
    // if a normalise runs in front of it.
    let token_contract_recorded = dexdo_core::address::display_self_dapp(token_contract);
    // The normalised form is not stored and not compared here -- the loop below matches on the
    // note's ADDRESS. It is computed for its refusal alone: a `token_contract` that is not an
    // address must stop this write before the entry is touched, and that check is this call. Bound
    // to `_` because a named binding nothing reads is a lint failure and reads as an oversight.
    let _: String = dexdo_core::normalize_wallet_address(token_contract)
        .map_err(|e| anyhow!("token_contract {token_contract}: {e}"))?;
    let notes = pool["notes"]
        .as_array_mut()
        .ok_or_else(|| anyhow!("DEXDO_PN_POOL: malformed (\"notes\" is not an array)"))?;
    let mut matched = 0usize;
    for note in notes {
        let Some(address) = note["address"].as_str() else {
            continue;
        };
        let normalized = dexdo_core::normalize_wallet_address(address)
            .unwrap_or_else(|_| address.trim().to_ascii_lowercase());
        if normalized == note_addr {
            matched += 1;
            // The ENTRY'S OWN address upgraded, never the match key. Writing `note_addr` back here
            // downgraded an already-canonical entry to `0:<account_id>` -- a pool written correctly
            // by `note deploy` reverted on the first recovery write. Writing the CANONICALISED match
            // key would be worse in a rarer case and silent: `normalize_wallet_address` drops the
            // DApp half to compare, so an entry recorded under some other DApp matches here and
            // would be re-scoped to the dexdo DApp by a write that was only supposed to attach a
            // TokenContract. A supplied DApp id is authoritative; this upgrades what is missing.
            let recorded = pool_note_address_as_recorded(address);
            note["address"] = json!(recorded);
            // The TokenContract in ITS canonical form, which is the self-DApp one: a per-deal TC is
            // a self-DApp account, as `pool_note_recovery_records` and the recovery writer already
            // spell it when they print it. Left as the workchain form it would put the two
            // conventions this file is unifying inside a single entry, one field apart.
            note["token_contract"] = json!(token_contract_recorded);
            note["token_contract_role"] = json!(role);
            note["token_contract_updated_at_unix"] = json!(updated_at_unix);
        }
    }
    match matched {
        1 => Ok(pool),
        0 => bail!(
            "DEXDO_PN_POOL has no note entry for {}; refusing to claim TokenContract recovery metadata \
             was persisted",
            dexdo_core::address::display(&note_addr)
        ),
        _ => bail!(
            "DEXDO_PN_POOL has {matched} entries for note {}; refusing ambiguous TokenContract metadata",
            dexdo_core::address::display(&note_addr)
        ),
    }
}

pub(crate) fn pool_has_unique_note_entry(pool: &Value, note_addr: &str) -> Result<()> {
    let note_addr = dexdo_core::normalize_wallet_address(note_addr)
        .map_err(|e| anyhow!("note address {note_addr}: {e}"))?;
    let notes = pool["notes"]
        .as_array()
        .ok_or_else(|| anyhow!("DEXDO_PN_POOL: malformed (\"notes\" is not an array)"))?;
    let matched = notes
        .iter()
        .filter_map(|note| note["address"].as_str())
        .filter(|address| {
            dexdo_core::normalize_wallet_address(address)
                .unwrap_or_else(|_| address.trim().to_ascii_lowercase())
                == note_addr
        })
        .count();
    match matched {
        1 => Ok(()),
        0 => bail!(
            "DEXDO_PN_POOL has no note entry for {}",
            dexdo_core::address::display(&note_addr)
        ),
        _ => bail!(
            "DEXDO_PN_POOL has {matched} entries for note {}",
            dexdo_core::address::display(&note_addr)
        ),
    }
}

/// One recovery-capable pool note entry: the durable recorded facts a pool-only recovery
/// (`reclaim`/`recover`/`dispute`) is allowed to act on. `recorded_at_unix` is the entry's own
/// `token_contract_updated_at_unix` -- a recorded fact written when the deal metadata was persisted,
/// never re-derived from the reader's wall clock or from the entry's position in the file.
/// Deliberately derives nothing: this struct holds a bare 64-hex owner secret, and a `Debug` impl is a
/// formatting footgun that costs nothing until the day something logs the struct or folds it into an
/// error message. Tests compare it by destructuring instead, which is exhaustive in the same way an
/// `assert_eq!` on the whole value is.
pub(crate) struct PoolNoteRecoveryRecord {
    pub(crate) note_addr: String,
    pub(crate) owner_secret_hex: String,
    pub(crate) token_contract: String,
    pub(crate) role: String,
    pub(crate) recorded_at_unix: Option<u64>,
}

/// Read every recovery entry the pool records.

/// A note that records no `token_contract` has simply never been in a deal and is not a recovery entry
/// at all. A note that **does** claim recovery metadata must carry all of it, well formed: a pool-only
/// recovery moves money on these recorded facts alone, so a half-recorded or wrong-typed entry is
/// refused loudly here, before any chain contact, instead of being silently dropped from the plan while
/// its escrow stays stranded.
pub(crate) fn pool_note_recovery_records(pool: &Value) -> Result<Vec<PoolNoteRecoveryRecord>> {
    let notes = pool["notes"]
        .as_array()
        .ok_or_else(|| anyhow!("DEXDO_PN_POOL: malformed (\"notes\" is not an array)"))?;
    let mut out = Vec::new();
    for (index, note) in notes.iter().enumerate() {
        if note["token_contract"].is_null() {
            continue;
        }
        let token_contract = note["token_contract"].as_str().ok_or_else(|| {
            anyhow!("DEXDO_PN_POOL notes[{index}]: token_contract is present but is not a string")
        })?;
        let note_addr = note["address"].as_str().ok_or_else(|| {
            anyhow!(
                "DEXDO_PN_POOL notes[{index}] records TokenContract {} but has no string address; \
                 refusing to recover from an incomplete recovery record",
                dexdo_core::address::display_self_dapp(token_contract)
            )
        })?;
        let owner_secret = note["owner_secret_key_hex"].as_str().ok_or_else(|| {
            anyhow!(
                "DEXDO_PN_POOL notes[{index}] ({}) records TokenContract {} but has no string \
                 owner_secret_key_hex; its escrow cannot be recovered from this pool",
                dexdo_core::address::display(note_addr),
                dexdo_core::address::display_self_dapp(token_contract)
            )
        })?;
        let role = match &note["token_contract_role"] {
            Value::Null => "unknown",
            role => role.as_str().ok_or_else(|| {
                anyhow!(
                    "DEXDO_PN_POOL notes[{index}] ({}): token_contract_role is not a string",
                    dexdo_core::address::display(note_addr)
                )
            })?,
        };
        if role != "buyer" && role != "seller" && role != "unknown" {
            bail!(
                "DEXDO_PN_POOL token_contract_role must be buyer, seller, or unknown, got `{role}`"
            );
        }
        let recorded_at_unix = match &note["token_contract_updated_at_unix"] {
            Value::Null => None,
            recorded => Some(recorded.as_u64().ok_or_else(|| {
                anyhow!(
                    "DEXDO_PN_POOL notes[{index}] ({}): token_contract_updated_at_unix is not a unix \
                     second count",
                    dexdo_core::address::display(note_addr)
                )
            })?),
        };
        out.push(PoolNoteRecoveryRecord {
            note_addr: dexdo_core::normalize_wallet_address(note_addr)
                .map_err(|e| anyhow!("DEXDO_PN_POOL note address {note_addr}: {e}"))?,
            owner_secret_hex: owner_secret.to_string(),
            token_contract: dexdo_core::normalize_wallet_address(token_contract)
                .map_err(|e| anyhow!("DEXDO_PN_POOL token_contract {token_contract}: {e}"))?,
            role: role.to_string(),
            recorded_at_unix,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod note_deploy_tests {
    use super::*;

    fn fixture_secret_hex() -> String {
        "2a".repeat(32)
    }

    fn complete_state() -> OnboardPnState {
        let secret = fixture_secret_hex();
        let public = derive_owner_pubkey_from_secret_hex(&secret).expect("fixture key derives");
        OnboardPnState {
            endpoint: "net-a.example".into(),
            nominal: "N100".into(),
            token_type: SHELL_CURRENCY_ID,
            raw_value: 100_000_000_000,
            pn_address: Some("0:abc".into()),
            deposit_identifier_hash: Some("123".into()),
            owner_public_key_hex: Some(public),
            owner_secret_key_hex: Some(secret.into()),
            deployed_at_unix: Some(1000),
            shell_funded: true,
            sanity_checked: true,
        }
    }

    fn voucher_checkpoint_with_proof() -> NoteDeployVoucherCheckpoint {
        let mut checkpoint = NoteDeployVoucherCheckpoint::new(
            &"1".repeat(64),
            SHELL_CURRENCY_ID,
            100,
            false,
            "2".repeat(64),
            "3".repeat(64),
        )
        .unwrap();
        checkpoint.submit_maybe_sent = true;
        checkpoint.event = Some(NoteDeployVoucherEvent {
            id: "event".into(),
            boc: "boc".into(),
            body: "body".into(),
            dst: format!("0:{}", "4".repeat(64)),
            created_at: 1,
            block_id: Some("block".into()),
        });
        checkpoint.proof = Some(NoteDeployVoucherProof {
            proof: "proof-layer-1".into(),
            deposit_identifier_hash_hex: "5".repeat(64),
            final_layer_historical_hash_root_hex: "6".repeat(64),
            voucher_nominal_fr_hex: "7".repeat(64),
            token_type_fr_hex: "8".repeat(64),
            ephemeral_pubkey_hex: "1".repeat(64),
            voucher_value: 100,
            voucher_token_type: SHELL_CURRENCY_ID,
            layer_number: 1,
            sk_u_hex: "2".repeat(64).into(),
            sk_u_commit_hex: "3".repeat(64),
        });
        checkpoint.validate("fixture checkpoint").unwrap();
        checkpoint
    }

    #[test]
    fn note_nominal_accepts_every_contract_legal_value_and_computes_funding() {
        let gas_deposit = u128::from(contract_gas_deposit_raw());
        let decimals = u64::try_from(UNIT_SCALE).expect("unit scale fits u64");
        for ((spelling, count), listed) in [
            ("N100", 100u64),
            ("N1000", 1_000),
            ("N10000", 10_000),
            ("N100000", 100_000),
            ("N1000000", 1_000_000),
        ]
        .into_iter()
        .zip(NoteNominal::ALL)
        {
            let nominal = NoteNominal::parse(spelling)
                .unwrap_or_else(|error| panic!("{spelling} is contract-legal: {error}"));
            assert_eq!(nominal, listed, "{spelling} is listed for funding output");
            assert_eq!(nominal.label(), spelling);
            let raw = nominal.raw_value(decimals);
            assert_eq!(raw, count * decimals, "{spelling} nominal");
            assert_eq!(
                note_deploy_voucher_wire_raw(false, raw),
                u128::from(raw) + gas_deposit,
                "{spelling} funding includes RootPN.GAS_DEPOSIT"
            );
        }
    }

    /// the whole ECC[2] requirement of a SHELL `note deploy` is three summands, and the one
    /// the recipe used to omit is the gas voucher. `GAS_DEPOSIT` is taken from the CONTRACT here,
    /// not from `ROOT_PN_GAS_DEPOSIT_RAW`: anchoring it to the client constant
    /// would compare the client against a second copy of itself.
    #[test]
    fn note_deploy_shell_ecc_requirement_is_nominal_plus_gas_deposit() {
        let gas_deposit = u128::from(contract_gas_deposit_raw());
        let decimals = u64::try_from(UNIT_SCALE).expect("unit scale fits u64");
        for nominal in NoteNominal::ALL {
            let raw = nominal.raw_value(decimals);
            assert_eq!(
                note_deploy_shell_ecc_required_raw(raw),
                u128::from(raw) + gas_deposit,
                "{} requires the nominal and GAS_DEPOSIT, and nothing beyond them",
                nominal.label()
            );
            assert_eq!(
                note_deploy_shell_ecc_required_raw(raw),
                note_deploy_voucher_wire_raw(false, raw),
                "{} has one leg: what the wallet must hold is what the deposit attaches",
                nominal.label()
            );
        }
    }

    /// The recipe's two stages are different amounts and each is derived from one place. Stage two
    /// is the deploy requirement itself; stage one is the native predeploy leg, which the gas
    /// voucher must NOT reach -- it is ECC[2] money paid to RootPN, not the wallet's own gas.

    /// This test used to close with `ecc - native == gas_voucher`. That holds only while stage one
    /// is `nominal + GAS_DEPOSIT`, so the assertion pinned the money defect itself: a native
    /// predeploy leg that scaled with the nominal, on a leg that becomes gas and can never be spent
    /// as currency again. Stage one is now flat deploy gas
    /// ([`dexdo_core::params::OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE`]), so that relationship no
    /// longer exists. It is replaced below by the invariant that took its place rather than dropped:
    /// stage one is identical for every nominal, while stage two still moves with each one.
    #[test]
    fn operator_wallet_recipe_stage_one_is_flat_stage_two_carries_the_nominal_and_summands_add_up()
    {
        let gas_deposit = u128::from(contract_gas_deposit_raw());
        let decimals = dexdo_core::private_note::proof::TokenType::Shell.decimals();
        let stage_one = operator_wallet_predeploy_native_raw();
        let mut previous_stage_two: Option<u128> = None;
        for nominal in NoteNominal::ALL {
            let ecc = operator_wallet_funding_raw(nominal);
            let native = operator_wallet_predeploy_native_raw();
            let (nominal_raw, gas_deposit_raw) = operator_wallet_funding_summands_raw(nominal);

            assert_eq!(
                nominal_raw + gas_deposit_raw,
                ecc,
                "{} printed breakdown must add up to the figure note deploy checks",
                nominal.label()
            );
            assert_eq!(
                nominal_raw,
                u128::from(nominal.raw_value(decimals)),
                "{} first summand is the nominal",
                nominal.label()
            );
            assert_eq!(
                gas_deposit_raw,
                gas_deposit,
                "{} second summand is the contract's GAS_DEPOSIT",
                nominal.label()
            );
            assert_eq!(
                native,
                stage_one,
                "{} stage one must be the same flat deploy-gas figure as every other nominal: it \
                 becomes native gas that can never be spent as currency again, so nothing about \
                 the note being minted may move it",
                nominal.label()
            );
            if let Some(previous) = previous_stage_two {
                assert!(
                    ecc > previous,
                    "{} stage two must still move with the nominal ({ecc} raw is not above the \
                     previous nominal's {previous} raw); only stage one is flat",
                    nominal.label()
                );
            }
            previous_stage_two = Some(ecc);
        }
    }

    #[test]
    fn note_nominal_refuses_unlisted_value_and_names_every_legal_choice() {
        let error = NoteNominal::parse("N42").expect_err("N42 is not contract-legal");
        assert_eq!(
            error.to_string(),
            "unknown nominal `n42` (use N100|N1000|N10000|N100000|N1000000)"
        );
    }

    #[test]
    fn note_nominal_keeps_the_existing_spellings_byte_identical() {
        for (canonical, numeric, count) in [
            ("N100", "100", 100u64),
            ("N1000", "1000", 1_000),
            ("N10000", "10000", 10_000),
        ] {
            let named = NoteNominal::parse(canonical).expect("existing named spelling");
            let bare = NoteNominal::parse(numeric).expect("existing numeric spelling");
            assert_eq!(named, bare);
            assert_eq!(named.label().as_bytes(), canonical.as_bytes());
            assert_eq!(named.count(), count);
        }
    }

    /// the deducted figure comes from the contract's own constant, never from a number
    /// arrived at by trying values.: the oracle is `contract_gas_deposit_raw()` -- what the
    /// contract declares -- and never the client constant this test exists to check, so changing
    /// `ROOT_PN_GAS_DEPOSIT_RAW` to 251 turns this red instead of moving the goalposts with it.
    #[test]
    fn note_deploy_gas_deposit_mirrors_the_contract_constant() {
        assert_eq!(
            ROOT_PN_GAS_DEPOSIT_RAW,
            contract_gas_deposit_raw(),
            "RootPN.GAS_DEPOSIT and the deposit the client attaches must be the same figure"
        );
    }

    /// The defect this file exists to keep fixed: 4.0.33 `RootPN.generateVoucher` computes
    /// `nominal = attached - GAS_DEPOSIT` on the non-gas path and checks THAT against
    /// `ALLOWED_NOMINALS`, so a client attaching the bare nominal leaves 9 750 for an N10000 deposit
    /// and is refused (`ERR_NOT_ALLOWED`, 141) at every denomination -- N100 does not even reach the
    /// list (`ERR_BELOW_GAS_DEPOSIT`, 408). The wallet must attach nominal + `GAS_DEPOSIT`, and the
    /// nominal must not follow it: on the pre-fix arithmetic the attached figure equals the nominal
    /// and this test fails at the first denomination. The expected figure is the CONTRACT's
    /// -- a client constant moved to 251 fails here rather than dragging the expectation along.
    #[test]
    fn deposit_voucher_attaches_the_nominal_plus_the_gas_deposit() {
        let gas_deposit = u128::from(contract_gas_deposit_raw());
        for allowed in [100u64, 1_000, 10_000, 100_000, 1_000_000] {
            let nominal = allowed * 1_000_000_000;
            let checkpoint = NoteDeployVoucherCheckpoint::new(
                &"1".repeat(64),
                SHELL_CURRENCY_ID,
                nominal,
                false,
                "2".repeat(64),
                "3".repeat(64),
            )
            .unwrap();

            let attached = checkpoint.voucher_currency_map();
            assert_eq!(attached.len(), 1, "N{allowed}: one currency, the SHELL leg");
            let sent: u128 = attached
                .get(&SHELL_CURRENCY_ID.to_string())
                .and_then(Value::as_str)
                .expect("SHELL leg")
                .parse()
                .expect("numeric SHELL leg");
            assert_eq!(
                sent,
                u128::from(nominal) + gas_deposit,
                "N{allowed}: the wallet must attach the nominal plus RootPN.GAS_DEPOSIT"
            );
            assert_eq!(
                sent - gas_deposit,
                u128::from(nominal),
                "N{allowed}: what the contract keeps after its deduction must be the allowed nominal"
            );
            assert_eq!(
                checkpoint.raw_value, nominal,
                "N{allowed}: the proven nominal must not follow the attached figure"
            );
        }
    }

    /// The gas voucher pays no gas: `isFee = true` is the branch RootPN deducts nothing on, and its
    /// ECC is handed straight back to the same note. Charging it here would take 250 SHELL out of a
    /// 100 SHELL voucher and underflow the contract's own `ERR_BELOW_GAS_DEPOSIT` guard.
    #[test]
    fn gas_voucher_attaches_exactly_its_own_nominal() {
        let checkpoint = NoteDeployVoucherCheckpoint::new(
            &"1".repeat(64),
            SHELL_CURRENCY_ID,
            100_000_000_000,
            true,
            "2".repeat(64),
            "3".repeat(64),
        )
        .unwrap();
        assert_eq!(
            checkpoint
                .voucher_currency_map()
                .get(&SHELL_CURRENCY_ID.to_string())
                .and_then(Value::as_str),
            Some("100000000000"),
            "the gas voucher is deducted nothing, so it attaches exactly its nominal"
        );
    }

    /// The prover's money input is the one boundary that cannot be driven offline -- reaching it
    /// needs a real `VoucherGenerated` and a real halo2 prover -- so it is pinned at the source, and
    /// with it the rule that makes the pin sufficient: **the wire figure exists in exactly two
    /// places**, the currency map and the wallet preflight, and the gas deposit appears in
    /// `note_cmd.rs` production nowhere at all. The other three boundaries are observed directly
    /// (`note_deploy_wallet_message_attaches_wire_while_the_persisted_checkpoint_keeps_nominal`,
    /// `note_deploy_private_note_value_is_the_proven_nominal`).

    /// Every way of putting nominal + `GAS_DEPOSIT` into the proof is red here: calling the wire
    /// helper raises the count, naming the constant or its literal is refused outright, and either
    /// way `voucher_value: checkpoint.raw_value` no longer stands.
    #[test]
    fn the_gas_deposit_never_reaches_the_proof_or_the_deploy_value() {
        const NOTE_CMD: &str = include_str!("note_cmd.rs");
        let production = NOTE_CMD
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("note_cmd.rs production half");
        assert!(
            production.len() < NOTE_CMD.len(),
            "note_cmd.rs test module marker moved; this pin is reading the wrong half"
        );

        assert_eq!(
            production.matches("note_deploy_voucher_wire_raw(").count(),
            1,
            "the wire figure is computed in the wallet preflight and nowhere else in note_cmd.rs"
        );
        assert!(
            production.contains("let cc = checkpoint.voucher_currency_map();"),
            "the wallet message must attach what voucher_currency_map builds"
        );
        for forbidden in ["ROOT_PN_GAS_DEPOSIT_RAW", "250_000_000_000", "250000000000"] {
            assert!(
                !production.contains(forbidden),
                "note_cmd.rs production must not carry the gas deposit ({forbidden}); the only \
                 figures it handles are the nominal and what voucher_currency_map returns"
            );
        }
        assert!(
            production.contains("voucher_value: checkpoint.raw_value,"),
            "the halo2 prover must be given the proven nominal from the persisted checkpoint"
        );
        assert!(
            production.contains("\"value\": deposit_zk.voucher_value,"),
            "deployPrivateNote must be given the nominal the proof was built over"
        );
    }

    /// The money-path trap in the same change: `VoucherGenerated` emits the POST-deduction figure,
    /// so a proof built over the attached amount is a public-input mismatch (`ERR_INVALID_ZKPROOF`,
    /// 137) found only after the wallet has already spent. The persisted checkpoint keeps the
    /// nominal and refuses a proof carrying the wire figure.
    #[test]
    fn persisted_proof_keeps_the_nominal_not_the_attached_amount() {
        let checkpoint = voucher_checkpoint_with_proof();
        assert!(!checkpoint.is_fee);
        checkpoint
            .validate("nominal proof")
            .expect("a proof over the nominal is the valid one");

        let mut over_the_wire = checkpoint.clone();
        over_the_wire.proof.as_mut().unwrap().voucher_value =
            checkpoint.raw_value + contract_gas_deposit_raw();
        let error = over_the_wire
            .validate("wire proof")
            .expect_err("a proof over the attached amount must be refused before it is used")
            .to_string();
        assert!(
            error.contains("proof voucher_value does not match checkpoint"),
            "{error}"
        );
    }

    fn replace_with_next_layer(checkpoint: &mut NoteDeployVoucherCheckpoint) -> bool {
        let Some(next) = checkpoint.next_sdk_proof_layer() else {
            return false;
        };
        let mut replacement = checkpoint.proof.as_ref().unwrap().clone();
        replacement.layer_number = next as u8 + 1;
        replacement.proof = format!("proof-layer-{}", replacement.layer_number);
        replacement.final_layer_historical_hash_root_hex =
            replacement.layer_number.to_string().repeat(64);
        checkpoint.replace_rejected_proof(replacement).unwrap();
        true
    }

    #[test]
    fn secret_bearing_note_states_redact_debug_output() {
        fn assert_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>(_: &T) {}

        let mut onboard = complete_state();
        onboard.owner_secret_key_hex = Some("onboard-secret-sentinel".to_string().into());
        assert_zeroize_on_drop(onboard.owner_secret_key_hex.as_ref().unwrap());
        let onboard_debug = format!("{onboard:?}");
        assert!(!onboard_debug.contains("onboard-secret-sentinel"));
        assert!(onboard_debug.contains("owner_secret_key_hex: \"<redacted>\""));
        assert!(onboard_debug.contains("net-a.example"));

        let proof = NoteDeployVoucherProof {
            proof: "public-proof".into(),
            deposit_identifier_hash_hex: "deposit-id".into(),
            final_layer_historical_hash_root_hex: "history-root".into(),
            voucher_nominal_fr_hex: "nominal".into(),
            token_type_fr_hex: "token-type".into(),
            ephemeral_pubkey_hex: "ephemeral-key".into(),
            voucher_value: 1,
            voucher_token_type: 2,
            layer_number: 3,
            sk_u_hex: "proof-secret-sentinel".to_string().into(),
            sk_u_commit_hex: "proof-commit".into(),
        };
        assert_zeroize_on_drop(&proof.sk_u_hex);
        let proof_debug = format!("{proof:?}");
        assert!(!proof_debug.contains("proof-secret-sentinel"));
        assert!(proof_debug.contains("sk_u_hex: \"<redacted>\""));
        assert!(proof_debug.contains("proof-commit"));

        let checkpoint = NoteDeployVoucherCheckpoint {
            sk_u_hex: "checkpoint-secret-sentinel".to_string().into(),
            sk_u_commit_hex: "checkpoint-commit".into(),
            recipient_ephemeral_pubkey_hex: "recipient-key".into(),
            token_type: 2,
            raw_value: 1,
            is_fee: false,
            submit_maybe_sent: false,
            event: None,
            proof: Some(proof),
            last_rejected_proof_layer: None,
        };
        assert_zeroize_on_drop(&checkpoint.sk_u_hex);
        let checkpoint_debug = format!("{checkpoint:?}");
        assert!(!checkpoint_debug.contains("checkpoint-secret-sentinel"));
        assert!(checkpoint_debug.contains("sk_u_hex: \"<redacted>\""));
        assert!(checkpoint_debug.contains("checkpoint-commit"));

        let mut recovery = complete_recovery_state();
        recovery.owner_secret_key_hex = "recovery-secret-sentinel".to_string().into();
        assert_zeroize_on_drop(&recovery.owner_secret_key_hex);
        recovery.deposit_voucher = Some(checkpoint);

        let recovery_debug = format!("{recovery:?}");
        assert!(!recovery_debug.contains("recovery-secret-sentinel"));
        assert!(recovery_debug.contains("owner_secret_key_hex: \"<redacted>\""));
        assert!(recovery_debug.contains("0:abc"));
    }

    #[test]
    fn rejected_history_layers_roundtrip_and_exhaust_without_losing_paid_voucher() {
        let mut checkpoint = voucher_checkpoint_with_proof();
        let mut rejected_second_layer = checkpoint.clone();
        rejected_second_layer.proof.as_mut().unwrap().layer_number = 2;
        rejected_second_layer.last_rejected_proof_layer = Some(2);
        assert_eq!(rejected_second_layer.next_sdk_proof_layer(), Some(2));
        let mut stale_after_second_rejection = rejected_second_layer.clone();
        stale_after_second_rejection
            .proof
            .as_mut()
            .unwrap()
            .layer_number = 1;
        assert!(stale_after_second_rejection
            .validate("stale recovery")
            .is_err());

        let mut skipped_layer = checkpoint.clone();
        skipped_layer.reject_current_proof().unwrap();
        let mut layer_three = skipped_layer.proof.as_ref().unwrap().clone();
        layer_three.layer_number = NOTE_DEPLOY_PROOF_LAYER_MAX;
        assert!(skipped_layer.replace_rejected_proof(layer_three).is_err());

        let identity = (
            checkpoint.sk_u_hex.clone(),
            checkpoint.sk_u_commit_hex.clone(),
            checkpoint.event.clone(),
            checkpoint
                .proof
                .as_ref()
                .unwrap()
                .deposit_identifier_hash_hex
                .clone(),
        );

        for expected_layer in 1..=NOTE_DEPLOY_PROOF_LAYER_MAX {
            assert_eq!(
                checkpoint.proof.as_ref().unwrap().layer_number,
                expected_layer
            );
            checkpoint.reject_current_proof().unwrap();
            if expected_layer < NOTE_DEPLOY_PROOF_LAYER_MAX {
                assert!(replace_with_next_layer(&mut checkpoint));
            }
        }

        assert!(checkpoint.current_proof_is_rejected());
        assert_eq!(
            checkpoint.last_rejected_proof_layer,
            Some(NOTE_DEPLOY_PROOF_LAYER_MAX)
        );
        assert_eq!(checkpoint.next_sdk_proof_layer(), None);
        let roundtrip: NoteDeployVoucherCheckpoint =
            serde_json::from_str(&serde_json::to_string(&checkpoint).unwrap()).unwrap();
        assert_eq!(roundtrip, checkpoint);
        assert_eq!(
            (
                roundtrip.sk_u_hex,
                roundtrip.sk_u_commit_hex,
                roundtrip.event,
                roundtrip
                    .proof
                    .as_ref()
                    .unwrap()
                    .deposit_identifier_hash_hex
                    .clone(),
            ),
            identity
        );
    }

    #[test]
    fn recovery_load_rejects_proof_that_skips_the_next_layer() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-skipped-layer-test");
        let path = dir.join("pn_pool.json.recovery.json");
        let mut recovery = complete_recovery_state();
        let mut checkpoint = voucher_checkpoint_with_proof();
        checkpoint.recipient_ephemeral_pubkey_hex = recovery.owner_public_key_hex.clone();
        checkpoint.token_type = recovery.token_type;
        checkpoint.raw_value = recovery.raw_value;
        let proof = checkpoint.proof.as_mut().unwrap();
        proof.ephemeral_pubkey_hex = recovery.owner_public_key_hex.clone();
        proof.voucher_token_type = recovery.token_type;
        proof.voucher_value = recovery.raw_value;
        proof.layer_number = NOTE_DEPLOY_PROOF_LAYER_MAX;
        checkpoint.last_rejected_proof_layer = Some(1);
        recovery.deposit_voucher = Some(checkpoint);
        write_private_atomic(&path, &serde_json::to_vec_pretty(&recovery).unwrap()).unwrap();

        let error = load_note_deploy_recovery(&path).unwrap_err().to_string();

        assert!(
            error.contains(&format!(
                "current proof layer {NOTE_DEPLOY_PROOF_LAYER_MAX}"
            )),
            "{error}"
        );
        assert!(error.contains("rejected layer 1"), "{error}");
        assert!(error.contains("immediate successor 2"), "{error}");

        let checkpoint = recovery.deposit_voucher.as_mut().unwrap();
        checkpoint.last_rejected_proof_layer = Some(NOTE_DEPLOY_PROOF_LAYER_MAX);
        checkpoint.proof.as_mut().unwrap().layer_number =
            NOTE_DEPLOY_PROOF_LAYER_MAX.saturating_add(1);
        write_private_atomic(&path, &serde_json::to_vec_pretty(&recovery).unwrap()).unwrap();

        let error = load_note_deploy_recovery(&path).unwrap_err().to_string();

        assert!(
            error.contains(&format!(
                "current proof layer {}",
                NOTE_DEPLOY_PROOF_LAYER_MAX.saturating_add(1)
            )),
            "{error}"
        );
        assert!(
            error.contains(&format!(
                "outside canonical plan 1..={NOTE_DEPLOY_PROOF_LAYER_MAX}"
            )),
            "{error}"
        );
    }

    proptest::proptest! {
        #[test]
        fn history_reproof_never_reuses_submitted_layer_or_wallet_spend(
            exact_403_outcomes in proptest::collection::vec(proptest::bool::ANY, 0..12)
        ) {
            let mut checkpoint = voucher_checkpoint_with_proof();
            let identity = (
                checkpoint.sk_u_hex.clone(),
                checkpoint.sk_u_commit_hex.clone(),
                checkpoint.event.clone(),
                checkpoint.proof.as_ref().unwrap().deposit_identifier_hash_hex.clone(),
            );
            let mut wallet_submit_maybe_sent = false;
            let mut wallet_sends = 0_usize;
            let mut submitted_layers = Vec::new();

            for exact_403 in exact_403_outcomes {
                if !wallet_submit_maybe_sent {
                    wallet_submit_maybe_sent = true;
                    wallet_sends += 1;
                }
                if checkpoint.current_proof_is_rejected()
                    && !replace_with_next_layer(&mut checkpoint)
                {
                    break;
                }
                let layer = checkpoint.proof.as_ref().unwrap().layer_number;
                proptest::prop_assert!(!submitted_layers.contains(&layer));
                submitted_layers.push(layer);
                if exact_403 {
                    checkpoint.reject_current_proof().unwrap();
                } else {
                    break;
                }
            }

            proptest::prop_assert!(wallet_sends <= 1);
            proptest::prop_assert!(
                submitted_layers.len() <= usize::from(NOTE_DEPLOY_PROOF_LAYER_MAX)
            );
            proptest::prop_assert_eq!(
                (
                    checkpoint.sk_u_hex.clone(),
                    checkpoint.sk_u_commit_hex.clone(),
                    checkpoint.event.clone(),
                    checkpoint.proof.as_ref().unwrap().deposit_identifier_hash_hex.clone(),
                ),
                identity
            );
        }
    }

    fn tvm_tonos_fixture_phrase() -> String {
        const WORD_INDICES: [u16; 12] = [
            1636, 1293, 905, 102, 1057, 1956, 1247, 1750, 597, 881, 1302, 3,
        ];
        WORD_INDICES
            .iter()
            .map(|i| bip39::Language::English.wordlist().get_word((*i).into()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn recovery_request<'a>(
        endpoint: &'a str,
        funding_multisig_address: &'a str,
    ) -> NoteDeployRecoveryRequest<'a> {
        NoteDeployRecoveryRequest {
            endpoint,
            nominal: "N100",
            token_type: SHELL_CURRENCY_ID,
            raw_value: 100_000_000_000,
            funding_multisig_address,
        }
    }

    fn complete_recovery_state() -> NoteDeployRecoveryState {
        let state = complete_state();
        NoteDeployRecoveryState {
            version: NOTE_DEPLOY_RECOVERY_VERSION,
            endpoint: state.endpoint,
            nominal: state.nominal,
            token_type: state.token_type,
            raw_value: state.raw_value,
            funding_multisig_address: format!("0:{}", "a".repeat(64)),
            owner_public_key_hex: state.owner_public_key_hex.unwrap(),
            owner_secret_key_hex: state.owner_secret_key_hex.unwrap(),
            pn_address: state.pn_address,
            deposit_identifier_hash: state.deposit_identifier_hash,
            deployed_at_unix: state.deployed_at_unix,
            deposit_voucher: None,
            shell_funded: state.shell_funded,
            sanity_checked: state.sanity_checked,
        }
    }

    /// A pool file holding exactly the notes named, funded by `funding` -- a pool with notes and no
    /// funding wallet is refused before anything about duplicates is decided, which is not what
    /// these cases are about.
    fn pool_file_with(
        dir: &std::path::Path,
        addresses: &[&str],
        funding: &str,
    ) -> std::path::PathBuf {
        let path = dir.join("pn_pool.json");
        // With the owner key the fixture recovery carries: a pool entry is only a safe second copy
        // when it holds the same secret, and that is what retiring checks.
        let notes: Vec<Value> = addresses
            .iter()
            .map(|address| {
                json!({
                    "address": address,
                    "owner_secret_key_hex": complete_recovery_state().owner_secret_key_hex.as_str(),
                })
            })
            .collect();
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "token_type": SHELL_CURRENCY_ID,
                "nominal": "N100",
                "funding_multisig_address": funding,
                "notes": notes,
            }))
            .expect("serialize pool"),
        )
        .expect("write pool");
        path
    }

    /// The live failure this exists for: a second `note deploy` on the default recovery path
    /// loaded a finished attempt, asked the operator to confirm a Vault -> Hot transfer, ran the
    /// whole flow, and only then refused at the pool. Nothing about that run could ever have
    /// produced a note -- so the spent file is retired and the run deploys a new one.
    #[test]
    fn a_finished_recorded_deploy_is_retired_so_the_next_one_can_be_deployed() {
        let temp = tempfile::tempdir().expect("temp dir");
        let recovery = complete_recovery_state();
        let deployed = recovery.pn_address.clone().expect("fixture deploys a note");
        let pool = pool_file_with(
            temp.path(),
            &[&deployed],
            &recovery.funding_multisig_address,
        );
        let recovery_path = temp.path().join("pn_pool.json.recovery.json");

        crate::cli::support::write_owner_only_key_fixture(&recovery_path, "{}");

        let verdict = retire_a_finished_deploy(&recovery_path, &recovery, &pool)
            .expect("a recorded deploy whose key the pool holds is retirable");

        assert_eq!(verdict, FinishedDeploy::Retired);
        assert!(!recovery_path.exists(), "the spent file must be gone");
    }

    /// The guard that makes retiring safe. A pool entry under a DIFFERENT key means this file may
    /// be the only copy of the key to that note, and deleting it would put the note out of reach
    /// for good.
    #[test]
    fn a_pool_entry_with_another_key_is_refused_and_the_file_kept() {
        let temp = tempfile::tempdir().expect("temp dir");
        let recovery = complete_recovery_state();
        let deployed = recovery.pn_address.clone().expect("fixture deploys a note");
        let pool_path = temp.path().join("pn_pool.json");
        std::fs::write(
            &pool_path,
            serde_json::to_vec(&json!({
                "token_type": SHELL_CURRENCY_ID,
                "nominal": "N100",
                "funding_multisig_address": recovery.funding_multisig_address,
                "notes": [{ "address": deployed, "owner_secret_key_hex": "ff".repeat(32) }],
            }))
            .expect("serialize pool"),
        )
        .expect("write pool");
        let recovery_path = temp.path().join("pn_pool.json.recovery.json");
        std::fs::write(&recovery_path, b"{}").expect("a file that must survive");

        let error = retire_a_finished_deploy(&recovery_path, &recovery, &pool_path)
            .expect_err("a mismatched key must stop the run, not delete the file");

        assert!(error.to_string().contains("DIFFERENT owner key"), "{error}");
        assert!(recovery_path.exists(), "the file must be kept");
    }

    /// The one thing this must not break: a note that WAS deployed but never made it into the pool
    /// is finished by re-running with that same file. Refusing there would strand it.
    #[test]
    fn a_deploy_the_pool_never_recorded_is_still_resumable() {
        let temp = tempfile::tempdir().expect("temp dir");
        let recovery = complete_recovery_state();
        let pool = pool_file_with(
            temp.path(),
            &[&format!("0:{}", "d".repeat(64))],
            &recovery.funding_multisig_address,
        );

        assert_eq!(
            retire_a_finished_deploy(&temp.path().join("r.json"), &recovery, &pool)
                .expect("an unrecorded deploy must still be finishable"),
            FinishedDeploy::Unfinished
        );
    }

    /// An interrupted attempt has no note yet, and a missing pool is the first run of all. Neither
    /// is a finished deploy, and refusing either would make the command unusable.
    #[test]
    fn an_unfinished_attempt_and_a_missing_pool_both_pass() {
        let temp = tempfile::tempdir().expect("temp dir");
        let mut unfinished = complete_recovery_state();
        unfinished.pn_address = None;
        let pool = pool_file_with(temp.path(), &[], &unfinished.funding_multisig_address);
        assert_eq!(
            retire_a_finished_deploy(&temp.path().join("r.json"), &unfinished, &pool)
                .expect("an attempt with no note is what resuming is for"),
            FinishedDeploy::Unfinished
        );

        let finished = complete_recovery_state();
        assert_eq!(
            retire_a_finished_deploy(
                &temp.path().join("r.json"),
                &finished,
                &temp.path().join("absent.json"),
            )
            .expect("no pool yet is the first run, not a duplicate"),
            FinishedDeploy::Unfinished
        );
    }

    /// The early refusal and the pool write must agree about what "the same note" means. They read
    /// the same file for opposite reasons -- one to stop a run before it spends, the other to stop
    /// a duplicate before it is written -- and a disagreement means one of them lets through
    /// exactly what the other exists to catch.

    /// Asserted as agreement rather than against a hand-written expectation: the address forms a
    /// pool can hold are the comparison's business, and pinning them twice is how the two copies
    /// drifted apart in the first place.
    #[test]
    fn retiring_and_the_pool_write_agree_on_what_the_same_note_is() {
        let temp = tempfile::tempdir().expect("temp dir");
        let recovery = complete_recovery_state();
        let deployed = recovery.pn_address.clone().expect("fixture deploys a note");
        let funding = recovery.funding_multisig_address.clone();

        for (index, recorded) in [
            deployed.clone(),
            deployed.to_ascii_uppercase(),
            format!("0:{}", "d".repeat(64)),
        ]
        .into_iter()
        .enumerate()
        {
            let dir = temp.path().join(format!("case-{index}"));
            std::fs::create_dir_all(&dir).expect("case dir");
            let pool_path = pool_file_with(&dir, &[&recorded], &funding);
            let recovery_path = dir.join("r.json");
            crate::cli::support::write_owner_only_key_fixture(&recovery_path, "{}");

            // The same question asked of both: is the note in this recovery file already in this
            // pool. One answers by retiring the spent file, the other by refusing a duplicate.
            let matched_early = retire_a_finished_deploy(&recovery_path, &recovery, &pool_path)
                .expect("the pool holds the same key in every case here")
                == FinishedDeploy::Retired;

            let pool: Value =
                serde_json::from_slice(&std::fs::read(&pool_path).expect("read pool"))
                    .expect("parse pool");
            let refused_at_write = pool_with_note_added(
                Some(pool),
                &complete_state(),
                json!({ "address": deployed, "nominal": "N100" }),
                42,
                &funding,
            )
            .err()
            .is_some_and(|error| error.to_string().contains("already in the pool"));

            assert_eq!(
                matched_early, refused_at_write,
                "recorded as {recorded}: the two readers disagree about the same note"
            );
        }
    }

    fn recovery_state_for_owner(
        secret: &str,
        note_address: Option<&str>,
    ) -> NoteDeployRecoveryState {
        let mut state = complete_recovery_state();
        state.owner_secret_key_hex = secret.to_string().into();
        state.owner_public_key_hex = derive_owner_pubkey_from_secret_hex(secret).unwrap();
        state.pn_address = note_address.map(ToOwned::to_owned);
        state.deposit_identifier_hash =
            note_address.map(|address| address.trim_start_matches("0:").chars().take(64).collect());
        state.deployed_at_unix = note_address.map(|_| 1000);
        state.shell_funded = note_address.is_some();
        state.sanity_checked = note_address.is_some();
        state.validate().unwrap();
        state
    }

    struct TempDirCleanup(std::path::PathBuf);

    impl Drop for TempDirCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(prefix: &str) -> (std::path::PathBuf, TempDirCleanup) {
        let dir = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&dir).unwrap();
        (dir.clone(), TempDirCleanup(dir))
    }

    /// account-reader data formats SHELL/ECC[2] and native gas in readable units plus raw units.
    #[test]
    fn note_balance_formats_shell_and_native_balances() {
        let view = build_note_balance_view(
            "0:abc",
            Some(NoteAccountSnapshot {
                address: "0:abc".into(),
                status: "Active".into(),
                native_raw: 5_000_000_123,
                ecc: vec![(7, 42), (2, 1_234_567_890)],
                code_hash: Some("cafe".into()),
            }),
            NoteGetterBalanceMaps {
                balance: NoteBalanceMap::Known(vec![(2, 2_000_000_001), (1, 10)]),
                locked_in_orders: NoteBalanceMap::Unknown("getter unavailable".into()),
            },
        )
        .unwrap();
        let out = render_note_balance(&view);
        assert!(out.contains("PrivateNote 0:abc"), "{out}");
        assert!(
            out.contains("SHELL gas ECC[2]: 1.234567890 SHELL (raw 1234567890)"),
            "{out}"
        );
        assert!(
            out.contains("VMSHELL native gas: 5.000000123 vmshell (raw 5000000123)"),
            "{out}"
        );
        assert!(
            out.contains("ECC[2] SHELL: 2.000000001 SHELL (raw 2000000001)"),
            "{out}"
        );
        assert!(out.contains("ECC[7]: raw 42"), "{out}");
        assert!(out.contains("unknown (getter unavailable)"), "{out}");
    }

    /// The renderer keeps spendable trading money distinct from account-level SHELL gas.
    #[test]
    fn note_balance_consumer_prefixes_match_renderer() {
        let view = build_note_balance_view(
            "0:abc",
            Some(NoteAccountSnapshot {
                address: "0:abc".into(),
                status: "Active".into(),
                native_raw: 1_000_000_000,
                ecc: vec![(2, 350_000_000_000)],
                code_hash: Some("cafe".into()),
            }),
            NoteGetterBalanceMaps {
                balance: NoteBalanceMap::Known(vec![(2, 100_000_000_000)]),
                locked_in_orders: NoteBalanceMap::Known(vec![]),
            },
        )
        .unwrap();
        let out = render_note_balance(&view);

        assert!(
            out.lines()
                .any(|line| line.starts_with("SHELL gas ECC[2]: ")),
            "account-level gas prefix missing: {out}"
        );
        assert!(
            out.contains(
                "PrivateNote.getDetails spendable token balance (trading money):\n  ECC[2] SHELL: "
            ),
            "spendable trading-money prefix missing: {out}"
        );
    }

    /// a configured nominal must not look like proof that the note is funded.
    #[test]
    fn note_balance_labels_live_shell_and_configured_nominal_unambiguously() {
        let view = build_note_balance_view(
            "0:abc",
            Some(NoteAccountSnapshot {
                address: "0:abc".into(),
                status: "Active".into(),
                native_raw: 7,
                ecc: vec![(2, 0)],
                code_hash: None,
            }),
            NoteGetterBalanceMaps {
                balance: NoteBalanceMap::Known(vec![(2, 10_000_000_000_000)]),
                locked_in_orders: NoteBalanceMap::Known(vec![]),
            },
        )
        .unwrap();

        let out = render_note_balance(&view);
        assert!(
            out.contains(
                "account ECC balances (deployment gas):\n  ECC[2] SHELL: 0.000000000 SHELL (raw 0)"
            ),
            "{out}"
        );
        assert!(
            out.contains("PrivateNote.getDetails spendable token balance (trading money):\n  ECC[2] SHELL: 10000.000000000 SHELL (raw 10000000000000)"),
            "{out}"
        );
        assert_eq!(out.matches("live spendable balance").count(), 0, "{out}");
        assert_eq!(out.matches("authoritative funding").count(), 0, "{out}");
    }

    /// negative: a null/unreadable account is not rendered as zero.
    #[test]
    fn note_balance_null_account_fails_loud() {
        let err = build_note_balance_view(
            "0:missing",
            None,
            unknown_note_getter_balance_maps("not queried"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("cannot read PrivateNote account"), "{err}");
        assert!(err.contains("refusing to report zero"), "{err}");
    }

    #[test]
    fn note_balance_zero_current_account_is_valid() {
        let view = build_note_balance_view(
            "0:zero",
            Some(NoteAccountSnapshot {
                address: "0:zero".into(),
                status: "Active".into(),
                native_raw: 0,
                ecc: Vec::new(),
                code_hash: Some("current".into()),
            }),
            note_getter_balance_maps(Some(&json!({
                "balance": {},
                "lockedInOrders": {}
            }))),
        )
        .unwrap();
        let out = render_note_balance(&view);
        assert!(
            out.contains("SHELL gas ECC[2]: 0.000000000 SHELL (raw 0)"),
            "{out}"
        );
        assert!(
            out.contains("VMSHELL native gas: 0.000000000 vmshell (raw 0)"),
            "{out}"
        );
        assert_eq!(out.matches("none reported").count(), 3, "{out}");
    }

    /// `getDetails` maps preserve unknown vs empty and parse known token maps.
    #[test]
    fn note_balance_getter_maps_preserve_unknown() {
        let maps = note_getter_balance_maps(None);
        assert!(matches!(maps.balance, NoteBalanceMap::Unknown(_)));
        assert!(matches!(maps.locked_in_orders, NoteBalanceMap::Unknown(_)));

        let maps = note_getter_balance_maps(Some(&json!({
            "balance": {"2": "3000000000", "7": "9"},
            "lockedInOrders": null
        })));
        assert_eq!(
            maps.balance,
            NoteBalanceMap::Known(vec![(2, 3_000_000_000), (7, 9)])
        );
        assert!(matches!(maps.locked_in_orders, NoteBalanceMap::Unknown(_)));

        let maps = note_getter_balance_maps(Some(&json!({
            "balance": {},
            "lockedInOrders": {}
        })));
        assert_eq!(maps.balance, NoteBalanceMap::Known(vec![]));
        assert_eq!(maps.locked_in_orders, NoteBalanceMap::Known(vec![]));
    }

    /// When a figure cannot be read at all, the balance report says so in words and gives the
    /// reason, instead of showing a zero or quietly showing a different figure in its place.

    /// A report is rendered for an account that does hold money, with both of the figures that
    /// have to be asked for separately made unreadable and a reason attached. Both must come out
    /// as the word "unknown" carrying that reason verbatim; neither may come out as "none
    /// reported", which is what a figure that was read and found empty says; and no figure
    /// anywhere in the report may be a zero, because that is the answer a reader would act on.

    /// The 4.0.33 semantics this encodes: `PrivateNote.getDetails().balance` is already the free
    /// record balance, so it is reported as read and never reduced by `lockedInOrders` a second
    /// time -- which is also why an unreadable one cannot be reconstructed from the pocket beside it.

    /// E2E-FUND-06, `tests/e2e/test-specification.md`.
    /// Partial/blocked: this proves getter labels and unknown handling only; whole-balance
    /// decomposition, component totals, and overflow behavior require separate evidence.
    /// E2E-ROW: E2E-FUND-06/L0
    #[test]
    fn fund06_an_unreadable_figure_is_reported_as_unknown_with_its_reason_never_as_zero() {
        let reason = "getDetails error: transport closed";
        let view = build_note_balance_view(
            "0:note",
            Some(NoteAccountSnapshot {
                address: "0:note".into(),
                status: "Active".into(),
                native_raw: 1,
                ecc: vec![(2, 100_000_000_000)],
                code_hash: Some("current".into()),
            }),
            unknown_note_getter_balance_maps(reason),
        )
        .unwrap();

        let out = render_note_balance(&view);
        assert_eq!(
            out.matches(&format!("unknown ({reason})")).count(),
            2,
            "both separately-asked figures must say unknown and carry the reason: {out}"
        );
        assert!(
            !out.contains("none reported"),
            "an unreadable figure must not be reported as an empty one: {out}"
        );
        assert!(
            !out.contains("(raw 0)"),
            "an unreadable figure must not be reported as zero: {out}"
        );
        assert!(
            out.lines()
                .all(|line| !line.trim_start().starts_with("ECC[2] SHELL: 0.")),
            "an unreadable figure must not be reported as zero: {out}"
        );
    }

    /// Two figures that have to be asked for separately fail separately: one of them being
    /// unreadable does not turn the other one, which was read perfectly well, into a shrug.

    /// The report is rendered twice, once with each of the two figures unreadable and the other
    /// present. In both directions the figure that was read must appear as its own exact number,
    /// and only the other one may say unknown, with the reason naming which figure failed. The
    /// same is then required when one figure is present but malformed rather than absent.

    /// The 4.0.33 semantics this encodes: `PrivateNote.getDetails().balance` is already the free
    /// record balance, so the two pockets are reported side by side as read and the free balance is
    /// never reduced by `lockedInOrders` a second time.

    /// E2E-FUND-06, `tests/e2e/test-specification.md`.
    /// Partial/blocked: this proves independent getter decoding only; whole-balance decomposition,
    /// component totals, and overflow behavior require separate evidence.
    /// E2E-ROW: E2E-FUND-06/L0
    #[test]
    fn fund06_the_two_separately_asked_figures_fail_independently() {
        let account = || NoteAccountSnapshot {
            address: "0:note".into(),
            status: "Active".into(),
            native_raw: 1,
            ecc: vec![(2, 7)],
            code_hash: Some("current".into()),
        };

        let locked_missing = note_getter_balance_maps(Some(&json!({
            "balance": {"2": "10000000000000"},
            "lockedInOrders": null
        })));
        assert_eq!(
            locked_missing.balance,
            NoteBalanceMap::Known(vec![(2, 10_000_000_000_000)])
        );
        let NoteBalanceMap::Unknown(reason) = &locked_missing.locked_in_orders else {
            panic!("the absent figure must be unknown");
        };
        assert!(reason.contains("lockedInOrders"), "{reason}");
        let out = render_note_balance(
            &build_note_balance_view("0:note", Some(account()), locked_missing).unwrap(),
        );
        assert!(
            out.contains("ECC[2] SHELL: 10000.000000000 SHELL (raw 10000000000000)"),
            "the figure that was read must still be shown in full: {out}"
        );
        assert_eq!(out.matches("unknown (").count(), 1, "{out}");

        let balance_missing = note_getter_balance_maps(Some(&json!({
            "lockedInOrders": {"2": "500"}
        })));
        assert_eq!(
            balance_missing.locked_in_orders,
            NoteBalanceMap::Known(vec![(2, 500)])
        );
        let NoteBalanceMap::Unknown(reason) = &balance_missing.balance else {
            panic!("the absent figure must be unknown");
        };
        assert!(reason.contains("balance"), "{reason}");
        let out = render_note_balance(
            &build_note_balance_view("0:note", Some(account()), balance_missing).unwrap(),
        );
        assert!(
            out.contains("ECC[2] SHELL: 0.000000500 SHELL (raw 500)"),
            "the figure that was read must still be shown in full: {out}"
        );
        assert_eq!(out.matches("unknown (").count(), 1, "{out}");

        let malformed = note_getter_balance_maps(Some(&json!({
            "balance": {"2": "not-a-number"},
            "lockedInOrders": {"2": "3"}
        })));
        assert!(
            matches!(&malformed.balance, NoteBalanceMap::Unknown(reason) if reason.contains("balance")),
            "a malformed figure must be unknown, not silently dropped: {:?}",
            malformed.balance
        );
        assert_eq!(
            malformed.locked_in_orders,
            NoteBalanceMap::Known(vec![(2, 3)]),
            "a malformed figure must not contaminate the one beside it"
        );
    }

    /// a fully deployed note state maps to the exact pool note schema the seller/buyer consume.
    #[test]
    fn pn_state_to_note_exact_schema() {
        let state = complete_state();
        let public = state.owner_public_key_hex.clone().unwrap();
        let secret = state.owner_secret_key_hex.clone().unwrap();
        let n = pn_state_to_pool_note(&state).unwrap();
        assert_eq!(n["address"], "0:abc");
        assert_eq!(n["deposit_identifier_hash"], "123");
        assert_eq!(n["owner_public_key_hex"].as_str(), Some(public.as_str()));
        assert_eq!(n["owner_secret_key_hex"].as_str(), Some(secret.as_str()));
        assert_eq!(n["deployed_at_unix"], 1000);
        assert_eq!(n["shell_funded"], true);
        assert_eq!(n["native_funded"], true);
    }

    /// (negatives): an incomplete deploy state fails loud -- never pooled.
    #[test]
    fn incomplete_onboard_fails_loud() {
        let mut s = complete_state();
        s.pn_address = None;
        assert!(pn_state_to_pool_note(&s)
            .unwrap_err()
            .to_string()
            .contains("pn_address"));
        let mut s = complete_state();
        s.shell_funded = false;
        assert!(pn_state_to_pool_note(&s)
            .unwrap_err()
            .to_string()
            .contains("not fully deployed"));
        let mut s = complete_state();
        s.sanity_checked = false;
        assert!(pn_state_to_pool_note(&s)
            .unwrap_err()
            .to_string()
            .contains("not fully deployed"));
    }

    #[test]
    fn non_shell_onboard_and_pool_fail_closed() {
        let mut state = complete_state();
        state.token_type = 1;
        let error = pn_state_to_pool_note(&state).unwrap_err().to_string();
        assert!(error.contains("require SHELL currency id 2"), "{error}");

        let stale_pool = json!({"token_type": 1, "notes": []});
        let error = ensure_shell_pool_currency(&stale_pool)
            .unwrap_err()
            .to_string();
        assert!(error.contains("require SHELL currency id 2"), "{error}");
    }

    #[test]
    fn recovery_rejects_non_shell_currency() {
        let mut state = complete_recovery_state();
        state.token_type = 1;
        let error = state.validate().unwrap_err().to_string();
        assert!(error.contains("require SHELL currency id 2"), "{error}");
    }

    /// regression: a pool entry whose stored secret cannot derive the recorded owner pubkey is
    /// rejected before the bad DEXDO_PN_POOL entry is serialized. Without this, later owner-signed writes fail
    /// opaquely with ERR_INVALID_SENDER 101.
    #[test]
    fn pn_state_to_note_rejects_owner_secret_public_mismatch() {
        let mut s = complete_state();
        s.owner_public_key_hex = Some("11".repeat(32));

        let err = pn_state_to_pool_note(&s).unwrap_err().to_string();

        assert!(err.contains("DEXDO_PN_POOL"), "{err}");
        assert!(err.contains("owner_secret_key_hex derives pubkey"), "{err}");
        assert!(err.contains("ERR_INVALID_SENDER 101"), "{err}");
        assert!(err.contains("--pool <new_file>"), "{err}");
    }

    /// regression: deploy must compare the freshly deployed PrivateNote's on-chain owner key
    /// (`getDetails().ephemeralPubkey`) against the saved pool key before writing the pool file.
    #[test]
    fn onchain_owner_check_rejects_mismatched_pool_key() {
        let derived = derive_owner_pubkey_from_secret_hex(&fixture_secret_hex()).unwrap();
        assert!(ensure_onchain_owner_matches_pool_key(
            "note deploy",
            "0:abc",
            Some(&format!("0x{}", derived.to_ascii_uppercase())),
            &derived,
        )
        .is_ok());

        let err = ensure_onchain_owner_matches_pool_key(
            "note deploy",
            "0:abc",
            Some(&format!("0x{}", "11".repeat(32))),
            &derived,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("_ephemeralPubkey"), "{err}");
        assert!(err.contains("provision/sell/withdraw"), "{err}");
        assert!(err.contains("ERR_INVALID_SENDER 101"), "{err}");
        assert!(err.contains("--pool <new_file>"), "{err}");
    }

    /// a fresh pool is created from the pn_state pool-level fields; a second note appends.
    #[test]
    fn pool_create_then_append() {
        let s = complete_state();
        let n1 = pn_state_to_pool_note(&s).unwrap();
        let wallet = format!("0:{}", "a".repeat(64));
        let pool = pool_with_note_added(None, &s, n1, 42, &wallet).unwrap();
        assert_eq!(pool["nominal"], "N100");
        assert_eq!(pool["raw_value_per_pn"], 100_000_000_000u64);
        // the WALLET keeps the form it was given, while the note is recorded canonically.
        // A note's DApp is a fact (`PrivateNote` lives in the dexdo DApp); a multisig's self-DApp
        // half is only reconstructed from its own account id, and storing that would record an
        // unverified claim -- and would break the account-only identity comparison this field
        // exists for.
        assert_eq!(pool["funding_multisig_address"], wallet);
        assert_eq!(pool["notes"].as_array().unwrap().len(), 1);

        let mut s2 = complete_state();
        s2.pn_address = Some("0:def".into());
        let n2 = pn_state_to_pool_note(&s2).unwrap();
        let pool = pool_with_note_added(Some(pool), &s2, n2, 43, &wallet).unwrap();
        // The append keeps what the pool already recorded -- it does not re-render it.
        assert_eq!(pool["funding_multisig_address"], wallet);
        assert_eq!(pool["notes"].as_array().unwrap().len(), 2);
    }

    /// residual: the pool entry itself carries the current TokenContract so buyer recovery/reclaim does not
    /// depend on a side manifest or scraped logs.
    #[test]
    fn pool_records_token_contract_next_to_note_entry() {
        let mut s = complete_state();
        s.pn_address = Some(format!("0:{}", "1".repeat(64)));
        let wallet = format!("0:{}", "a".repeat(64));
        let pool =
            pool_with_note_added(None, &s, pn_state_to_pool_note(&s).unwrap(), 1, &wallet).unwrap();
        let note_addr = s.pn_address.as_deref().unwrap();
        let tc = format!("0:{}", "b".repeat(64));

        let pool =
            pool_with_note_token_contract_recorded(pool, note_addr, &tc, "buyer", 99).unwrap();

        let note = &pool["notes"].as_array().unwrap()[0];
        // recorded in ITS canonical form, which for a per-deal TokenContract is the
        // self-DApp one -- the spelling this file's own refusals and `pool_note_recovery_records`
        // already print it in. Written as a literal rather than by calling the renderer: an
        // expectation stated through the function under test agrees with whatever that function
        // does, which is not an expectation.
        assert_eq!(
            note["token_contract"],
            format!("{}::{}", "b".repeat(64), "b".repeat(64))
        );
        assert_eq!(note["token_contract_role"], "buyer");
        assert_eq!(note["token_contract_updated_at_unix"], 99);
        let records = pool_note_recovery_records(&pool).unwrap();
        assert_eq!(records.len(), 1);
        // Destructured, not `assert_eq!`d: a new recorded field breaks this pattern exactly as it would
        // break a whole-value comparison, and nothing here can render the secret.
        let PoolNoteRecoveryRecord {
            note_addr: recorded_note_addr,
            owner_secret_hex,
            token_contract,
            role,
            recorded_at_unix,
        } = &records[0];
        assert_eq!(recorded_note_addr, note_addr);
        assert_eq!(
            owner_secret_hex.as_str(),
            s.owner_secret_key_hex.clone().unwrap().as_str()
        );
        assert_eq!(token_contract, &tc);
        assert_eq!(role, "buyer");
        assert_eq!(recorded_at_unix, &Some(99));
    }

    /// negative: do not silently claim recovery metadata was persisted if the active pool is not the note's
    /// pool.
    #[test]
    fn pool_token_contract_record_requires_matching_note() {
        let mut s = complete_state();
        s.pn_address = Some(format!("0:{}", "1".repeat(64)));
        let wallet = format!("0:{}", "a".repeat(64));
        let pool =
            pool_with_note_added(None, &s, pn_state_to_pool_note(&s).unwrap(), 1, &wallet).unwrap();
        let err = pool_with_note_token_contract_recorded(
            pool,
            &format!("0:{}", "c".repeat(64)),
            &format!("0:{}", "b".repeat(64)),
            "buyer",
            99,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no note entry"), "{err}");
    }

    #[test]
    fn pool_note_entry_preflight_requires_unique_note() {
        let mut s = complete_state();
        s.pn_address = Some(format!("0:{}", "1".repeat(64)));
        let wallet = format!("0:{}", "a".repeat(64));
        let pool =
            pool_with_note_added(None, &s, pn_state_to_pool_note(&s).unwrap(), 1, &wallet).unwrap();
        pool_has_unique_note_entry(&pool, s.pn_address.as_deref().unwrap()).unwrap();
        let err = pool_has_unique_note_entry(&pool, &format!("0:{}", "c".repeat(64)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no note entry"), "{err}");
    }

    /// (negatives): duplicate address + mixed nominal are refused.
    #[test]
    fn pool_refuses_duplicate_and_mixed() {
        let s = complete_state();
        let wallet = format!("0:{}", "a".repeat(64));
        let pool =
            pool_with_note_added(None, &s, pn_state_to_pool_note(&s).unwrap(), 1, &wallet).unwrap();
        // duplicate address
        let dup = pn_state_to_pool_note(&s).unwrap();
        assert!(
            pool_with_note_added(Some(pool.clone()), &s, dup, 2, &wallet)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        // mixed nominal
        let mut s2 = complete_state();
        s2.nominal = "N1000".into();
        s2.pn_address = Some("0:xyz".into());
        let n2 = pn_state_to_pool_note(&s2).unwrap();
        assert!(pool_with_note_added(Some(pool), &s2, n2, 3, &wallet)
            .unwrap_err()
            .to_string()
            .contains("homogeneous pool"));
    }

    /// rewards provenance is root-level and cannot silently mix funding multisigs or backfill legacy pools.
    #[test]
    fn pool_records_and_guards_funding_multisig_provenance() {
        let s = complete_state();
        let h1 = "1".repeat(64);
        let h2 = "B".repeat(64);
        let wallet_half_form = format!("{h1}::{h2}");
        let wallet = format!("0:{}", h2.to_ascii_lowercase());
        let other_wallet = format!("0:{}", "c".repeat(64));

        let pool = pool_with_note_added(
            None,
            &s,
            pn_state_to_pool_note(&s).unwrap(),
            1,
            &wallet_half_form,
        )
        .unwrap();
        assert_eq!(
            pool["funding_multisig_address"],
            format!("{h1}::{}", h2.to_ascii_lowercase())
        );

        let mut s2 = complete_state();
        s2.pn_address = Some("0:def".into());
        let err = pool_with_note_added(
            Some(pool.clone()),
            &s2,
            pn_state_to_pool_note(&s2).unwrap(),
            2,
            &other_wallet,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("funding_multisig_address"), "{err}");

        let mut legacy = pool;
        legacy
            .as_object_mut()
            .unwrap()
            .remove("funding_multisig_address");
        let err = pool_with_note_added(
            Some(legacy),
            &s2,
            pn_state_to_pool_note(&s2).unwrap(),
            3,
            &wallet,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown origin"), "{err}");
    }

    /// a supplied DApp half is persisted, not collapsed into the account-only chain
    /// parameter. Recovery and pool round-trips must therefore distinguish equal accounts that
    /// belong to different DApps.
    #[test]
    fn canonical_funding_identities_with_equal_accounts_persist_and_recover_distinctly() {
        let temp = tempfile::tempdir().expect("temp dir");
        let account = "c".repeat(64);
        let owner_secret = fixture_secret_hex();
        let owner_public = derive_owner_pubkey_from_secret_hex(&owner_secret).expect("owner key");
        let mut recovered_identities = Vec::new();
        let mut pool_identities = Vec::new();

        for (index, dapp) in ["a".repeat(64), "b".repeat(64)].into_iter().enumerate() {
            let supplied = format!(
                "{}::{}",
                dapp.to_ascii_uppercase(),
                account.to_ascii_uppercase()
            );
            let expected = format!("{dapp}::{account}");
            let state = NoteDeployRecoveryState::new(
                recovery_request("https://net-a.example", &supplied),
                &owner_public,
                &owner_secret,
            )
            .expect("canonical recovery state");
            assert_eq!(state.funding_multisig_address, expected);

            let recovery_path = temp.path().join(format!("canonical-{index}.recovery.json"));
            write_note_deploy_recovery(&recovery_path, &state).expect("persist recovery");
            let persisted: Value = serde_json::from_slice(
                &std::fs::read(&recovery_path).expect("read persisted recovery"),
            )
            .expect("parse persisted recovery");
            assert_eq!(persisted["funding_multisig_address"], expected);
            let recovered = load_note_deploy_recovery(&recovery_path)
                .expect("load recovery")
                .expect("recovery exists");
            assert_eq!(recovered.funding_multisig_address, expected);
            recovered_identities.push(recovered.funding_multisig_address);

            let state = complete_state();
            let pool = pool_with_note_added(
                None,
                &state,
                pn_state_to_pool_note(&state).expect("pool note"),
                1,
                &supplied,
            )
            .expect("persist pool identity");
            assert_eq!(pool["funding_multisig_address"], expected);
            pool_identities.push(
                pool["funding_multisig_address"]
                    .as_str()
                    .expect("pool funding identity")
                    .to_string(),
            );
        }

        assert_ne!(recovered_identities[0], recovered_identities[1]);
        assert_ne!(pool_identities[0], pool_identities[1]);
    }

    /// compatibility: this fixture is written by hand in the exact account-only shape used
    /// by the current binary. It is deliberately not produced through the writer under test.
    /// A recovery file written by a binary that still had the second gas voucher carries
    /// `ecc_shell_deposit`. This branch removed the field, and the operator's file on disk is not
    /// rewritten by that: `note deploy` resumes from whatever is there. So the record has to LOAD,
    /// validate and finish -- an unknown field is ignored, not a refusal that strands a deploy
    /// half-way with the wallet already spent.
    #[test]
    fn a_recovery_file_that_still_carries_the_dropped_field_loads_and_finishes() {
        let temp = tempfile::tempdir().expect("temp dir");
        let recovery_path = temp.path().join("with-dropped-field.recovery.json");
        let account = "a".repeat(64);
        let funding = format!("0:{account}");
        let owner_secret = fixture_secret_hex();
        let owner_public = derive_owner_pubkey_from_secret_hex(&owner_secret).expect("owner key");

        let written_by_the_old_binary = json!({
            "version": 1,
            "endpoint": "https://net-a.example",
            "nominal": "N100",
            "token_type": SHELL_CURRENCY_ID,
            "raw_value": 100_000_000_000u64,
            // The field this branch removed, exactly as the old binary wrote it.
            "ecc_shell_deposit": 100_000_000_000u64,
            "funding_multisig_address": funding,
            "owner_public_key_hex": owner_public,
            "owner_secret_key_hex": owner_secret,
            "pn_address": format!("0:{}", "d".repeat(64)),
            "deposit_identifier_hash": "f".repeat(64),
            "deployed_at_unix": 1234,
            "deposit_voucher": null,
            "shell_voucher": null,
            "shell_funded": true,
            "sanity_checked": true
        });
        crate::cli::support::write_owner_only_key_fixture(
            &recovery_path,
            &serde_json::to_string_pretty(&written_by_the_old_binary).expect("serialize"),
        );

        let recovery = load_note_deploy_recovery(&recovery_path)
            .expect("a record with the dropped field still loads")
            .expect("the record exists");
        recovery.validate().expect("it validates");
        recovery
            .ensure_matches_request(recovery_request(
                "https://net-a.example",
                &format!("{}::{account}", "9".repeat(64)),
            ))
            .expect("it matches the same funding account");
        recovery
            .ensure_ready_for_pool()
            .expect("it can still be folded into the pool");
    }

    #[test]
    fn legacy_recovery_and_pool_files_load_validate_and_remain_usable_for_deploy_fold() {
        let temp = tempfile::tempdir().expect("temp dir");
        let recovery_path = temp.path().join("legacy.recovery.json");
        let pool_path = temp.path().join("legacy.pool.json");
        let account = "a".repeat(64);
        let legacy_funding = format!("0:{account}");
        assert_eq!(legacy_funding.len(), 66);
        assert!(!legacy_funding.contains("::"));

        let owner_secret = fixture_secret_hex();
        let owner_public = derive_owner_pubkey_from_secret_hex(&owner_secret).expect("owner key");
        let new_note_address = format!("0:{}", "d".repeat(64));
        let legacy_recovery = json!({
            "version": 1,
            "endpoint": "https://net-a.example",
            "nominal": "N100",
            "token_type": SHELL_CURRENCY_ID,
            "raw_value": 100_000_000_000u64,
            "funding_multisig_address": legacy_funding,
            "owner_public_key_hex": owner_public,
            "owner_secret_key_hex": owner_secret,
            "pn_address": new_note_address,
            "deposit_identifier_hash": "f".repeat(64),
            "deployed_at_unix": 1234,
            "deposit_voucher": null,
            "shell_voucher": null,
            "shell_funded": true,
            "sanity_checked": true
        });
        crate::cli::support::write_owner_only_key_fixture(
            &recovery_path,
            &serde_json::to_string_pretty(&legacy_recovery).expect("serialize hand-built recovery"),
        );

        let recovery = load_note_deploy_recovery(&recovery_path)
            .expect("load legacy recovery")
            .expect("legacy recovery exists");
        recovery.validate().expect("validate legacy recovery");
        let supplied_canonical = format!("{}::{account}", "9".repeat(64));
        recovery
            .ensure_matches_request(recovery_request(
                "https://net-a.example",
                &supplied_canonical,
            ))
            .expect("legacy recovery matches the same funding account");
        recovery
            .ensure_ready_for_pool()
            .expect("legacy recovery ready");

        let legacy_pool = json!({
            "endpoint": "https://net-a.example",
            "created_at_unix": 1000,
            "nominal": "N100",
            "token_type": SHELL_CURRENCY_ID,
            "raw_value_per_pn": 100_000_000_000u64,
            "funding_multisig_address": legacy_funding,
            "notes": [{
                "address": format!("0:{}", "e".repeat(64)),
                "deposit_identifier_hash": "1".repeat(64),
                "owner_public_key_hex": "2".repeat(64),
                "owner_secret_key_hex": "3".repeat(64),
                "deployed_at_unix": 1000,
                "shell_funded": true,
                "native_funded": true
            }]
        });
        crate::cli::support::write_owner_only_key_fixture(
            &pool_path,
            &serde_json::to_string_pretty(&legacy_pool).expect("serialize hand-built pool"),
        );
        let existing_pool: Value = serde_json::from_slice(
            &std::fs::read(&pool_path).expect("load hand-built legacy pool file"),
        )
        .expect("parse legacy pool");
        let onboard = recovery.to_onboard_state().expect("recover deploy state");
        let note = pn_state_to_pool_note(&onboard).expect("recover pool note");
        let updated = pool_with_note_added(
            Some(existing_pool),
            &onboard,
            note,
            1235,
            &recovery.funding_multisig_address,
        )
        .expect("legacy pool remains usable by the deploy fold");

        assert_eq!(updated["funding_multisig_address"], legacy_funding);
        assert_eq!(updated["notes"].as_array().expect("pool notes").len(), 2);
    }

    /// the funding wallet seed phrase is an input-only credential. The pool stores only the deployed note
    /// material the runtime consumes; seed words must not appear in serialized pool output.
    #[test]
    fn pool_output_does_not_contain_seed_words() {
        let phrase = tvm_tonos_fixture_phrase();
        let derived =
            dexdo::wallet_seed::derive_multisig_private_key_from_seed_phrase(&phrase).unwrap();
        let mut state = complete_state();
        state.owner_public_key_hex = Some(derived.public_hex().to_string());
        state.owner_secret_key_hex = Some(derived.secret_hex().to_string().into());
        let wallet = format!("0:{}", "a".repeat(64));
        let pool = pool_with_note_added(
            None,
            &state,
            pn_state_to_pool_note(&state).unwrap(),
            1,
            &wallet,
        )
        .unwrap();
        let json = serde_json::to_string(&pool).unwrap();
        for word in phrase.split_whitespace() {
            assert!(!json.contains(word), "pool output contains a seed word");
        }
    }

    /// the recovery file is the durable owner-key copy and must be private, atomic JSON.
    #[test]
    fn recovery_file_writes_owner_key_with_private_mode() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-test");
        let path = dir.join("pn_pool.json.recovery.json");
        let state = NoteDeployRecoveryState::new(
            recovery_request("https://net-a.example", &format!("0:{}", "a".repeat(64))),
            &derive_owner_pubkey_from_secret_hex(&fixture_secret_hex()).unwrap(),
            &fixture_secret_hex(),
        )
        .unwrap();

        write_note_deploy_recovery(&path, &state).unwrap();
        let loaded = load_note_deploy_recovery(&path).unwrap().unwrap();

        assert_eq!(loaded.owner_secret_key_hex.as_str(), fixture_secret_hex());
        assert_eq!(loaded.pn_address, None);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "recovery file must be 0600");
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_atomic_replacement_preserves_0600_and_complete_content() {
        use std::os::unix::fs::PermissionsExt as _;

        let (dir, _cleanup) = temp_dir("dexdo-private-atomic-replacement-test");
        let path = dir.join("pn_pool.json");
        write_private_atomic(&path, b"first").unwrap();
        write_private_atomic(&path, b"replacement-content-with-complete-tail").unwrap();

        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"replacement-content-with-complete-tail"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "replacement secret file must remain 0600");
    }

    /// Public regression: a successful deploy may replace an owner-only stale attempt because no wallet
    /// spend or live note can depend on that old key.
    #[test]
    fn successful_deploy_refreshes_stale_unspent_recovery_owner() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-refresh-test");
        let path = dir.join("pn_pool.json.recovery.json");
        let stale = recovery_state_for_owner(&"31".repeat(32), None);
        let successful =
            recovery_state_for_owner(&"42".repeat(32), Some(&format!("0:{}", "2".repeat(64))));
        write_note_deploy_recovery(&path, &stale).unwrap();

        refresh_note_deploy_recovery_after_success(&path, &successful).unwrap();
        let loaded = load_note_deploy_recovery(&path).unwrap().unwrap();

        assert_eq!(loaded.owner_public_key_hex, successful.owner_public_key_hex);
        assert_eq!(loaded.owner_secret_key_hex, successful.owner_secret_key_hex);
        assert_eq!(loaded.pn_address, successful.pn_address);
        assert_eq!(
            loaded.deposit_identifier_hash,
            successful.deposit_identifier_hash
        );
    }

    /// Public happy path: final success writes the deployed note recovery when no prior file exists.
    #[test]
    fn successful_deploy_writes_recovery_when_path_is_absent() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-success-test");
        let path = dir.join("pn_pool.json.recovery.json");
        let successful =
            recovery_state_for_owner(&"42".repeat(32), Some(&format!("0:{}", "2".repeat(64))));

        refresh_note_deploy_recovery_after_success(&path, &successful).unwrap();
        let loaded = load_note_deploy_recovery(&path).unwrap().unwrap();

        assert_eq!(loaded.owner_secret_key_hex, successful.owner_secret_key_hex);
        assert_eq!(loaded.pn_address, successful.pn_address);
    }

    /// Public money-safety: a recovery path that already holds another deployed note is never clobbered.
    #[test]
    fn successful_deploy_refuses_different_live_note_without_clobber() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-live-key-test");
        let path = dir.join("pn_pool.json.recovery.json");
        let existing =
            recovery_state_for_owner(&"31".repeat(32), Some(&format!("0:{}", "1".repeat(64))));
        let successful =
            recovery_state_for_owner(&"42".repeat(32), Some(&format!("0:{}", "2".repeat(64))));
        write_note_deploy_recovery(&path, &existing).unwrap();
        let before = std::fs::read(&path).unwrap();

        let err = refresh_note_deploy_recovery_after_success(&path, &successful)
            .unwrap_err()
            .to_string();

        assert!(err.contains("different deployed PrivateNote"), "{err}");
        assert!(err.contains("refusing to clobber"), "{err}");
        assert!(err.contains("--recovery <different-file>"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let loaded = load_note_deploy_recovery(&path).unwrap().unwrap();
        assert_eq!(loaded.owner_secret_key_hex, existing.owner_secret_key_hex);
        assert_eq!(loaded.pn_address, existing.pn_address);
    }

    /// Public money-safety: an address-less state can still carry an uncertain wallet spend and must not be
    /// treated as an unspent stale attempt.
    #[test]
    fn successful_deploy_refuses_different_owner_with_uncertain_spend() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-pending-spend-test");
        let path = dir.join("pn_pool.json.recovery.json");
        let mut existing = recovery_state_for_owner(&"31".repeat(32), None);
        let mut voucher = NoteDeployVoucherCheckpoint::new(
            &existing.owner_public_key_hex,
            existing.token_type,
            existing.raw_value,
            false,
            "51".repeat(32),
            "61".repeat(32),
        )
        .unwrap();
        voucher.submit_maybe_sent = true;
        existing
            .set_voucher_checkpoint(NoteDeployVoucherKind::Deposit, voucher)
            .unwrap();
        let successful =
            recovery_state_for_owner(&"42".repeat(32), Some(&format!("0:{}", "2".repeat(64))));
        write_note_deploy_recovery(&path, &existing).unwrap();
        let before = std::fs::read(&path).unwrap();

        let err = refresh_note_deploy_recovery_after_success(&path, &successful)
            .unwrap_err()
            .to_string();

        assert!(
            err.contains("possible wallet-spend recovery material"),
            "{err}"
        );
        assert!(err.contains("--recovery <different-file>"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// Public load-time safety: recovery must refuse a target note whose on-chain owner is not the saved key.
    #[test]
    fn loaded_recovery_refuses_target_note_owner_mismatch() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-owner-check-test");
        let path = dir.join("pn_pool.json.recovery.json");
        let state =
            recovery_state_for_owner(&"42".repeat(32), Some(&format!("0:{}", "2".repeat(64))));
        write_note_deploy_recovery(&path, &state).unwrap();
        let before = std::fs::read(&path).unwrap();
        let loaded = load_note_deploy_recovery(&path).unwrap().unwrap();

        let err = ensure_recovery_owner_matches_target_note(
            &path,
            &loaded,
            Some(&format!("0x{}", "99".repeat(32))),
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("does not own target PrivateNote"), "{err}");
        assert!(err.contains("pass the recovery file"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    /// regression: recovery read, write, and cleanup use one canonical target, leaving no secret-bearing
    /// target behind when the CLI was given a symlink alias.
    #[cfg(unix)]
    #[test]
    fn recovery_symlink_resolves_once_for_read_write_and_cleanup() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-symlink-test");
        let target = dir.join("recovery-target.json");
        let alias = dir.join("recovery-alias.json");
        let mut state = complete_recovery_state();
        write_note_deploy_recovery(&target, &state).unwrap();
        std::os::unix::fs::symlink(&target, &alias).unwrap();

        let resolved = resolve_private_file_path(&alias, "--recovery").unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&target).unwrap());
        let loaded = load_note_deploy_recovery(&resolved).unwrap().unwrap();
        assert_eq!(loaded.owner_secret_key_hex, state.owner_secret_key_hex);

        state.sanity_checked = false;
        write_note_deploy_recovery(&resolved, &state).unwrap();
        assert!(std::fs::symlink_metadata(&alias)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(
            !load_note_deploy_recovery(&target)
                .unwrap()
                .unwrap()
                .sanity_checked
        );

        std::fs::remove_file(&resolved).unwrap();
        assert!(!target.exists());
        assert!(std::fs::symlink_metadata(&alias)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// negative regression: a recovery target that resolves to a directory is rejected before use.
    #[test]
    fn recovery_non_regular_target_is_rejected() {
        let (dir, _cleanup) = temp_dir("dexdo-note-recovery-nonregular-test");
        let sentinel = dir.join("recovery-directory");
        std::fs::create_dir(&sentinel).unwrap();

        let err = resolve_private_file_path(&sentinel, "--recovery")
            .unwrap_err()
            .to_string();
        assert!(err.contains("regular file"), "{err}");
    }

    /// recovery state contains note recovery material, but never the funding wallet secret.
    #[test]
    fn recovery_contents_exclude_funding_wallet_secret() {
        let state = complete_recovery_state();
        let wallet_secret = "f1".repeat(32);
        let json = serde_json::to_string_pretty(&state).unwrap();

        assert!(json.contains("owner_secret_key_hex"), "{json}");
        assert!(json.contains(state.owner_secret_key_hex.as_str()), "{json}");
        assert!(
            !json.contains(&wallet_secret),
            "wallet secret leaked into recovery JSON"
        );
        assert!(
            !json.contains("multisig_private_key") && !json.contains("multisig_seed"),
            "recovery JSON must not serialize funding wallet credential fields: {json}"
        );
    }

    /// a complete recovery state can rebuild the exact pool entry without wallet credentials/spend.
    #[test]
    fn recovery_state_finalizes_pool_entry_without_wallet_secret() {
        let state = complete_recovery_state();
        state.ensure_ready_for_pool().unwrap();
        let onboard = state.to_onboard_state().unwrap();
        let note = pn_state_to_pool_note(&onboard).unwrap();
        let pool =
            pool_with_note_added(None, &onboard, note, 1234, &state.funding_multisig_address)
                .unwrap();

        assert_eq!(pool["notes"].as_array().unwrap().len(), 1);
        assert_eq!(pool["notes"][0]["address"], state.pn_address.unwrap());
        assert_eq!(
            pool["notes"][0]["owner_secret_key_hex"],
            state.owner_secret_key_hex.as_str()
        );
    }

    /// negative: owner-key-only recovery is useful for resume, but not enough to write a pool entry.
    #[test]
    fn incomplete_recovery_refuses_finalize_with_clear_message() {
        let mut state = complete_recovery_state();
        state.pn_address = None;
        state.deposit_identifier_hash = None;
        state.shell_funded = false;
        state.sanity_checked = false;

        let err = state.ensure_ready_for_pool().unwrap_err().to_string();

        assert!(err.contains("owner key"), "{err}");
        assert!(err.contains("no deployed PrivateNote address"), "{err}");
        assert!(err.contains("`dexdo note deploy`"), "{err}");
        assert!(err.contains("resumes from this file"), "{err}");
        assert!(
            !err.contains(state.owner_secret_key_hex.as_str()),
            "secret leaked in error: {err}"
        );
    }

    /// regression: voucher-level recovery may contain a wallet-submitted deposit voucher, but without a
    /// deployed PrivateNote it must resume through `note deploy`, not be folded into a pool.
    #[test]
    fn voucher_submitted_recovery_refuses_pool_finalize_without_note_deploy() {
        let mut state = complete_recovery_state();
        state.pn_address = None;
        state.deposit_identifier_hash = None;
        state.shell_funded = false;
        state.sanity_checked = false;
        let mut voucher = NoteDeployVoucherCheckpoint::new(
            &state.owner_public_key_hex,
            state.token_type,
            state.raw_value,
            false,
            "11".repeat(32),
            "22".repeat(32),
        )
        .unwrap();
        voucher.submit_maybe_sent = true;
        state
            .set_voucher_checkpoint(NoteDeployVoucherKind::Deposit, voucher)
            .unwrap();

        let err = state.ensure_ready_for_pool().unwrap_err().to_string();
        let json = serde_json::to_string_pretty(&state).unwrap();

        assert!(err.contains("no deployed PrivateNote address"), "{err}");
        assert!(json.contains("\"deposit_voucher\""), "{json}");
        assert!(json.contains("\"submit_maybe_sent\": true"), "{json}");
        assert!(
            !err.contains(state.deposit_voucher.as_ref().unwrap().sk_u_hex.as_str()),
            "voucher secret leaked in error: {err}"
        );
    }

    /// regression: voucher checkpoints serialize the recovery material required to avoid a second wallet
    /// spend, but never serialize the funding-wallet credential names or values.
    #[test]
    fn recovery_contents_include_voucher_checkpoint_without_wallet_secret() {
        let mut state = complete_recovery_state();
        let wallet_secret = "f1".repeat(32);
        let mut voucher = NoteDeployVoucherCheckpoint::new(
            &state.owner_public_key_hex,
            state.token_type,
            state.raw_value,
            false,
            "33".repeat(32),
            "44".repeat(32),
        )
        .unwrap();
        voucher.submit_maybe_sent = true;
        voucher.event = Some(NoteDeployVoucherEvent {
            id: "event-id".to_string(),
            boc: "boc".to_string(),
            body: "body".to_string(),
            dst: ":0000000000000000000000000000000000000000000000000000000000000087".to_string(),
            created_at: 1234,
            block_id: Some("block".to_string()),
        });
        state
            .set_voucher_checkpoint(NoteDeployVoucherKind::Deposit, voucher)
            .unwrap();

        let json = serde_json::to_string_pretty(&state).unwrap();

        assert!(json.contains("\"sk_u_hex\""), "{json}");
        assert!(json.contains("\"sk_u_commit_hex\""), "{json}");
        assert!(json.contains("\"event\""), "{json}");
        assert!(
            !json.contains(&wallet_secret),
            "wallet secret leaked: {json}"
        );
        assert!(
            !json.contains("multisig_private_key") && !json.contains("multisig_seed"),
            "wallet credential field leaked: {json}"
        );
    }

    /// negative: an existing recovery file is tied to the deploy request and cannot be silently reused.
    #[test]
    fn recovery_rejects_mismatched_request() {
        let state = complete_recovery_state();

        let err = state
            .ensure_matches_request(recovery_request(
                "https://other-chain.example",
                &state.funding_multisig_address,
            ))
            .unwrap_err()
            .to_string();

        assert!(err.contains("does not match this deploy request"), "{err}");
        assert!(err.contains("fresh --pool/--recovery"), "{err}");
        assert!(
            !err.contains(state.owner_secret_key_hex.as_str()),
            "secret leaked in error: {err}"
        );
    }

    /// this message is printed both when this run created the recovery file and when it
    /// loaded one an earlier run left, so it must be true in both. It must not tell the operator to
    /// add `--recovery` -- that path is already this run's, and pasting the flag twice is rejected
    /// by clap -- must not hardcode a pool file name, and must hand over no argument-carrying
    /// command line at all, since a resume reuses inputs this message does not know.

    /// carried forward, deliberately: "names the next command and the file" is a **separate**
    /// requirement from "carries no key material", and the two are asserted separately here and in
    /// `recovery_guidance_built_from_state_never_carries_key_material`. A message that degraded to
    /// silence would satisfy the secret check perfectly. The command-span sweep below does not
    /// cover it either: it asserts that every span it finds is a bare command path, so a message
    /// that dropped the `dexdo note recover` half entirely would still hand it the `note deploy`
    /// span and pass. So both are named outright.
    #[test]
    fn recovery_owner_key_message_is_valid_in_both_states_it_is_printed_in() {
        use crate::cli::support::printed_commands::{
            classify, runs, top_level_subcommands, PrintedRun,
        };
        let path = std::path::Path::new("/tmp/pn pool/pn_pool.json.recovery.json");
        let msg = recovery_owner_key_written_message(path);

        assert!(
            msg.contains("re-run this same `dexdo note deploy` command unchanged"),
            "{msg}"
        );
        assert!(!msg.contains("added"), "{msg}");
        assert!(
            !msg.contains("pn_pool.json is missing"),
            "the pool file name is arbitrary: {msg}"
        );
        // The operator must still be told which command finalizes an already-finalized recovery,
        // and which file to hand it. The path is asserted as the whole path this run wrote, not as
        // a bare file name, so a message that stopped interpolating it cannot pass on a fixture
        // name that happens to appear elsewhere in the text.
        assert!(
            msg.contains("`dexdo note recover`"),
            "the message must still name the command that finalizes the recovery: {msg}"
        );
        assert!(
            msg.contains(&path.display().to_string()),
            "the message must still name the recovery file the operator has to act on: {msg}"
        );
        let found = runs(&msg, &top_level_subcommands());
        assert!(!found.is_empty(), "{msg}");
        for run in found {
            assert_eq!(
                classify(&run.text),
                Ok(PrintedRun::Reference),
                "this message names commands, it does not hand over a line to run: `{}`",
                run.text
            );
        }
    }

    /// the one `note recover` line the CLI prints complete. A recovery or pool path with a
    /// space in it must reach the parser as one argument, which is only visible if the rendered
    /// line is split the way the operator's shell would split it.
    #[test]
    fn printed_note_recover_line_survives_paths_a_shell_would_split() {
        use crate::cli::support::printed_commands::shell_split;
        use clap::Parser as _;
        let recovery = std::path::Path::new("/tmp/pn pool/it's.recovery.json");
        let pool = std::path::Path::new("/tmp/pn pool/pn_pool.json");
        let line = note_recover_finalize_command(recovery, pool);
        let argv = shell_split(&line).expect("the printed recover line must survive a shell");
        assert_eq!(
            argv,
            vec![
                "dexdo",
                "note",
                "recover",
                "--recovery",
                "/tmp/pn pool/it's.recovery.json",
                "--pool",
                "/tmp/pn pool/pn_pool.json",
            ],
            "{line}"
        );
        let parsed = crate::Cli::try_parse_from(&argv)
            .unwrap_or_else(|e| panic!("the printed line must parse: {line}\n{e}"));
        let crate::Command::Note(args) = parsed.command else {
            panic!("note command");
        };
        let crate::cli::args::NoteCommand::Recover(args) = args.command else {
            panic!("note recover command");
        };
        assert_eq!(args.recovery, recovery);
        assert_eq!(args.pool.as_deref(), Some(pool));
    }

    /// A maximal run of hex digits at least `len` long: the shape a note owner secret has in a
    /// pool file, which is bare 64-hex with no `0x` prefix to grep for.
    fn contains_hex_run(text: &str, len: usize) -> bool {
        let mut run = 0usize;
        for c in text.chars() {
            run = if c.is_ascii_hexdigit() { run + 1 } else { 0 };
            if run >= len {
                return true;
            }
        }
        false
    }

    /// the guidance a user sees once a recovery file exists must never carry key
    /// material -- and the state that holds the secret has to reach the function under test for
    /// that to mean anything. The earlier version of this test built a state with a secret and
    /// then called a message function that takes only a path, so the secret had no route into the
    /// assertion and the test could not fail. Every message below is derived from the state
    /// itself, and each is rejected both for the exact secret and for any 64-hex run, because a
    /// pool note stores `owner_secret_key_hex` bare.
    #[test]
    fn recovery_guidance_built_from_state_never_carries_key_material() {
        let complete = complete_recovery_state();
        let secret = complete.owner_secret_key_hex.as_str().to_string();
        let public = complete.owner_public_key_hex.clone();
        assert_eq!(
            secret.len(),
            64,
            "the fixture must carry a real bare-hex secret or this proves nothing"
        );
        assert!(contains_hex_run(&secret, 64));

        let mut unfinished = complete_recovery_state();
        unfinished.pn_address = None;
        unfinished.deposit_identifier_hash = None;
        unfinished.shell_funded = false;
        unfinished.sanity_checked = false;

        let mut unfunded = complete_recovery_state();
        unfunded.shell_funded = false;

        let messages = vec![
            unfinished
                .ensure_ready_for_pool()
                .expect_err("owner-key-only recovery cannot be pooled")
                .to_string(),
            unfunded
                .ensure_ready_for_pool()
                .expect_err("an unfunded recovery cannot be pooled")
                .to_string(),
            complete
                .ensure_matches_request(NoteDeployRecoveryRequest {
                    endpoint: "https://other.example",
                    nominal: &complete.nominal,
                    token_type: complete.token_type,
                    raw_value: complete.raw_value,
                    funding_multisig_address: &complete.funding_multisig_address,
                })
                .expect_err("a mismatched request is refused")
                .to_string(),
            complete
                .to_onboard_state()
                .map(|_| String::new())
                .unwrap_or_else(|e| e.to_string()),
            recovery_owner_key_written_message(std::path::Path::new("pn_pool.json.recovery.json")),
        ];

        for msg in messages {
            assert!(!msg.contains(&secret), "secret leaked in guidance: {msg}");
            assert!(
                !msg.contains(&public),
                "owner key material leaked in guidance: {msg}"
            );
            assert!(
                !contains_hex_run(&msg, 64),
                "a 64-hex run reached user-facing guidance: {msg}"
            );
        }
    }
}

/// Stage ONE of the `note wallet` recipe is flat deploy gas, sized from a live deploy receipt.

/// The defect these pin: stage one demanded `nominal + GAS_DEPOSIT` in NATIVE vmshell -- 350 SHELL
/// on N100, 1_000_250 on N1000000 -- and SHELL that lands as native can never be spent as currency
/// again. On mainnet that is a permanent loss of a nominal-sized
/// amount of real money, for a deploy the nominal has nothing to do with.

/// The owner's ruling is the specification: gas is needed strictly only for the deploy, and
/// subsequent operations spend SHELL, converting it as needed. So stage one is the deploy plus the
/// sends `note deploy` makes from this wallet, and stops there.

/// Its own module rather than a row in `tests` above: these assert a money figure and its
/// derivation, not the note schema everything there is about.
#[cfg(test)]
mod stage_one_native_is_flat_deploy_gas {
    use super::{
        note_deploy_voucher_wire_raw, operator_wallet_funding_raw,
        operator_wallet_predeploy_native_raw, NoteNominal, ROOT_PN_GAS_DEPOSIT_RAW,
    };
    use dexdo_core::params::{
        NOTE_DEPLOY_SUBMIT_NATIVE_VALUE, OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE,
    };
    use dexdo_core::private_note::proof::TokenType;

    /// Measured live on the test chain, 2026-08-12. `live_1173_operator_wallet_funds_from_an_ordinary_wallet` and
    /// `live_961_operator_wallet_deploys_after_external_funding` each read this balance at Uninit,
    /// this one at Active, and this one after the first inbound ECC[2] transfer -- two fresh
    /// addresses, two different funding routes, identical to the raw unit, and each test asserts
    /// exactly one transaction between consecutive reads. Written as the readings rather than as
    /// their differences so what this module states is the receipt.
    const PREDEPLOY_RAW: u128 = 1_250_000_000_000;
    const AFTER_DEPLOY_RAW: u128 = 1_249_846_499_000;
    const AFTER_FIRST_INBOUND_RAW: u128 = 1_249_843_778_000;

    /// What ONE canonical operator-wallet deploy transaction cost.
    const MEASURED_DEPLOY_COST_RAW: u128 = PREDEPLOY_RAW - AFTER_DEPLOY_RAW;

    /// The sends `note deploy` makes FROM this wallet: the deposit voucher (`isFee = false`) and the
    /// SHELL gas voucher (`isFee = true`), both through `note_deploy_build_voucher_submit_boc`.
    const NOTE_DEPLOY_WALLET_SUBMITS: u128 = 2;

    /// Counting the deploy a second way, against a figure recorded before an intermediate read
    /// existed to separate the deploy from the message that followed it.

    /// `.claude/skills/dexdo-sell-model-for-agent/SKILL.md` states that "deploying from a 1,250 SHELL
    /// predeploy balance consumed `156 222 000` raw native". That is the deploy PLUS the first
    /// inbound ECC[2] transfer, and it decomposes into the two readings above exactly -- which is
    /// what makes the smaller half safe to build a shipped constant on. A cost that had drifted
    /// would break this identity rather than quietly move the figure with it.
    const RECORDED_DEPLOY_PLUS_FIRST_INBOUND_RAW: u128 = 156_222_000;
    const _: () = assert!(
        MEASURED_DEPLOY_COST_RAW + (AFTER_DEPLOY_RAW - AFTER_FIRST_INBOUND_RAW)
            == RECORDED_DEPLOY_PLUS_FIRST_INBOUND_RAW
    );
    const _: () = assert!(MEASURED_DEPLOY_COST_RAW < RECORDED_DEPLOY_PLUS_FIRST_INBOUND_RAW);

    /// A wallet that deploys and then cannot send is useless, so the stage-one figure must cover the
    /// deploy AND the wallet's own note-deploy submits -- each of which costs the attached
    /// [`NOTE_DEPLOY_SUBMIT_NATIVE_VALUE`] plus its own transaction fee, bounded above by the
    /// measured deploy (a `submitTransaction` installs no state-init, runs no constructor and grows
    /// no code cell, so it cannot cost more than the transaction that does all three).
    fn measured_budget_raw() -> u128 {
        MEASURED_DEPLOY_COST_RAW
            + NOTE_DEPLOY_WALLET_SUBMITS * NOTE_DEPLOY_SUBMIT_NATIVE_VALUE
            + NOTE_DEPLOY_WALLET_SUBMITS * MEASURED_DEPLOY_COST_RAW
    }

    #[test]
    fn stage_one_covers_the_measured_deploy_and_the_wallets_own_sends() {
        let budget = measured_budget_raw();
        assert!(
            OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE >= budget,
            "stage one funds {OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE} raw native, but the measured \
             budget is {budget} raw: one deploy at {MEASURED_DEPLOY_COST_RAW} plus \
             {NOTE_DEPLOY_WALLET_SUBMITS} note-deploy submits, each attaching \
             {NOTE_DEPLOY_SUBMIT_NATIVE_VALUE} and paying at most one deploy in fees. A wallet that \
             deploys and then cannot send its vouchers is useless"
        );
        assert!(
            OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE < 2 * budget,
            "stage one funds {OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE} raw native against a measured \
             budget of {budget} raw. Native vmshell is gas and is never spendable as currency \
             again, so anything beyond a rounding margin is money burned for nothing"
        );
    }

    /// The exact shape of the defect: the nominal must not reach this figure. The compile-time half
    /// is that the function takes no nominal at all; this is the value half, and it holds for the
    /// SMALLEST nominal, so no nominal can satisfy it.
    #[test]
    fn no_nominal_reaches_the_stage_one_figure() {
        let decimals = TokenType::Shell.decimals();
        let stage_one = operator_wallet_predeploy_native_raw();
        assert_eq!(
            stage_one, OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE,
            "stage one must be the canonical parameter, not a second copy of it"
        );
        for nominal in NoteNominal::ALL {
            let nominal_scaled = note_deploy_voucher_wire_raw(false, nominal.raw_value(decimals));
            assert!(
                stage_one < nominal_scaled,
                "{}: stage one is {stage_one} raw but the nominal-scaled figure it replaced is \
                 {nominal_scaled} raw; stage one is permanent gas and must not follow the nominal",
                nominal.label()
            );
            assert!(
                stage_one < operator_wallet_funding_raw(nominal),
                "{}: stage one {stage_one} raw must stay below the stage-two ECC[2] requirement, \
                 which is the stage the nominal belongs to",
                nominal.label()
            );
        }
    }

    /// The recipe prints this figure in whole SHELL by integer division, and the old stage one was
    /// large enough for that to be invisible. This one sits on the unit boundary: a value that is
    /// not a whole number of SHELL would instruct the user to send LESS than the deploy needs, and a
    /// value under one SHELL would instruct them to send nothing at all.
    #[test]
    fn stage_one_is_a_whole_number_of_shell_so_the_printed_recipe_cannot_understate_it() {
        let decimals = u128::from(TokenType::Shell.decimals());
        assert!(
            OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE >= decimals
                && OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE.is_multiple_of(decimals),
            "stage one is {OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE} raw, which is not a whole number \
             of SHELL ({decimals} raw each); the funding recipe prints whole SHELL, so the user \
             would be told to send less than the deploy requires"
        );
    }

    /// A bound from the contracts rather than from taste: the gas a user burns forever to stand the
    /// wallet up must be smaller than the smallest money item the deploy flow moves, which is
    /// `RootPN.GAS_DEPOSIT`. The old stage one exceeded it for every nominal, by construction.
    #[test]
    fn stage_one_is_below_the_smallest_money_item_in_the_deploy_flow() {
        assert!(
            OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE < u128::from(ROOT_PN_GAS_DEPOSIT_RAW),
            "stage one burns {OPERATOR_WALLET_PREDEPLOY_NATIVE_VALUE} raw into gas permanently, \
             which is not below RootPN.GAS_DEPOSIT ({ROOT_PN_GAS_DEPOSIT_RAW} raw), the smallest \
             amount this flow moves as money"
        );
    }
}

/// PR1276 review, amended by: a pool write records the CANONICAL form, and never
/// re-renders an entry it was not asked to write.

/// The original rule was "keep the form you were given", everywhere. It was drawn from a real
/// incident (below) and it over-reached: what that incident actually cost was a write that
/// re-rendered OTHER notes' addresses, plus a consumer that handed pool bytes to the SDK without
/// normalising. keeps the first half of that rule and drops the second, because the pool is
/// an artifact an operator reads and it was showing two conventions in one file -- a canonical
/// `funding_multisig_address` beside notes spelled `0:<account_id>`, which names no DApp at all.

/// What the incident is now handled by instead: [`crate::cli::note_pick::ask_which`] converts the
/// picked address to the workchain form before any caller can reach `Address::parse`, which is what
/// the `--note-addr` flag has always done via `arg_to_chain_param`. That closes the failure class
/// for pools written by anyone, including the out-of-tree `mint_pn_pool` and any tool that already
/// emits the canonical spelling.

/// The pool file is a durable operator artifact, not a view. It is written by us and read by
/// programs that are not ours to change -- the out-of-tree `mint_pn_pool`, which emits
/// `0:<64hex>`, and `ci/shell_only_funding_bootstrap.sh:370`, which is why that assertion now takes
/// both spellings, on the route `ci/release_artifact_gate.sh` runs before a release. Reading stays
/// tolerant of both forms in every matcher, so a pool written by an earlier release keeps working
/// without a migration.

/// WHY THIS MODULE EXISTS RATHER THAN AN ASSERTION IN AN EXISTING TEST. Every pool fixture in this
/// crate spells its note address `"0:abc"`. That is not a parseable address: `CanonicalAddress::parse`
/// rejects it, and `dexdo_core::address::display` returns anything it cannot parse unchanged. So a
/// writer that upgrades legacy addresses to `<dapp_id>::<account_id>` is a NO-OP against every
/// existing fixture, and the whole offline suite stays green while the real path rewrites real
/// files. These tests use real 64-hex addresses for exactly that reason; a fixture that cannot
/// exhibit the behaviour cannot guard it.
#[cfg(test)]
mod pool_address_form_tests {
    use super::*;

    /// Exactly 64 hex characters, whatever the seed's length. A half that is not 64 wide is not an
    /// address at all, and a fixture that is not an address cannot exhibit the behaviour under test
    /// -- which is the same trap `"0:abc"` set for every other pool fixture in this crate.
    fn hex64(seed: &str) -> String {
        let rendered: String = seed.chars().cycle().take(64).collect();
        assert_eq!(rendered.len(), 64, "fixture half must be 64 hex chars");
        assert!(rendered.bytes().all(|byte| byte.is_ascii_hexdigit()));
        rendered
    }

    /// A real legacy address: `0:` plus 64 hex. The form the out-of-tree minter writes.
    fn legacy(seed: &str) -> String {
        format!("0:{}", hex64(seed))
    }

    /// A real canonical address, both halves 64 hex.
    fn canonical(dapp: &str, account: &str) -> String {
        format!("{}::{}", hex64(dapp), hex64(account))
    }

    fn state_for(address: &str, secret_seed: &str) -> OnboardPnState {
        let secret = secret_seed.repeat(32);
        let public = derive_owner_pubkey_from_secret_hex(&secret).expect("fixture key derives");
        OnboardPnState {
            endpoint: "net-a.example".into(),
            nominal: "N100".into(),
            token_type: SHELL_CURRENCY_ID,
            raw_value: 100_000_000_000,
            pn_address: Some(address.to_string()),
            deposit_identifier_hash: Some("123".into()),
            owner_public_key_hex: Some(public),
            owner_secret_key_hex: Some(secret.into()),
            deployed_at_unix: Some(1000),
            shell_funded: true,
            sanity_checked: true,
        }
    }

    /// Drive the real fold, then the real private atomic write, then read the bytes back off disk.
    /// The round trip is the point: a check that stops at the in-memory `Value` would pass while the
    /// file on disk had been rewritten.
    fn fold_and_round_trip(
        existing: Option<serde_json::Value>,
        state: &OnboardPnState,
        wallet: &str,
        path: &std::path::Path,
    ) -> serde_json::Value {
        let note = pn_state_to_pool_note(state).expect("pool note");
        let pool = pool_with_note_added(existing, state, note, 1234, wallet).expect("fold");
        let bytes = serde_json::to_string_pretty(&pool).expect("serialize pool");
        write_private_atomic(path, bytes.as_bytes()).expect("write pool");
        serde_json::from_slice(&std::fs::read(path).expect("read pool back")).expect("pool is json")
    }

    fn addresses(pool: &serde_json::Value) -> Vec<String> {
        pool["notes"]
            .as_array()
            .expect("notes[]")
            .iter()
            .map(|note| note["address"].as_str().expect("note address").to_string())
            .collect()
    }

    /// The incident, reduced: an existing legacy pool gains one note, and every address already in
    /// the file keeps the spelling it had.

    /// This is what a live campaign did on 2026-08-12. One pool of nine legacy notes was consumed by
    /// a buyer, every address in it came back as `<dapp_id>::<account_id>`, and the run died with 28
    /// identical `unsupported address workchain "0000...0004"` lines -- the SDK `Address::parse`
    /// reading the DApp half as a workchain. The file grew by exactly 9 x 64 bytes: nothing but the
    /// spelling had changed, and a whole minted note set was spent proving it.

    /// THE SEEDED ENTRY IS HAND-BUILT, not folded through `pn_state_to_pool_note`. Since that
    /// fold records the canonical form, so a pool seeded through it holds nothing legacy and this
    /// test would be comparing canonical to canonical: mutate `pool_with_note_added` to re-render
    /// every existing entry -- the exact write that cost the nine-note pool -- and it would stay
    /// green, because re-rendering an already-canonical address is a no-op. The property is about a
    /// pool written by SOMEONE ELSE (the out-of-tree minter, an earlier release), so the fixture has
    /// to be one.
    #[test]
    fn a_pool_write_leaves_every_address_already_in_the_file_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pn_pool.json");
        let wallet = legacy("b");
        let already_there = legacy("a1");
        let added = legacy("c3");

        // A pool as the out-of-tree `mint_pn_pool` leaves it: every note spelled `0:<64hex>`.
        let seeded = serde_json::json!({
            "endpoint": "net-a.example",
            "created_at_unix": 1234,
            "nominal": "N100",
            "token_type": SHELL_CURRENCY_ID,
            "raw_value_per_pn": 100_000_000_000u64,
            "funding_multisig_address": wallet,
            "notes": [{ "address": already_there, "owner_secret_key_hex": "00" }],
        });

        let grown = fold_and_round_trip(Some(seeded), &state_for(&added, "3b"), &wallet, &path);
        assert_eq!(
            addresses(&grown)[0],
            already_there,
            "adding a note must not re-render an address already in the pool -- that rewrite is \
             what cost a nine-note pool on 2026-08-12"
        );
        assert_eq!(
            addresses(&grown).len(),
            2,
            "the second note has to actually be in the file, or the check above compares nothing"
        );
        assert_eq!(
            grown["funding_multisig_address"].as_str().expect("wallet"),
            wallet,
            "the funding wallet identity keeps its stored form too"
        );
    }

    /// what a write RECORDS is the canonical `<dapp_id>::<account_id>`, for a note whose
    /// state carries the legacy spelling.

    /// The legacy `0:<account_id>` is the form an ABI-encoded contract parameter takes, and the
    /// only place it belongs. Storage is not that place: the pool held it beside a canonical
    /// `funding_multisig_address`, so one file showed two conventions and the same note reached the
    /// operator spelled three different ways across `note list`, `history` and this file.

    /// The account half is unchanged -- this upgrades the identity the spelling failed to carry, it
    /// does not move the address.
    #[test]
    fn a_written_note_is_recorded_canonically_even_when_its_state_is_legacy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pn_pool.json");
        let account = hex64("a1");
        let stored =
            fold_and_round_trip(None, &state_for(&legacy("a1"), "2a"), &legacy("b"), &path);

        assert_eq!(
            addresses(&stored),
            vec![format!("{}::{account}", dexdo_core::DEXDO_DAPP_ID)],
            "a PrivateNote lives in the dexdo DApp, and that is what the file must say"
        );
    }

    /// A supplied DApp id is authoritative: canonical in, the same bytes out. The rule is that a
    /// note is recorded canonically -- not that this writer invents a DApp for an address that
    /// already names one.
    #[test]
    fn a_canonical_address_survives_a_write_byte_for_byte() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pn_pool.json");
        let wallet = canonical("b", "b");
        let first = canonical("4", "a1");
        let second = canonical("4", "c3");

        let seeded = fold_and_round_trip(None, &state_for(&first, "2a"), &wallet, &path);
        let grown = fold_and_round_trip(Some(seeded), &state_for(&second, "3b"), &wallet, &path);
        assert_eq!(addresses(&grown), vec![first, second]);
        assert_eq!(
            grown["funding_multisig_address"].as_str().expect("wallet"),
            wallet
        );
    }

    /// The guard on the guard: these fixtures must be addresses the canonical renderer would
    /// actually change, or the tests above prove nothing. `"0:abc"` -- the spelling every other
    /// pool fixture in this crate uses -- fails this, and is why the defect was invisible offline.
    #[test]
    fn the_legacy_fixture_is_an_address_canonical_rendering_rewrites() {
        let address = legacy("a1");
        assert_ne!(
            dexdo_core::address::display(&address),
            address,
            "the fixture must be a real address that canonical rendering would rewrite"
        );
    }
}

/// the `lockedInOrders` section says what it measures, so no wrong conclusion is available.

/// Measured on the chain: a note with a standing inference order for 6.100000000 SHELL printed
/// `none reported` under a heading an operator reads as "what is holding my money", while the
/// trading record had honestly dropped by exactly that amount.
#[cfg(test)]
mod issue_1558_locked_in_orders_names_its_own_scope {
    use super::{render_locked_in_orders, NoteBalanceMap};

    fn rendered(map: NoteBalanceMap) -> String {
        let mut out = String::new();
        render_locked_in_orders(&mut out, &map);
        out
    }

    /// The empty case is the dangerous one: it is the reading that says "all clear".
    #[test]
    fn an_empty_field_does_not_read_as_nothing_is_locked() {
        let out = rendered(NoteBalanceMap::Known(Vec::new()));
        assert!(
            out.contains("PMP OrderBook collateral"),
            "the heading must name what the field measures: {out}"
        );
        assert!(
            out.contains("inference-order escrow is NOT counted by this field"),
            "an operator can still read this as 'nothing is locked': {out}"
        );
        assert!(
            out.contains("dexdo note outstanding"),
            "the line must say where standing orders can be seen: {out}"
        );
    }

    /// The caveat is as true when the field has a figure in it, and this file's rule is that no
    /// state is reported by silence -- so it is printed on every branch, not only the empty one.
    #[test]
    fn a_populated_field_carries_the_same_caveat() {
        let out = rendered(NoteBalanceMap::Known(vec![(
            dexdo_core::params::SHELL_CURRENCY_ID,
            3_000_000_000,
        )]));
        assert!(out.contains("ECC[2] SHELL: 3.000000000 SHELL"), "{out}");
        assert!(
            out.contains("inference-order escrow is NOT counted by this field"),
            "the caveat vanished as soon as the field had a number: {out}"
        );
    }

    /// An unreadable field stays unreadable: narrowing what the section claims must not turn "not
    /// read" into "nothing there".
    #[test]
    fn an_unreadable_field_is_still_reported_as_unread() {
        let out = rendered(NoteBalanceMap::Unknown("getter unavailable".to_string()));
        assert!(!out.contains("none reported"), "{out}");
        assert!(out.contains("getter unavailable"), "{out}");
    }
}
