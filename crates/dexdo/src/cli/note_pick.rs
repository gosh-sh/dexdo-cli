//! Which note a command spends from, when the operator did not say.

//! `--note-addr` is 128 hex characters the client itself wrote into the pool, and asking for it back
//! is a lookup the operator performs by hand. Where there is a terminal, the notes are offered
//! instead; where there is not, the refusal names the flag, exactly as before.

//! The rows are built here and the picking is [`super::choose`]'s: this module knows what a note
//! looks like on a line, and nothing about arrow keys.

use anyhow::Result;
use serde_json::Value;

/// One line of the picker: the note, and what it holds.

/// Two forms of the address, deliberately. `address` is what the pool recorded and what every later
/// command is handed -- it is an address the client can parse. `shown` is how it is displayed.

/// A note is not a self-DApp account: `PrivateNote` lives in `DEXDO_DAPP_ID`, which is what
/// `dexdo history` prints and what the pool records since. Rendering a legacy `0:<account>`
/// through the self-DApp seam produced `<account>::<account>` -- an address naming a DApp the note
/// is not in, and one a live run was refused for when the displayed form was passed on. Pools
/// written by earlier releases still hold the legacy form, so the upgrade happens here too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NoteRow {
    /// Exactly what the pool holds. Handed to the command that spends.
    pub(crate) address: String,
    /// The same note as the operator reads it.
    pub(crate) shown: String,
    /// What the note holds, already rendered -- or why that could not be read. Never a guess: a
    /// balance nobody could read is said to be unread, because an operator choosing where to spend
    /// from must not mistake "unknown" for "empty".
    pub(crate) holds: String,
}

impl NoteRow {
    /// `...8146ea::...8146ea 120 SHELL`

    /// The address is shortened but the dapp half is kept: it says which dapp the account lives in,
    /// and that is not decoration. The tail is what tells two notes apart at a glance, and the whole
    /// form is one line away in the log.
    pub(crate) fn line(&self) -> String {
        format!("{:<24} {}", short(&self.shown), self.holds)
    }
}

/// The last six characters of each half, so a line stays readable and still identifies the note.
pub(crate) fn short(address: &str) -> String {
    match address.split_once("::") {
        Some((dapp, account)) => format!("...{}::...{}", tail(dapp), tail(account)),
        None => format!("...{}", tail(address)),
    }
}

fn tail(part: &str) -> String {
    let chars: Vec<char> = part.chars().collect();
    chars[chars.len().saturating_sub(6)..].iter().collect()
}

/// Every note the pool records, in the order it records them.

/// Balances are not read here: this is the pure half, and what a note holds comes from the chain.
/// `holds` is filled in by the caller that has a client to ask.
pub(crate) fn rows_of(pool: &Value) -> Vec<NoteRow> {
    pool["notes"]
        .as_array()
        .map(|notes| {
            notes
                .iter()
                .filter_map(|note| note["address"].as_str())
                .map(|address| NoteRow {
                    address: address.to_string(),
                    shown: dexdo_core::address::display(address),
                    holds: "balance unread".to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// What one note holds of the currency this order is in, read the way `note balance` reads it.

/// `getDetails` and not the account's own ECC balance: the tokens a note holds are virtual, kept in
/// the note's `balance` map, which is what a deal is funded from. Only the order's currency is
/// shown -- an operator choosing where to sell SHELL from has no use for a column of eccUSDC.

/// A read that fails is reported as unread rather than as zero, because those are opposite answers
/// to "can I spend from this one".
pub(crate) async fn holdings_of(
    client: &dexdo_core::ChainClient,
    address: &str,
    currency: u32,
    unit: &str,
) -> String {
    match holdings_raw_of(client, address, currency).await {
        Ok(raw) => format!("{} {unit}", whole_units(raw)),
        Err(why) => why,
    }
}

/// The same read, as a NUMBER rather than as the sentence a person reads.

/// Split out for the machine contract: a document that repeated "balance unread"
/// would make a runtime sentinel-match English prose to tell a real balance from a failed read, and
/// a reworded human line would silently change the contract. `Err` carries the reason, in the same
/// words the human view shows, because that reason is itself a fact worth reporting.

/// A read that fails stays a failed read on both paths. Reporting it as zero is the one thing
/// neither may do: those are opposite answers to "can I spend from this one".
pub(crate) async fn holdings_raw_of(
    client: &dexdo_core::ChainClient,
    address: &str,
    currency: u32,
) -> Result<u128, String> {
    use crate::cli::note::{note_getter_balance_maps, NoteBalanceMap};
    use dexdo_core::chain::RetryingReads as _;
    use dexdo_core::private_note::artifacts::PRIVATE_NOTE_ABI_JSON;

    let Ok(parsed) = dexdo_core::address::parse_chain_address(address) else {
        return Err("unreadable address".to_string());
    };
    let details: Result<Option<Value>, _> = client
        .run_getter_retrying(
            &parsed,
            PRIVATE_NOTE_ABI_JSON,
            "getDetails",
            serde_json::json!({}),
        )
        .await;
    let maps = match details {
        Ok(details) => note_getter_balance_maps(details.as_ref()),
        Err(_) => return Err("balance unread".to_string()),
    };
    match maps.balance {
        NoteBalanceMap::Known(entries) => Ok(entries
            .iter()
            .find(|(id, _)| *id == currency)
            .map(|(_, amount)| *amount)
            .unwrap_or_default()),
        NoteBalanceMap::Unknown(_) => Err("balance unread".to_string()),
    }
}

/// Raw units as the operator's own unit, with the fraction only where there is one.

/// One SHELL is `SHELL_UNIT` raw, and an operator reading a menu of notes is choosing between "a
/// hundred" and "forty", not between twelve-digit integers.
pub(crate) fn whole_units(raw: u128) -> String {
    // One implementation of this rendering in the client, not three: the menu shows the same figure
    // in the same shape as every other answer, and the currency label beside it says which currency
    // it is.
    dexdo_core::shell_amount(raw)
}

/// Ask which note, or refuse the way every other unanswerable question is refused.

/// `None` from the picker is the operator leaving without choosing, and it is an error rather than
/// a default: a command that carried on with row zero would spend from a note nobody chose.
pub(crate) fn ask_which(rows: &[NoteRow]) -> Result<String> {
    if rows.is_empty() {
        anyhow::bail!(
            "no note to spend from: the pool records none. Deploy one with `dexdo note deploy`, \
             or pass --note-addr for a note kept outside the pool."
        );
    }
    if !crate::cli::interaction::may_ask() {
        return Err(crate::cli::interaction::cannot_ask(
            "the note to spend from",
            "--note-addr",
        ));
    }
    // One note and a terminal: still shown, still confirmed with Enter. A command that silently
    // picked "the only one" would do the same thing silently on the day there were two.
    let chosen = crate::cli::choose::ask(
        "Which note should this use?",
        rows.iter().map(NoteRow::line).collect(),
    )?
    .ok_or_else(|| anyhow::anyhow!("no note chosen"))?;
    // Settled BEFORE the confirmation is printed. A pool entry this client cannot convert is
    // refused where the operator is still reading the menu, not under a success glyph that has
    // already said the note was chosen -- a checkmark followed by a refusal describes two different
    // outcomes of one action.
    let picked = spendable_address(&rows[chosen])?;
    // The menu is erased on the way out, so the choice is left in its place. Without this a command
    // that later fails says nothing about which note it was going to spend from -- and that is the
    // first thing an operator reading the failure wants to know.
    // `spec.md`: the answer that stays behind names the note by its tail and what it holds, on one
    // line under the success glyph. The whole address belongs to a result, not to an echo.
    {
        use crate::cli::style::{self, Palette, Role};
        let palette = Palette::stderr();
        let row = &rows[chosen];
        eprintln!(
            "{}",
            style::glyph_line(
                palette,
                style::OK,
                Role::Ok,
                // "spendable" only in front of a figure. `holds` also carries "balance unread" and
                // "unreadable address", and prefixing those produced "spendable balance unread" --
                // a claim about spendability the client did not make and cannot make. This module
                // says it itself: unread and zero "are opposite answers to 'can I spend from this
                // one'".
                &format!(
                    "note {} {} {}{}",
                    // `shown`, not `address`: the echo is read by a person, and `address` is the
                    // pool's own bytes, which for a pool written before is `0:<account>` --
                    // a form that names no DApp, cannot be looked up in an explorer, and is not
                    // what this run will print anywhere else.
                    style::paint(palette, Role::Id, &style::short_id(&row.shown)),
                    style::paint(palette, Role::Label, "\u{b7}"),
                    if row
                        .holds
                        .trim()
                        .starts_with(|first: char| first.is_ascii_digit())
                    {
                        "spendable "
                    } else {
                        ""
                    },
                    style::paint(palette, Role::Bold, row.holds.trim())
                )
            )
        );
    }
    Ok(picked)
}

/// The picked note as the value `--note-addr` would have produced: the workchain form.

/// The picker's answer stands in for that flag, so it has to BE that flag's value -- clap runs
/// [`dexdo_core::address::arg_to_chain_param`] on it, and the pool's own bytes never went through
/// anything. Callers hand this straight to `dexdo_core::Address::parse`, which takes the workchain
/// form only, because that is what an ABI-encoded address parameter is. That conversion belongs
/// here, at the boundary, and nowhere earlier: the pool is storage and stores the canonical form.

/// Measured on 2026-08-12: a pool whose notes were spelled `<dapp_id>::<account_id>` was consumed
/// by a buyer and the run died with 28 `unsupported address workchain "0000...0004"` lines -- the
/// SDK reading the DApp half as a workchain. The answer then was to forbid that spelling in the
/// file, which left the seam itself unguarded; any pool written by another tool in the canonical
/// form still walked into it. Converting here closes the class for every writer.
pub(crate) fn spendable_address(row: &NoteRow) -> Result<String> {
    // `to_chain_param`, not `arg_to_chain_param`. The `arg_` variant exists to give a PERSON a
    // better refusal for a mistyped flag, and part of that is passing a value carrying no `::`
    // through untouched -- so `0:abc` and any non-address placeholder came out of it unchanged, the
    // success glyph printed, and the run died deep in the spend path at `Address::parse`. That is
    // the outcome this seam exists to prevent, and it is not the operator's typo: it is a pool
    // entry, and every form of it has to be judged here.
    dexdo_core::address::to_chain_param(&row.address).map_err(|error| {
        anyhow::anyhow!(
            "the pool records note {} in a form this client cannot use: {error}",
            row.shown
        )
    })
}

/// The whole question, from the pool file to the address: rows, balances, and the picker.

/// The balances are read here rather than inside the picker because reading them is a chain call
/// and the picker is pure terminal work. Where the chain cannot be reached the rows still show --
/// with the balance named as unread -- because the operator can still tell their notes apart, and a
/// command that refused to offer a choice over a missing balance would be worse than one that says
/// it does not know.
pub(crate) async fn ask_which_note(
    contracts: &std::path::Path,
    endpoint: Option<&str>,
) -> Result<String> {
    let Some(pool_path) = crate::cli::commands::note_pool_path(None) else {
        anyhow::bail!(
            "no pool to choose a note from. Deploy one with `dexdo note deploy`, or pass \
             --note-addr for a note kept outside the pool."
        );
    };
    let pool_path = crate::cli::note::resolve_private_file_path(&pool_path, "DEXDO_PN_POOL")?;
    let bytes = std::fs::read(&pool_path)
        .map_err(|error| anyhow::anyhow!("read the pool {}: {error}", pool_path.display()))?;
    let pool: Value = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "the pool {} is not valid JSON: {error}",
            pool_path.display()
        )
    })?;
    let mut rows = rows_of(&pool);
    if rows.is_empty() || !crate::cli::interaction::may_ask() {
        // Nothing to show, or nobody to show it to: `ask_which` says which of those it is.
        return ask_which(&rows);
    }
    if let Some(client) = balance_reader(contracts, endpoint) {
        // A live line for the wait, and a bound on it. Measured against the chain on a slow day: one
        // trivial read takes 6.1s, so a pool read one note after another is half a minute of a
        // screen with nothing on it -- which reads as a hung command, and was reported as one.

        // Three at a time, because the endpoint refuses more than three requests a second from one
        // address; five seconds each, because a balance nobody could read in five seconds is
        // reported as unread rather than waited for. The operator can still tell their notes apart
        // by address, and choosing is what they are here to do.
        let reading = crate::cli::progress::Status::new(format!(
            "reading what each note holds ({} note(s))",
            rows.len()
        ));
        let currency = dexdo_core::params::SHELL_CURRENCY_ID;
        for row in &mut rows {
            row.holds = match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                holdings_of(&client, &row.address, currency, "SHELL"),
            )
            .await
            {
                Ok(held) => held,
                Err(_) => "balance unread".to_string(),
            };
        }
        drop(reading);
    }
    ask_which(&rows)
}

/// A read-only client for the balances, or `None` when one cannot be built. The choice is still
/// offered without it -- an operator can tell their own notes apart by address.

/// The endpoint is the manifest's where it carries one, and otherwise the default of the network
/// the manifest NAMES. Deployed manifests routinely carry no endpoint at all -- `network:
/// "<one label>"` and nothing else -- which is why reading the field alone left every balance unread.
pub(crate) fn balance_reader(
    contracts: &std::path::Path,
    endpoint: Option<&str>,
) -> Option<dexdo_core::ChainClient> {
    // The client's own rule, and deliberately not a second one: explicit endpoint, then the
    // manifest's, then the default for the network it names. Deployed manifests routinely carry no
    // endpoint at all -- reading that field alone is what left every balance unread.
    let manifest = dexdo_core::Deployed::load(contracts).ok()?;
    let endpoint = dexdo_core::resolve_endpoint(endpoint, &manifest).ok()?;
    dexdo_core::ChainClient::connect(&endpoint).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pool() -> Value {
        json!({
            "notes": [
                { "address": format!("0:{}", "a".repeat(58) + "8146ea") },
                { "address": format!("{}::{}", "b".repeat(58) + "62bd69", "c".repeat(58) + "62bd69") },
            ]
        })
    }

    /// Shown in the scoped form, handed on exactly as recorded. A live run refused with
    /// "unsupported address workchain" when the displayed form was passed to the next command:
    /// `0:<account>` displays as `<account>::<account>`, which reads correctly and parses as
    /// nothing.
    #[test]
    fn a_row_is_shown_scoped_and_handed_on_as_recorded() {
        let pool = pool();
        let recorded: Vec<&str> = pool["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|note| note["address"].as_str().unwrap())
            .collect();
        let rows = rows_of(&pool);

        assert_eq!(rows.len(), 2);
        for (row, recorded) in rows.iter().zip(recorded) {
            assert_eq!(row.address, recorded, "handed on exactly as recorded");
            assert!(row.shown.contains("::"), "shown scoped: {}", row.shown);
            // Parsing lives behind the chain half; where it is compiled, what is handed on must
            // survive it, because that is what a later command does with it.
            assert!(
                dexdo_core::address::parse_chain_address(&row.address).is_ok(),
                "what is handed on has to parse: {}",
                row.address
            );
        }
    }

    /// The dapp half is kept, because it says which dapp the account lives in; both halves are cut
    /// to their tails, because 128 characters on a menu line is not a menu.
    #[test]
    fn a_shortened_address_keeps_both_halves() {
        let short = short("0000000000000000000000000000000000000000000000000000000000000004::c59c7c5867f2addcad6b0bc9ad29eaa4e0cba92a874a0e4d8520104e626cb785");
        assert_eq!(short, "...000004::...6cb785");
    }

    #[test]
    fn a_row_reads_as_a_note_and_what_it_holds() {
        let row = NoteRow {
            address: "0:aabb".to_string(),
            shown: "aa::bb".to_string(),
            holds: "120 SHELL".to_string(),
        };
        assert!(row.line().starts_with("...aa::...bb"), "{}", row.line());
        assert!(row.line().ends_with("120 SHELL"), "{}", row.line());
    }

    /// The menu is read by a person choosing where to spend from: raw units are not a quantity they
    /// hold an opinion about.
    #[test]
    fn an_amount_reads_as_the_unit_the_operator_holds() {
        let unit = dexdo_core::params::SHELL_UNIT;
        assert_eq!(whole_units(0), "0");
        assert_eq!(whole_units(unit), "1");
        assert_eq!(whole_units(120 * unit), "120");
        assert_eq!(whole_units(unit / 2), "0.5");
        assert_eq!(whole_units(3 * unit + unit / 4), "3.25");
    }

    /// a note is shown in the spelling the rest of the client uses for a note.

    /// `PrivateNote` lives in `DEXDO_DAPP_ID`. Rendering it through the self-DApp seam produced
    /// `<account>::<account>` for a legacy entry -- an address naming a DApp the note is not in,
    /// which `dexdo note list` printed under a closing line telling the operator to paste it as
    /// `--note-addr`. `dexdo history` printed the SAME note as `<dapp_id>::<account>` throughout.
    #[test]
    fn a_legacy_entry_is_shown_in_the_dapp_the_note_lives_in() {
        let account = "a".repeat(58) + "8146ea";
        let rows = rows_of(&json!({ "notes": [{ "address": format!("0:{account}") }] }));

        assert_eq!(
            rows[0].shown,
            format!("{}::{account}", dexdo_core::DEXDO_DAPP_ID),
            "the dapp half must be the dexdo DApp, not a second copy of the account id"
        );
    }

    /// whatever the pool holds, what the picker HANDS ON is the workchain form.

    /// This is the seam the 2026-08-12 incident went through: nine canonically-spelled notes were
    /// handed to `Address::parse` unchanged and the run died with `unsupported address workchain
    /// "0000...0004"` twenty-eight times. The `--note-addr` flag never had this problem, because
    /// clap converts; the picker did not convert at all.
    #[test]
    fn what_the_picker_hands_on_is_what_the_flag_would_have_produced() {
        let account = "c".repeat(58) + "62bd69";
        let legacy = format!("0:{account}");
        let canonical = format!("{}::{account}", dexdo_core::DEXDO_DAPP_ID);

        for recorded in [&legacy, &canonical] {
            let rows = rows_of(&json!({ "notes": [{ "address": recorded }] }));
            assert_eq!(
                spendable_address(&rows[0]).expect("a recorded address is spendable"),
                legacy,
                "the picker's answer must be the value `--note-addr` produces, for a pool \
                 recorded as `{recorded}`"
            );
        }
    }

    /// A pool with no notes is not a menu with no rows: the caller has to say so, and say what to do
    /// about it.
    #[test]
    fn an_empty_pool_produces_no_rows() {
        assert!(rows_of(&json!({ "notes": [] })).is_empty());
        assert!(rows_of(&json!({})).is_empty());
    }

    /// Under `cargo test` nothing can be asked, so this is the script's path: a refusal naming the
    /// flag, never a silent first row.
    #[test]
    fn without_a_terminal_it_refuses_and_names_the_flag() {
        let rows = rows_of(&pool());
        let refusal = ask_which(&rows)
            .expect_err("nothing can be asked here")
            .to_string();
        assert!(refusal.contains("--note-addr"), "{refusal}");

        let empty = ask_which(&[]).expect_err("no notes at all").to_string();
        assert!(empty.contains("note deploy"), "{empty}");
    }
}
