//! Order-book display command handler (Track C6, move-only).

use crate::cli::args::OrdersArgs;
use crate::cli::args::OrdersCommand;
use crate::cli::commands::{
    book_target_for, fold_snapshot_from_orders, read_book_target, registry_requested_model,
    resolve_order_book_target, retry_executable_read, target_from_market, BookTarget,
};
use anyhow::{bail, Result};
use std::future::Future;
// `render_order_line` and `render_escrow` are built in a default-features test build too, so
// `one_unit_everywhere` can drive the order row in the gate that runs on every push.
use dexdo_core::address as addr;
use dexdo_core::chain::BookEventFold;
use dexdo_core::OrderBookOrder;
use dexdo_core::OrderBookSnapshot;

/// An `orders` snapshot together with its provenance: which read path produced the rows and
/// the freshness marker that came with them.
struct OrdersView {
    snapshot: OrderBookSnapshot,
    rows: &'static str,
    last_update_id: String,
    /// The order ids the book's own `InferenceOrderExpired` named on this read.

    /// The fold computes this and then loses it: `LiveBookOrder::expired_by_event` has no
    /// counterpart on `OrderBookOrder`, so `fold_snapshot_from_orders` cannot carry it and the
    /// rendered row never saw it. It travels beside the snapshot rather than inside it because the
    /// snapshot is the shape BOTH read paths produce, and only one of them can know this.
    swept_order_ids: std::collections::BTreeSet<u128>,
}

/// What one read path can honestly say about the escrow behind one row.

/// Three states, not two. The two `orders` read paths do not know the same things. `getOrder`
/// returns the stored `Order`, whose `escrow` is the SHELL a bid is holding
/// (`contracts/airegistry/InferenceOrderBook.sol:259`, "BUY: SHELL budget held; SELL: 0"), and the
/// parser refuses a record that lacks it. The event fold has no such number to fold:
/// `InferenceOrderPlaced(orderId, isBuy, price, ticks, note, tokenContract, deadline, flags)`
/// (`:371`) never carries escrow, so `LiveBookOrder` has no escrow field and
/// `fold_snapshot_from_orders` fills the struct with a zero that must never reach an operator as a
/// quantity.

/// But "the fold cannot tell you the amount" was answering two different questions with one `-`
/// . For an order past its deadline the owner's question is not "how much" -- it is "is the
/// book still holding it?", and those are opposite actions: still held means run `dexdo orders
/// expire <id>` and get the money back, already swept means there is nothing to do. The fold knows
/// which, through the `InferenceOrderExpired` it deliberately treats as non-terminal, and until now
/// that knowledge stopped one layer below the row that needed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EscrowRead {
    /// `getOrder` answered, so the number on the row is the book's own. A record the getter returned
    /// is by construction still in the book: the sweep removes it.
    Authoritative,
    /// The event fold, order still in the book: the amount is unknowable on this path, and saying so
    /// is not the same as reporting the filler zero.
    HeldAmountUnknown,
    /// The event fold, and the book's own `InferenceOrderExpired` named this order id: the book has
    /// removed it and refunded a bid's escrow (`InferenceOrderBook.sol`, the expiry sweep). A SELL
    /// commits none in the first place, so on either side the answer to "is the book holding my
    /// money for this order" is now no.
    Returned,
}

impl OrdersView {
    /// Everything the fold read path derives from one folded book, in ONE place.

    /// `read_live_order_snapshot` reads the book and then does nothing else with what it got: the
    /// rows both read paths share come out of `fold_snapshot_from_orders`, and the one thing only
    /// this path can know -- which ids the book's own `InferenceOrderExpired` named -- is derived
    /// here, beside the `ROWS_CHAIN_EVENTS` marker that makes [`OrdersView::escrow_read`] consult
    /// it at all. Those two belong together: the marker without the ids renders every fold row as
    /// an authoritative `escrow=0`, which is the filler zero exists to keep off the row.

    /// It is a function rather than three expressions inline in the read closure because a
    /// regression that restates the derivation beside the code proves the carry the TEST performs,
    /// not the carry the read path performs -- and stays green while the read path stops carrying
    /// anything at all, which is exactly the state reported.
    fn from_fold(
        target: &BookTarget,
        order_book: &str,
        orders: &[&dexdo_core::chain::LiveBookOrder],
        last_update_id: String,
    ) -> Self {
        OrdersView {
            snapshot: fold_snapshot_from_orders(target, order_book, orders.iter().copied()),
            rows: crate::cli::provenance::ROWS_CHAIN_EVENTS,
            last_update_id,
            // WHICH of the visible rows the book has already swept is carried out with them.
            // The fold keeps a swept row deliberately, so the row itself has to say whether
            // the escrow behind it is still in the book or already back at the note -- the two
            // states call for opposite actions and rendered identically until this.
            swept_order_ids: orders
                .iter()
                .filter(|order| order.expired_by_event)
                .map(|order| order.order_id)
                .collect(),
        }
    }

    /// What this read can say about one row's escrow.
    fn escrow_read(&self, order: &OrderBookOrder) -> EscrowRead {
        if self.rows == crate::cli::provenance::ROWS_CHAIN_GETTERS {
            return EscrowRead::Authoritative;
        }
        if self.swept_order_ids.contains(&order.order_id) {
            return EscrowRead::Returned;
        }
        EscrowRead::HeldAmountUnknown
    }
}

/// A `BookTarget` named only by the model's on-chain id.

/// The hash is validated here rather than at the getter, so a mistyped one is refused before any
/// chain read and long before anything is signed. `frame_model` stays empty: this route genuinely
/// does not know the name, and every line that shows it goes through [`model_label`], which says the
/// hash instead of showing a blank where an identity belongs.
fn book_target_from_model_hash(model_hash: &str, note_addr: &str) -> Result<BookTarget> {
    let trimmed = model_hash.trim();
    let digits = trimmed.strip_prefix("0x").unwrap_or(trimmed);
    if digits.len() != 64 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!(
            "--model-hash {model_hash} is not a model id: it must be 0x followed by 64 hex digits, \
             exactly as `dexdo note outstanding` prints it"
        );
    }
    Ok(BookTarget {
        frame_model: String::new(),
        model_hash: format!("0x{}", digits.to_ascii_lowercase()),
        order_book: None,
        root_model: None,
        note_addr: Some(note_addr.to_string()),
    })
}

/// What to call the model on screen: its name where one is known, its id where none is.

/// A blank in a confirmation line reads as "this field failed to fill", not as "this model has no
/// name here", and the two call for opposite reactions from someone about to sign. The id is the
/// honest answer, and it is the same string the operator passed in.
fn model_label<'a>(frame_model: &'a str, model_hash: &'a str) -> &'a str {
    if frame_model.is_empty() {
        model_hash
    } else {
        frame_model
    }
}

#[cfg(test)]
#[path = "orders_1659_tests.rs"]
mod orders_1659_tests;

async fn read_live_order_snapshot(
    chain: &dexdo_core::RealChainBackend,
    target: &BookTarget,
    order_book: &str,
) -> Result<OrdersView> {
    read_live_order_snapshot_with(
        || async {
            let fold = chain
                .fold_order_book_events(order_book, BookEventFold::default())
                .await?;
            let last_update_id = fold.last_seen_id().unwrap_or("-").to_string();
            // explicit non-goal: an expired order is NOT filtered out of the owner's own list
            // -- not by the deadline predicate and not by consuming `InferenceOrderExpired`. Expiry
            // is lazy on chain, so a lapsed order can sit in the book holding escrow indefinitely;
            // hiding it loses the operator's money from view. It is shown, and marked `expired=yes`.
            let orders = fold.all_orders().collect::<Vec<_>>();
            Ok(OrdersView::from_fold(
                target,
                order_book,
                &orders,
                last_update_id,
            ))
        },
        || async { read_book_target(chain, target).await },
    )
    .await
}

/// the rule, with both reads as seams.

/// Split the way `market_views::read_executable_market_view_with` is already split, and for the same
/// reason: a branch that binds a concrete `RealChainBackend` cannot be brought into execution by a
/// test, and a rule nobody can execute has no proof -- only the claim that it was written.
async fn read_live_order_snapshot_with<FF, FFut, FB, FBFut>(
    fold_read: FF,
    mut fallback_read: FB,
) -> Result<OrdersView>
where
    FF: FnMut() -> FFut,
    FFut: Future<Output = Result<OrdersView>>,
    FB: FnMut() -> FBFut,
    FBFut: Future<Output = Result<OrderBookSnapshot>>,
{
    let mut fold_read = fold_read;
    match retry_executable_read("order-book event fold", &mut fold_read).await {
        // a fold that ANSWERED still has to be believed, and an empty answer is the one that
        // cannot be. History is kept in a window, so "no rows" and "the rows are older than what I
        // can see" arrive here identically; only the book's own storage separates them.
        Ok(mut view) => {
            let fold_rows = std::mem::take(&mut view.snapshot.orders);
            let (rows, source) =
                crate::cli::fold_completeness::rows_or_storage_when_empty(fold_rows, || async {
                    let snapshot = retry_executable_read(
                        "storage confirmation of an empty order-book fold",
                        &mut fallback_read,
                    )
                    .await?;
                    Ok(snapshot.orders)
                })
                .await?;
            view.snapshot.orders = rows;
            view.rows = source.provenance();
            if source == crate::cli::fold_completeness::RowSource::Storage {
                // The storage rows never announce a sweep; they are the rows the book still holds.
                view.swept_order_ids = std::collections::BTreeSet::new();
            }
            Ok(view)
        }
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "order-book event fold unavailable; using legacy chain fallback");
            let snapshot =
                retry_executable_read("legacy order-book fallback", &mut fallback_read).await?;
            Ok(OrdersView {
                snapshot,
                rows: crate::cli::provenance::ROWS_CHAIN_GETTERS,
                last_update_id: "-".to_string(),
                // The getters never announce a sweep; they simply stop returning the record. Every
                // row on this path is one the book still holds, and its escrow is a real number.
                swept_order_ids: std::collections::BTreeSet::new(),
            })
        }
    }
}

/// say where these rows came from and how fresh they are, in the same vocabulary
/// `dexdo market` uses, so the two views can be compared key for key instead of reading as
/// contradictory truth. `orders` never consults the indexer, so `source` is always `chain`.
fn render_orders_context(view: &OrdersView, as_of: u64, owner: &str) -> String {
    // The owner in the canonical spelling, whatever spelling the flag was given in: what the client
    // prints is one form, and made that form the one every command reads back.

    // The owner of a resting order is a NOTE, and a note is not a self-DApp account -- it lives in
    // `DEXDO_DAPP_ID`. The self-DApp seam rendered a legacy `0:<account>` as `<account>::<account>`,
    // so this line disagreed with the refusal thirty lines below it (`addr::display(note_addr)`)
    // about the spelling of the same note, inside one command.
    let owner = dexdo_core::address::display(owner);
    format!(
        "orders {} owner={owner}",
        crate::cli::provenance::render(
            "chain",
            &view.last_update_id,
            as_of,
            view.rows,
            crate::cli::provenance::SCOPE_OWNER_RESTING,
        )
    )
}

fn own_orders<'a>(snapshot: &'a OrderBookSnapshot, note_addr: &str) -> Vec<&'a OrderBookOrder> {
    let want = dexdo_core::normalize_wallet_address(note_addr)
        .unwrap_or_else(|_| note_addr.trim().to_string());
    snapshot
        .orders
        .iter()
        .filter(|o| {
            dexdo_core::normalize_wallet_address(&o.owner_note)
                .map(|owner| owner == want)
                .unwrap_or_else(|_| o.owner_note.eq_ignore_ascii_case(&want))
        })
        .collect()
}

/// `deadline` as a UTC date-time: the raw stamp stays for machines, the rendering is for the
/// operator who has to decide whether the order is still worth anything.

/// Rendered in place rather than pulled in from a date library: the workspace carries no date
/// dependency (its manifests say so deliberately for the HTTP and TLS stacks alike), `std` has no
/// calendar, and the civil-date conversion is a dozen lines pinned by tests below.
fn render_unix_utc(seconds: u64) -> String {
    if seconds == 0 {
        // No deadline was set. The epoch would be a lie in both directions: a GTC bid never expires,
        // and a zero-deadline SELL is malformed rather than long dead.
        return "-".to_string();
    }
    // days-from-civil, inverted (Howard Hinnant's algorithm, the one every date library implements):
    // shift the era to start on 0000-03-01 so the leap day lands at the end of the 400-year cycle.
    let days = (seconds / 86_400) as i64;
    let time_of_day = seconds % 86_400;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u64;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u64;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time_of_day / 3_600,
        (time_of_day % 3_600) / 60,
        time_of_day % 60
    )
}

/// The escrow cell, which is where an owner asks "is the book still holding my money?".

/// Never a zero stand-in. `escrow=0` is a real and meaningful reading -- every SELL rests with it --
/// so a filler zero is indistinguishable from the book saying "nothing is held here", and on a bid
/// it reads as "your money is already back" over an order that is still holding it.

/// `returned` is a word rather than a number for the same reason: the fold still has no amount to
/// report, and the one thing it does know is that the amount is no longer the book's to hold.
fn render_escrow(order: &OrderBookOrder, escrow: EscrowRead) -> String {
    match escrow {
        EscrowRead::Authoritative => dexdo_core::shell_amount(order.escrow),
        EscrowRead::HeldAmountUnknown => "-".to_string(),
        EscrowRead::Returned => "returned".to_string(),
    }
}

pub(crate) fn render_order_line(order: &OrderBookOrder, as_of: u64, escrow: EscrowRead) -> String {
    let side = if order.is_buy { "buy" } else { "sell" };
    // A bid has no deal contract to name: the book stores a BUY with `tokenContract: address(0)`
    // (`contracts/airegistry/InferenceOrderBook.sol:254,1185` -- "SELL: seller's deal contract;
    // BUY: 0"), so `-` here is the chain's own answer, not a lookup this client failed to do.
    let tc = order.token_contract.as_deref().unwrap_or("-");
    // ONE deadline predicate for every fold-backed view. `market` uses it to drop a row from
    // the executable scope; here the same verdict is a label, against the very `as_of` the context
    // line above already printed -- so the two views can be reconciled instead of second-guessed.

    // `expired` is the CLOCK's verdict and nothing more -- it is true both while the book is
    // still holding the escrow and after the book has swept it and given it back. The escrow cell is
    // what separates those two, which is why this stayed a `yes|no` deadline label.
    let expired = !dexdo_core::order_deadline_is_live(order.is_buy, order.deadline, as_of);
    // `flags` is printed unconditionally, with no authoritative/filler distinction, because
    // BOTH `orders` read paths really carry it -- `getOrder` returns the stored `Order.flags` and
    // `InferenceOrderPlaced` declares `flags` as a `uint8` the fold decodes. That is precisely what
    // separates it from `escrow` on the line above: the number is the book's, on either path.
    format!(
        "order_id={} side={} owner={} token_contract={} price_per_tick={} ticks={} escrow={} flags={} deadline={} deadline_utc={} expired={}",
        order.order_id,
        side,
        addr::display(&order.owner_note),
        addr::display_self_dapp(tc),
        dexdo_core::shell_amount(order.price_per_tick),
        order.ticks,
        render_escrow(order, escrow),
        order.flags,
        order.deadline,
        render_unix_utc(order.deadline),
        if expired { "yes" } else { "no" }
    )
}

/// What `dexdo orders expire` may do about one named order id, decided before any message is sent.
#[derive(Debug, PartialEq, Eq)]
enum ExpireAction {
    /// Nothing rests under this id for this note. The chain agrees: `expireOrder` on a gone order
    /// is a silent no-op, so this client says so and stops rather than sending a message that
    /// could only be a no-op too.
    NotResting,
    /// The book still holds it and its own deadline has not passed. `_isExpired` is
    /// `deadline != 0 && block.timestamp >= deadline`, so the sweep would do nothing -- and a client
    /// that sent it anyway would let the operator read "expired" over an order still holding money.
    StillLive { deadline: u64 },
    /// Past its own deadline and still in the book: one permissionless sweep is the whole action.
    Sweep,
}

/// The deadline verdict is [`dexdo_core::order_deadline_is_live`] -- the ONE predicate `market` and
/// `render_order_line` already use -- so the `expired=` column an operator just read and the action
/// this command takes can never disagree.
fn expire_action(order: Option<&OrderBookOrder>, as_of: u64) -> ExpireAction {
    let Some(order) = order else {
        return ExpireAction::NotResting;
    };
    if dexdo_core::order_deadline_is_live(order.is_buy, order.deadline, as_of) {
        return ExpireAction::StillLive {
            deadline: order.deadline,
        };
    }
    ExpireAction::Sweep
}

/// Name the deadline that makes the sweep premature, so the operator learns WHEN this becomes
/// possible instead of only that it is not possible now.
fn expire_too_early(order_id: u128, deadline: u64, as_of: u64) -> String {
    let until = if deadline == 0 {
        // A BUY may rest with deadline 0 (contract-permitted GTC). Nothing will ever expire it, so
        // saying "wait" would be a lie; cancel is the only exit.
        "carries deadline 0, which never expires".to_string()
    } else {
        format!(
            "is live until unix {deadline} ({}), {} seconds after this read at unix {as_of}",
            render_unix_utc(deadline),
            deadline.saturating_sub(as_of)
        )
    };
    format!(
        "refusing to expire: order {order_id} {until}. The book sweeps an order only once its own \
         deadline has passed, so no message was sent and no escrow moved; wait for that deadline \
         or release the escrow now with `dexdo orders cancel {order_id}`"
    )
}

/// Confirm one removal the way `subscription cancel` confirms its own: poll the authoritative book,
/// bounded by the read timeout, and read the note's spendable balance on either side of it.
/// Shared by `cancel` and `expire` -- the two differ in who may ask and in what a replay means, not
/// in what "the book removed it and the money came back" looks like.

/// The fold's explicit `expired_by_event` is sufficient. A row absent from the bounded fold is not:
/// direct order-book storage must independently confirm the absence before removal is reported.
/// That distinction lets an expiry remain visible in the owner's folded history without turning a
/// truncated history into a false cancellation.

/// The credit reported is one this client OBSERVED. It is never derived from the row's `escrow`
/// field, which the event-fold read path does not carry at all (`escrow=-`) -- computing a refund
/// from a number the book did not hand us is exactly the invented money E2E-CXL-14 forbids.
/// `InferenceOrderCancelled` does carry a `refunded` field; it is deliberately NOT used as the
/// figure here, so that one command cannot report a refund a different way from the other.
async fn reconcile_order_removal(
    chain: &dexdo_core::RealChainBackend,
    order_book: &str,
    note: &dexdo_core::Address,
    order_id: u128,
    balance_before: u128,
    wait: std::time::Duration,
) -> Result<Option<(u128, u128)>> {
    let book = dexdo_core::Address::parse(order_book)
        .map_err(|error| anyhow::anyhow!("order_book {order_book}: {error}"))?;
    reconcile_order_removal_with(
        || async {
            let fold = chain
                .fold_order_book_events(order_book, BookEventFold::default())
                .await?;
            Ok(fold.all_orders().cloned().collect::<Vec<_>>())
        },
        || async { chain.inference_orderbook_live_orders(&book).await },
        || async { chain.private_note_shell_balance(note).await },
        order_id,
        balance_before,
        wait,
    )
    .await
}

/// site 3: the rule pointed the other way, with its three reads as seams.

/// Two different things used to read alike here: the fold SEEING the row and saying it is swept,
/// which is a statement, and the fold not carrying the row at all, which is silence. Only the first
/// settles anything; for the second the book's own storage decides, exactly as it does before a
/// refusal in `read_live_order_snapshot`. Concluding "removed" from silence is how a bounded history
/// becomes a reported removal that never happened -- and this one CONFIRMS rather than refuses, so it
/// is the direction that hands an operator a figure instead of withholding one.
async fn reconcile_order_removal_with<FF, FFut, FS, FSut, FB, FBut>(
    mut fold_read: FF,
    mut storage_read: FS,
    mut balance_read: FB,
    order_id: u128,
    balance_before: u128,
    wait: std::time::Duration,
) -> Result<Option<(u128, u128)>>
where
    FF: FnMut() -> FFut,
    FFut: Future<Output = Result<Vec<dexdo_core::chain::LiveBookOrder>>>,
    FS: FnMut() -> FSut,
    FSut: Future<Output = Result<Vec<dexdo_core::OrderBookOrder>>>,
    FB: FnMut() -> FBut,
    FBut: Future<Output = Result<u128>>,
{
    let started = std::time::Instant::now();
    loop {
        let remaining = wait.saturating_sub(started.elapsed());
        let observe = async {
            let folded = fold_read().await?;
            let removed = match folded.iter().find(|order| order.order_id == order_id) {
                Some(order) => order.expired_by_event,
                None => {
                    crate::cli::fold_completeness::absence_is_confirmed_by_storage(
                        order_id,
                        &mut storage_read,
                    )
                    .await?
                }
            };
            let balance = balance_read().await?;
            Ok::<_, anyhow::Error>((removed, balance))
        };
        let (removed, balance_after) = match tokio::time::timeout(remaining, observe).await {
            Ok(observation) => observation?,
            Err(_) => return Ok(None),
        };
        if removed {
            return Ok(Some((
                balance_after.saturating_sub(balance_before),
                balance_after,
            )));
        }
        let elapsed = started.elapsed();
        if elapsed >= wait {
            return Ok(None);
        }
        tokio::time::sleep(
            dexdo_core::SUBSCRIPTION_ORDER_RECONCILE_POLL.min(wait.saturating_sub(elapsed)),
        )
        .await;
    }
}

/// Render the book's fill history for one note as candidates and refusals.

/// The wording is part of the safety surface. This command exists for an operator whose client died
/// holding a funded deal, and the two ways it can mislead are opposite: presenting a candidate as a
/// recovered deal invites acting on money that may already be settled, and presenting an empty list
/// as "you have no deal" hides a fill the book really emitted. So the header states what was found
/// before anything is listed, every refusal is printed with the TokenContract's own reason, and the
/// caveat says plainly that absence here is not proof.

/// Pure `&Report -> String`: no clock, no chain, no IO, so the exact operator text is testable.
fn render_orders_fills(
    frame_model: &str,
    order_book: &str,
    note_addr: &str,
    report: &dexdo_core::chain::BookFillCandidateReport,
) -> String {
    let mut rendered = format!(
        "orders fills model={frame_model} order_book={} owner={} fills_named={}\n",
        addr::display(order_book),
        addr::display(note_addr),
        report.fills_named()
    );
    rendered.push_str(
        "Caveat: these are book fill candidates, not recovered deals. InferenceFilled is emitted \
         in the match transaction itself, so it proves the book named this note -- not that the \
         deal is funded now, and not that it still exists. Every address below was re-checked \
         against that TokenContract's own getParties/getState. An empty report is not proof that \
         no deal exists: it can only see the ext-out history this endpoint still serves.\n",
    );
    if report.candidates.is_empty() {
        rendered.push_str("Verified funded fill candidates: none\n");
    } else {
        for candidate in &report.candidates {
            rendered.push_str(&format!(
                "Verified funded fill candidate: {}\n",
                render_fill_identity(candidate)
            ));
        }
    }
    for refusal in &report.refusals {
        rendered.push_str(&format!(
            "Refused InferenceFilled deal pointer: {} reason={}\n",
            render_fill_identity(&refusal.candidate),
            refusal.reason
        ));
    }
    rendered
}

/// The identity half of one fill, shared by the confirmed and refused lines.

/// Both lines carry it because a refused fill is exactly the case an operator has to reconcile by
/// hand, and `sellerTC`/`sellerNote`/the order ids are what is about not losing.
fn render_fill_identity(candidate: &dexdo_core::chain::BookFillCandidate) -> String {
    format!(
        "token_contract={} seller_note={} maker_id={} taker_id={} ticks={} clearing_price={}",
        addr::display(&candidate.seller_token_contract),
        addr::display(&candidate.seller_note),
        candidate.maker_id,
        candidate.taker_id,
        candidate.ticks,
        dexdo_core::shell_amount_of_text(&candidate.clearing_price)
    )
}

/// What the confirming `orders` arms print about the subscription journal, INCLUDING when the
/// journal could not be written.

/// Correcting the journal is fallible in three ways: the buyer's money lock is held (by a
/// running buyer, or by a sentinel a killed one left behind), the journal on disk does not decode,
/// or the record disagrees with a refund the chain has already paid. Propagating any of those with
/// `?` took the whole confirmation line away -- by that point the cancellation is on chain,
/// `reconcile_order_removal` has confirmed it and the escrow is back, and the operator was shown a
/// non-zero exit and no `cancel confirmed... refund=` line at all. The lock's own refusal even
/// reads "no BOC was sent", which is true where that sentence was written and false here.

/// So a failed write is a THIRD outcome of a field that already had two, never a return: the chain
/// fact is printed either way, and what could not be written to a local file is said separately in
/// the words of the failure. The exit stays 0 because the subject of the command -- the removal and
/// its refund -- succeeded, and because the journal left uncorrected reads `resting` about money
/// that is already back: the alarming direction, and the one an operator checks.
fn subscription_journal_report(
    order_id: u128,
    corrected: Result<bool>,
) -> (&'static str, Option<String>) {
    match corrected {
        Ok(true) => ("closed", None),
        Ok(false) => ("none", None),
        Err(error) => (
            "not-closed",
            Some(format!(
                "subscription_journal_not_closed order_id={order_id}: the removal above is \
                 confirmed on chain and the refund it names is the balance move that removal \
                 produced -- nothing here undoes either. What failed is only the write to this \
                 note's local subscription journal, so a `subscription` surface reading that file \
                 may keep showing an older phase for this order until it is corrected; the chain is \
                 the authority and `dexdo subscription status` reads it. The reason follows, and it \
                 is the reason the FILE was not written: some of its wordings belong to a path that \
                 submits nothing and do not describe this one. {error:#}"
            )),
        ),
    }
}

pub(crate) async fn run_orders(args: OrdersArgs) -> Result<()> {
    let note_addr = args.identity.note_addr.as_deref().ok_or_else(|| {
        anyhow::anyhow!("orders requires --note-addr (the owner PrivateNote to filter/cancel)")
    })?;
    // ONE budget for the command, and one read of the manifest path -- but BELOW the
    // argument check, not above it. `manifest_path()` reads the environment and can fail on its
    // own, and hoisting it over that check made `dexdo orders list` with no `--note-addr` report a
    // missing manifest instead of the missing argument, sending the operator to configure a
    // deployment for a command that was going to be refused on its arguments either way.

    // `direct_chain_read_with_timeout` bounds ONE read; this function made SEVEN, each taking a
    // fresh full `--read-timeout`. The bound is what the operator asked for, not what each read
    // asks for.

    // The budget covers READS. `reconcile_order_removal` below waits for chain state to settle
    // after a write and takes its own `--read-timeout`-shaped wait; that is a different thing from
    // a read and is deliberately not charged here. Charging it would make the read after a
    // reconciliation refuse as a timeout while nothing was hung.
    let manifest_path = crate::cli::commands::manifest_path()?;
    let budget = crate::cli::commands::ReadBudget::new(args.read_timeout.read_timeout_secs);
    let chain = dexdo_core::RealChainBackend::connect(
        manifest_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("DEXDO_MANIFEST: non-printable path"))?,
    )?;
    if matches!(&args.command, OrdersCommand::Journal) {
        return budget
            .read(crate::cli::buyer::run_buyer_submit_journal(
                &chain, note_addr,
            ))
            .await;
    }
    let target = if let Some(model_hash) = args.model_hash.as_deref() {
        // the note-and-key route. No file is read and no name is resolved, because an owner
        // recovering a stranded order has neither -- what they have is the hash `note outstanding`
        // read out of the note's own inbound history. `frame_model` is left empty on purpose: the
        // name is genuinely unknown here, and inventing one would put a guess into a line the
        // operator reads before signing.
        book_target_from_model_hash(model_hash, note_addr)?
    } else if let Some(market) = args.market.as_deref() {
        if args.model.is_some() {
            bail!("--market and --model are mutually exclusive for orders");
        }
        target_from_market(market)?
    } else {
        // `orders` had NO registry path at all -- not even behind
        // `--model-registry-validation`, which the other three read views did honour. So it was the
        // strictest of them: a `models.json` was required to name a book the registry could have

        // paths the registry serves, so it asks the same question the same way, through this
        // command's own budget.
        let model = args.model.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "orders without --market requires --model, or --model-hash when the name is \
                 not known -- `dexdo note outstanding` prints the hash of every resting order"
            )
        })?;
        let requested_model = budget
            .read(registry_requested_model(
                &manifest_path,
                None,
                &args.models,
                model,
            ))
            .await?;
        book_target_for(requested_model, Some(note_addr.to_string()))
    };
    // this is the one `orders` subcommand that is NOT about resting rows, so it returns
    // before the fold. A matched order is gone from the book; the evidence it left is the fill
    // event, and folding the resting projection first would pay a full history walk to produce
    // rows this command never reads.
    if matches!(&args.command, OrdersCommand::Fills) {
        return budget
            .read(async {
                let order_book = resolve_order_book_target(&chain, &target).await?;
                let book = dexdo_core::Address::parse(&order_book)
                    .map_err(|error| anyhow::anyhow!("order_book {order_book}: {error}"))?;
                let note = dexdo_core::Address::parse(note_addr)
                    .map_err(|error| anyhow::anyhow!("--note-addr {note_addr}: {error}"))?;
                let report = chain.verified_book_fill_candidates(&book, &note).await?;
                print!(
                    "{}",
                    render_orders_fills(
                        model_label(&target.frame_model, &target.model_hash),
                        &order_book,
                        note_addr,
                        &report,
                    )
                );
                Ok(())
            })
            .await;
    }
    let view = budget
        .read(async {
            let order_book = resolve_order_book_target(&chain, &target).await?;
            read_live_order_snapshot(&chain, &target, &order_book).await
        })
        .await?;
    let as_of = crate::cli::provenance::now_unix()?;
    let snapshot = &view.snapshot;
    let own = own_orders(snapshot, note_addr);
    match args.command {
        OrdersCommand::Journal => unreachable!("journal returns before resolving a model book"),
        // Handled above, before the resting-order fold: `fills` reads the book's fill history, not
        // the rows this projection carries.
        OrdersCommand::Fills => {}
        OrdersCommand::List => {
            // the provenance line comes FIRST, so a divergence from `dexdo market` is read
            // as a different source/scope before the rows are compared.
            println!("{}", render_orders_context(&view, as_of, note_addr));
            if own.is_empty() {
                println!(
                    "orders model={} order_book={} owner={} none=true",
                    model_label(&snapshot.frame_model, &snapshot.model_hash),
                    addr::display(&snapshot.order_book),
                    addr::display(note_addr)
                );
            } else {
                for order in own {
                    println!(
                        "{}",
                        render_order_line(order, as_of, view.escrow_read(order))
                    );
                }
            }
        }
        OrdersCommand::Show { order_id } => {
            let order = own
                .into_iter()
                .find(|o| o.order_id == order_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "order {order_id} is not a resting order owned by note {} in {}",
                        addr::display(note_addr),
                        addr::display(&snapshot.order_book)
                    )
                })?;
            println!("{}", render_orders_context(&view, as_of, note_addr));
            println!(
                "{}",
                render_order_line(order, as_of, view.escrow_read(order))
            );
        }
        OrdersCommand::Cancel { order_id } => {
            let order = own
                .into_iter()
                .find(|o| o.order_id == order_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "refusing to cancel: order {order_id} is not owned by note {} in {}",
                        addr::display(note_addr),
                        addr::display(&snapshot.order_book)
                    )
                })?;
            // The flag where it was passed, the pool entry for this note where it was not.
            let secret = crate::cli::support::note_owner_secret_for(
                args.identity.note_key.as_deref(),
                note_addr,
                None,
                "orders cancel",
                "the key that signs the note's owner method",
            )?;
            let note = dexdo_core::Address::parse(note_addr)
                .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
            let keys = dexdo_core::KeyPair::from_secret_hex(secret.trim())
                .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
            budget
                .read(chain.assert_note_owner_matches("orders cancel", &note, &keys))
                .await?;
            let balance_before = budget.read(chain.private_note_shell_balance(&note)).await?;
            chain
                .cancel_inference_order(&note, &keys, &target.model_hash, order.order_id)
                .await?;
            println!(
                "cancel submitted model={} order_book={} order_id={} owner={}",
                model_label(&snapshot.frame_model, &snapshot.model_hash),
                addr::display(&snapshot.order_book),
                order.order_id,
                addr::display(note_addr)
            );
            let confirmed = reconcile_order_removal(
                &chain,
                &snapshot.order_book,
                &note,
                order.order_id,
                balance_before,
                std::time::Duration::from_secs(args.read_timeout.read_timeout_secs),
            )
            .await?;
            let Some((refund, balance_after)) = confirmed else {
                // A cancel can also be REFUSED by the book -- a foreign owner or an order already
                // gone raises `InferenceOrderCancelRejected`, and a full queue means "ask again"
                // with the order still alive (`InferenceOrderBook.sol:1178-1183,218-221`). This
                // client cannot yet tell those apart from a slow read, so it claims neither a
                // removal nor a refund, and sends nothing a second time.
                bail!(
                    "cancel submitted for order {}, but its removal was not confirmed through the \
                     read timeout; no refund figure is claimed and no retry was sent",
                    order.order_id
                );
            };
            // the removal is confirmed on chain, so the local record of it may be corrected.
            // A subscription is an ordinary order in the book and this surface cancels it like any
            // other -- but the journal that says what a subscription is doing is the buyer's, and
            // until now nothing here told it. An operator who cancelled from here was then shown
            // `phase: resting` by the cheapest thing they read, about money that was already back.

            // It is done AFTER the confirmation and never before: the journal must record what the
            // chain did, not what this process attempted.

            // the result is REPORTED, never propagated. A `?` on this line would take the
            // confirmation below away from an operator whose money is already back.
            let (subscription_journal, journal_not_closed) = subscription_journal_report(
                order.order_id,
                crate::cli::buyer::mark_cancelled_subscription_order_terminal(
                    note_addr,
                    &snapshot.order_book,
                    order.order_id,
                ),
            );
            println!(
                "cancel confirmed model={} order_book={} order_id={} owner={} refund={} balance_before={} balance_after={} subscription_journal={}",
                model_label(&snapshot.frame_model, &snapshot.model_hash),
                addr::display(&snapshot.order_book),
                order.order_id,
                addr::display(note_addr),
                dexdo_core::shell_amount(refund),
                dexdo_core::shell_amount(balance_before),
                dexdo_core::shell_amount(balance_after),
                subscription_journal
            );
            if let Some(reason) = journal_not_closed {
                eprintln!("{reason}");
            }
        }
        OrdersCommand::Expire { order_id } => {
            // Permissionless: `expireOrder` accepts its own external message and is not owner-
            // authenticated (`contracts/airegistry/InferenceOrderBook.sol:1686`), so unlike
            // `cancel` this action signs nothing and needs no `--note-key`.
            match expire_action(own.iter().find(|o| o.order_id == order_id).copied(), as_of) {
                ExpireAction::NotResting => {
                    println!(
                        "expire noop model={} order_book={} order_id={order_id} owner={} resting=false submitted=false",
                        model_label(&snapshot.frame_model, &snapshot.model_hash),
                        addr::display(&snapshot.order_book),
                        addr::display(note_addr)
                    );
                }
                ExpireAction::StillLive { deadline } => {
                    bail!("{}", expire_too_early(order_id, deadline, as_of))
                }
                ExpireAction::Sweep => {
                    let note = dexdo_core::Address::parse(note_addr)
                        .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
                    let book = dexdo_core::Address::parse(&snapshot.order_book)
                        .map_err(|e| anyhow::anyhow!("order book {}: {e}", snapshot.order_book))?;
                    let frame_model =
                        model_label(&snapshot.frame_model, &snapshot.model_hash).to_string();
                    let order_book = snapshot.order_book.clone();
                    let balance_before =
                        budget.read(chain.private_note_shell_balance(&note)).await?;
                    chain.expire_inference_order(&book, order_id).await?;
                    println!(
                        "expire submitted model={frame_model} order_book={} order_id={order_id} owner={}",
                        addr::display(&order_book),
                        addr::display(note_addr)
                    );
                    let confirmed = reconcile_order_removal(
                        &chain,
                        &order_book,
                        &note,
                        order_id,
                        balance_before,
                        std::time::Duration::from_secs(args.read_timeout.read_timeout_secs),
                    )
                    .await?;
                    let Some((refund, balance_after)) = confirmed else {
                        bail!(
                            "expire submitted for order {order_id}, but its removal was not \
                             confirmed through the read timeout; no refund figure is claimed and \
                             no retry was sent"
                        );
                    };
                    // a swept expiry removes the order exactly as a cancel does, and the
                    // journal is as wrong afterwards. Same discipline as the cancel arm -- only
                    // after `reconcile_order_removal` confirmed the removal on chain.

                    // reported, not propagated -- same reason as the cancel arm.
                    let (subscription_journal, journal_not_closed) = subscription_journal_report(
                        order_id,
                        crate::cli::buyer::mark_cancelled_subscription_order_terminal(
                            note_addr,
                            &order_book,
                            order_id,
                        ),
                    );
                    println!(
                        "expire confirmed model={frame_model} order_book={} order_id={order_id} owner={} refund={} balance_before={} balance_after={} subscription_journal={}",
                        addr::display(&order_book),
                        addr::display(note_addr),
                        dexdo_core::shell_amount(refund),
                        dexdo_core::shell_amount(balance_before),
                        dexdo_core::shell_amount(balance_after),
                        subscription_journal
                    );
                    if let Some(reason) = journal_not_closed {
                        eprintln!("{reason}");
                    }
                }
            }
        }
        OrdersCommand::CancelAll => {
            if own.is_empty() {
                bail!(
                    "refusing to cancel-all: note {} has no resting orders in {}",
                    addr::display(note_addr),
                    addr::display(&snapshot.order_book)
                );
            }
            let secret = crate::cli::support::note_owner_secret_for(
                args.identity.note_key.as_deref(),
                note_addr,
                None,
                "orders cancel-all",
                "the key that signs the note's owner method",
            )?;
            let note = dexdo_core::Address::parse(note_addr)
                .map_err(|e| anyhow::anyhow!("--note-addr {note_addr}: {e}"))?;
            let keys = dexdo_core::KeyPair::from_secret_hex(secret.trim())
                .map_err(|e| anyhow::anyhow!("--note-key (SDK secret hex): {e:?}"))?;
            budget
                .read(chain.assert_note_owner_matches("orders cancel-all", &note, &keys))
                .await?;
            chain
                .cancel_all_inference_orders(&note, &keys, &target.model_hash)
                .await?;
            println!(
                "cancel-all submitted model={} order_book={} owner={} order_count={}",
                model_label(&snapshot.frame_model, &snapshot.model_hash),
                addr::display(&snapshot.order_book),
                addr::display(note_addr),
                own.len()
            );
        }
    }
    Ok(())
}

/// Where the production text ends, for every scanner in this FILE.

/// the attribute ALONE is not a boundary. `#[cfg(test)]` also
/// gates `#[path = "orders_1659_tests.rs"] mod orders_1659_tests;` near the top of this file, and
/// `split_once` stops at the FIRST match -- so the window collapsed from 49% of the file to 9%,
/// leaving every match arm and every display site outside it. The scans then reported absence, and
/// checks failed naming strings nobody had touched. Anchoring on the module line as well makes the
/// width independent of how many gated modules are declared, above this point or below it.

/// AT FILE LEVEL, not inside one test module, and that placement is the point. The boundary was
/// described two ways in this file at once -- repaired here, broken in
/// `issue_1554_a_failed_journal_write_is_an_outcome_not_a_refusal` -- and a third copy would only
/// decide which of them the next author happens to read. The boundary gets ONE owner per file.
#[cfg(test)]
const PRODUCTION_ENDS_AT: &str = "#[cfg(test)]\nmod tests";

#[cfg(test)]
mod tests {
    use super::*;

    /// - the load-bearing check of that PR, and the one it was missing.

    /// The repair is a call inside the `cancel` arm, and the arm cannot run offline: it needs a
    /// chain to fold the book, sign, submit and reconcile. So the fact is pinned where it lives --
    /// in the production text -- and what is pinned is the ORDER, because the order is the whole
    /// discipline: the journal must record what the chain did, never what this process attempted.
    /// Remove the call and this fails; move it above `reconcile_order_removal` and it fails too.

    /// What it does NOT prove: that the call reaches a journal on disk. That is
    /// `mark_cancelled_subscription_order_terminal_at`, driven against a real file in
    /// `crates/dexdo/src/cli/buyer.rs::issue_1547_both_cancel_surfaces_share_the_journal`.
    #[test]
    fn issue_1547_both_confirming_arms_correct_the_journal_after_the_chain_confirms() {
        let production = include_str!("orders.rs")
            .split_once(PRODUCTION_ENDS_AT)
            .expect("orders unit-test module boundary")
            .0;

        for (arm, label) in [
            ("OrdersCommand::Cancel { order_id }", "cancel"),
            ("OrdersCommand::Expire { order_id }", "expire"),
        ] {
            let body = production.split_once(arm).expect("the arm").1;
            let end = body.find("\n        OrdersCommand::").unwrap_or(body.len());
            let body = &body[..end];
            let confirms_at = body
                .find("reconcile_order_removal(")
                .unwrap_or_else(|| panic!("{label}: the arm must confirm the removal on chain"));
            let corrects_at = body
                .find("mark_cancelled_subscription_order_terminal(")
                .unwrap_or_else(|| {
                    panic!("{label}: the arm must correct the subscription journal ()")
                });
            assert!(
                confirms_at < corrects_at,
                "{label}: the journal is corrected BEFORE the chain confirmed the removal, so it \
                 would record an attempt instead of a fact"
            );
        }

        // `cancel-all` is deliberately NOT in that list, and the reason is the same discipline:
        // it prints "cancel-all submitted" and never reconciles, so there is no confirmed removal
        // to record. Writing the journal there would record an attempt. Pinned so the omission
        // stays a decision rather than becoming an oversight.
        let all = production
            .split_once("OrdersCommand::CancelAll =>")
            .expect("the cancel-all arm")
            .1;
        let all = &all[..all.find("\n    }").unwrap_or(all.len())];
        assert!(
            !all.contains("reconcile_order_removal("),
            "cancel-all now confirms removal; if so it must correct the journal too ()"
        );
        assert!(
            !all.contains("mark_cancelled_subscription_order_terminal"),
            "cancel-all must not write the journal while it cannot confirm what the chain did"
        );
    }

    /// the note-and-key route into `orders`.

    /// The half this closes is the sending half. `note outstanding` recovers a resting order as
    /// `modelHash` + `orderId`, and until now `orders` would take neither -- it wanted a model NAME
    /// (resolved through a `models.json` on disk) or a `market.json`, and `sha256(name)` does not run
    /// backwards. Everything past the input already worked on the hash.
    mod issue_1522_orders_take_the_model_id {
        const HASH: &str = "0x0000000000000000000000000000000000000000000000000000000000000abc";
        const NOTE: &str = "0:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

        /// The whole point: a target is built from the hash alone, reading nothing.

        /// `book_target_from_model_hash` takes no path and opens no file -- that is what makes this
        /// the route for an owner who has only the note and its key. The book is left unresolved
        /// because it is DERIVED from the hash on chain later, which is the step that never needed a
        /// name in the first place.
        #[test]
        fn a_target_is_built_from_the_hash_with_no_file_on_disk() {
            let target = super::super::book_target_from_model_hash(HASH, NOTE)
                .expect("a well formed model id names a book");
            assert_eq!(target.model_hash, HASH);
            assert_eq!(target.note_addr.as_deref(), Some(NOTE));
            assert!(
                target.order_book.is_none(),
                "the book is derived from the hash on chain, not carried in from a file"
            );
            assert!(
                target.frame_model.is_empty(),
                "this route does not know the name and must not invent one"
            );
        }

        /// Case and the `0x` prefix are the operator's to get wrong; the identity is not.
        #[test]
        fn the_hash_is_normalised_the_way_the_getter_wants_it() {
            let bare = super::super::book_target_from_model_hash(
                "0000000000000000000000000000000000000000000000000000000000000ABC",
                NOTE,
            )
            .expect("a bare uppercase id is the same id");
            assert_eq!(bare.model_hash, HASH);
        }

        /// A mistyped id is refused BEFORE any chain read and long before anything is signed.
        #[test]
        fn a_malformed_model_id_is_refused_at_the_input() {
            for bad in [
                "0xabc",
                "",
                "0xzz00000000000000000000000000000000000000000000000000000000000a",
                "not-a-hash",
            ] {
                let message = match super::super::book_target_from_model_hash(bad, NOTE) {
                    Ok(_) => panic!("a malformed model id must not reach the chain: {bad:?}"),
                    Err(error) => error.to_string(),
                };
                assert!(
                    message.contains("64 hex digits"),
                    "the refusal has to say what a model id looks like: {message}"
                );
                assert!(
                    message.contains("note outstanding"),
                    "and where to get a correct one: {message}"
                );
            }
        }

        /// The flag adds a way IN; it must not add a way AROUND. The guard that refuses to cancel an
        /// order this note does not own sits between the snapshot and the signature, and it reads
        /// the book's own rows -- so a hash that resolves to some other book yields no matching row
        /// and the command refuses instead of signing. Pinned on the production text because that
        /// ordering is one call site a later edit could quietly move.
        #[test]
        fn the_ownership_guard_still_stands_between_the_snapshot_and_the_signature() {
            let production = include_str!("orders.rs")
                .split_once(super::PRODUCTION_ENDS_AT)
                .expect("orders unit-test module boundary")
                .0;
            let cancel = production
                .split_once("OrdersCommand::Cancel { order_id }")
                .expect("the cancel arm")
                .1;
            let refuses_at = cancel
                .find("refusing to cancel: order")
                .expect("cancel refuses an order this note does not own");
            let signs_at = cancel
                .find("cancel_inference_order(")
                .expect("cancel signs the note's owner method");
            assert!(
                refuses_at < signs_at,
                "the ownership refusal must come before the signature, not after it"
            );
            assert!(
                cancel[..signs_at].contains("assert_note_owner_matches"),
                "and the key must be checked against the note before it signs"
            );
        }

        /// `fills` is the one reader of `frame_model`, and on this route there is no name to read.
        /// It gets the id instead of a blank -- an empty identity in a line about someone's money
        /// reads as a field that failed to fill.
        #[test]
        fn the_only_reader_of_the_name_is_given_the_id_instead_of_a_blank() {
            assert_eq!(super::super::model_label("", HASH), HASH);
            assert_eq!(
                super::super::model_label("qwen--qwen3--32b", HASH),
                "qwen--qwen3--32b"
            );

            let production = include_str!("orders.rs")
                .split_once(super::PRODUCTION_ENDS_AT)
                .expect("orders unit-test module boundary")
                .0;
            // BEFORE ANY "none found" ASSERTION, STATE WHAT WAS FOUND.

            // Both checks below are absences, and an absence over scanned text is true twice over:
            // when nothing offends, and when the scan has stopped finding anything at all. Rename
            // the binding or the field -- `snapshot` to `snap`, `frame_model` to `model_name` --
            // and `match_indices` returns nothing, the count is zero, and this test reports success
            // without having looked at a single display site. The floor is the same shape the
            // sweep carries in `main.rs` (`files >= 40`), moved to what THIS scan actually reads.

            // The floor counts the scanned substring itself, not `model_label(`: a field rename
            // leaves every `model_label(` in place, so counting those would survive exactly the
            // rename that blinds the scan.
            let reads = production.matches("snapshot.frame_model").count();
            assert!(
                reads >= 6,
                "the scan found {reads} reads of `snapshot.frame_model`, fewer than the 6 display \
                 sites this file carries: it is no longer reading the text it checks, so the \
                 zero-bare-reads assertion below would pass without looking at anything"
            );
            assert!(
                production.contains("model_label(&target.frame_model"),
                "the fills call no longer reads the label at all, so the absence checked next \
                 proves nothing about what fills renders"
            );
            assert!(
                !production.contains("render_orders_fills(&target.frame_model"),
                "fills must render the label, not the raw name that this route leaves empty"
            );
            // Every mention of the raw name must be the one INSIDE `model_label(...)`. Counting
            // bare occurrences would count those too -- `model_label(&snapshot.frame_model,` ends in
            // the very substring a naive check looks for -- so the reads are matched by what
            // precedes them.
            let bare = production
                .match_indices("snapshot.frame_model")
                .filter(|(at, _)| !production[..*at].ends_with("model_label(&"));
            assert_eq!(
                bare.count(),
                0,
                "no display site may print the raw name: every read goes through model_label"
            );
        }
    }

    /// The incident, to the second: SELL 11's deadline and the moment the operator read the
    /// book 779 seconds later, still being shown 956 ticks of liquidity no buyer could reach.
    const PAST_DEADLINE: u64 = 1_785_678_525;
    const AS_OF: u64 = 1_785_679_304;

    fn owner_note() -> String {
        format!("0:{}", "a".repeat(64))
    }

    fn folded_order(
        order_id: u128,
        is_buy: bool,
        deadline: u64,
    ) -> dexdo_core::chain::LiveBookOrder {
        dexdo_core::chain::LiveBookOrder {
            order_id,
            is_buy,
            price: 5_000_000_000,
            ticks_remaining: 956,
            note: owner_note(),
            token_contract: format!("0:{}", "b".repeat(64)),
            deadline,
            flags: 0,
            expired_by_event: false,
        }
    }

    fn owner_rows(orders: &[dexdo_core::chain::LiveBookOrder]) -> Vec<String> {
        let target = BookTarget {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "model-hash".to_string(),
            order_book: Some(format!("0:{}", "d".repeat(64))),
            root_model: None,
            note_addr: None,
        };
        let snapshot = fold_snapshot_from_orders(&target, &format!("0:{}", "d".repeat(64)), orders);
        // The production fold path, assembled the way `read_live_order_snapshot` assembles it: the
        // snapshot carries no escrow and the swept ids travel beside it, so these rows go through
        // the very `OrdersView::escrow_read` verdict `ROWS_CHAIN_EVENTS` reaches in production.
        let view = OrdersView {
            snapshot,
            rows: crate::cli::provenance::ROWS_CHAIN_EVENTS,
            last_update_id: "fold-13".to_string(),
            swept_order_ids: orders
                .iter()
                .filter(|order| order.expired_by_event)
                .map(|order| order.order_id)
                .collect(),
        };
        own_orders(&view.snapshot, &owner_note())
            .into_iter()
            .map(|order| render_order_line(order, AS_OF, view.escrow_read(order)))
            .collect()
    }

    fn fields(line: &str) -> std::collections::BTreeMap<&str, &str> {
        line.split_whitespace()
            .filter_map(|pair| pair.split_once('='))
            .collect()
    }

    /// the row carries `flags=`, and carries the fold's own value on the fold-backed path.

    /// The whole point of the field is that it distinguishes orders that are otherwise identical, so
    /// the assertion is that two rows differing ONLY in flags render differently. A `flags=` column
    /// that always printed the same number would satisfy "the field is present" and still tell the
    /// operator nothing -- that is the shape reports, a placement value replaced by a constant.

    /// Rendered through `owner_rows`, which is the fold path with an unswept `EscrowRead`:
    /// escrow is unknowable there and stays `-`, while flags is known and prints a number. Both
    /// halves are asserted so the two fields cannot be conflated back together.
    #[test]
    fn order_line_carries_the_folded_flags() {
        let shaped = dexdo_core::order_flags::AON | dexdo_core::order_flags::SUBSCRIPTION;
        let rows = owner_rows(&[
            dexdo_core::chain::LiveBookOrder {
                flags: shaped,
                ..folded_order(11, false, AS_OF + 3_600)
            },
            folded_order(12, false, AS_OF + 3_600),
        ]);

        let by_id = |want: &str| {
            rows.iter()
                .map(|row| fields(row))
                .find(|row| row.get("order_id") == Some(&want))
                .unwrap_or_else(|| panic!("row for order {want}: {rows:?}"))
        };
        assert_eq!(by_id("11").get("flags"), Some(&shaped.to_string().as_str()));
        assert_eq!(by_id("12").get("flags"), Some(&"0"));
        // The fold still cannot report escrow, and saying so is not the same as reporting a zero.
        assert_eq!(by_id("11").get("escrow"), Some(&"-"));
    }

    /// requirements 2 and 3: the operator cannot read `deadline=1785678280`. The row carries a
    /// date-time and says outright whether the order is past it, judged against the same `as_of` the
    /// context line prints.
    #[test]
    fn order_line_renders_a_human_deadline_and_the_expired_flag() {
        let expired = owner_rows(&[folded_order(11, false, PAST_DEADLINE)]);
        let expired = fields(&expired[0]);
        assert_eq!(expired.get("deadline"), Some(&"1785678525"));
        assert_eq!(expired.get("deadline_utc"), Some(&"2026-08-02T13:48:45Z"));
        assert_eq!(expired.get("expired"), Some(&"yes"));

        let live = owner_rows(&[folded_order(12, false, AS_OF + 3_600)]);
        let live = fields(&live[0]);
        assert_eq!(live.get("deadline_utc"), Some(&"2026-08-02T15:01:44Z"));
        assert_eq!(live.get("expired"), Some(&"no"));
    }

    /// 's explicit non-goal, and the deliberate opposite of what `market` does with the same
    /// row: an expired order may still be holding escrow, so the owner's own list keeps showing it.
    /// Hiding it would take the operator's money out of view.
    #[test]
    fn an_expired_order_stays_in_the_owner_list_flagged() {
        let rows = owner_rows(&[
            folded_order(11, false, PAST_DEADLINE),
            folded_order(13, false, AS_OF + 3_600),
        ]);

        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(fields(&rows[0]).get("order_id"), Some(&"11"));
        assert_eq!(fields(&rows[0]).get("expired"), Some(&"yes"));
        assert_eq!(fields(&rows[1]).get("expired"), Some(&"no"));
    }

    /// regression: "the book is still holding your money" and "your money is already back"
    /// stop rendering identically.

    /// Observed live on order_id=2: the row read `escrow=-... expired=yes` while the book had
    /// already swept it and the refund had landed at the note -- the same row a bid still holding its
    /// escrow renders. The two states call for OPPOSITE actions (`dexdo orders expire` on that
    /// order id versus nothing at all), and `expired=yes` is true in both because it is the clock's
    /// verdict, not the book's.

    /// The fold's own answer is what closes it: `InferenceOrderExpired` sets `expired_by_event`, and
    /// E2E-ORD-08 keep the row visible precisely BECAUSE the money may still be locked. Both
    /// halves are asserted together here, so a later change cannot buy the distinction back by
    /// dropping the swept row -- which would take the operator's money out of view for the case where
    /// it was never returned.
    #[test]
    fn an_expired_row_says_whether_the_book_still_holds_the_escrow() {
        let rows = owner_rows(&[
            // Past its deadline, nobody has swept it: the book is still holding this bid's escrow.
            folded_order(2, true, PAST_DEADLINE),
            // The book announced the sweep for this one; the escrow went back to the note.
            dexdo_core::chain::LiveBookOrder {
                expired_by_event: true,
                ..folded_order(3, true, PAST_DEADLINE)
            },
        ]);

        assert_eq!(
            rows.len(),
            2,
            "the swept row stays visible (): {rows:?}"
        );
        let by_id = |want: &str| {
            rows.iter()
                .map(|row| fields(row))
                .find(|row| row.get("order_id") == Some(&want))
                .unwrap_or_else(|| panic!("row for order {want}: {rows:?}"))
        };

        // The clock says the same thing about both, which is exactly why it cannot be the signal.
        assert_eq!(by_id("2").get("expired"), Some(&"yes"));
        assert_eq!(by_id("3").get("expired"), Some(&"yes"));

        // The escrow cell is where the owner's question is answered, and now it answers it.
        assert_eq!(
            by_id("2").get("escrow"),
            Some(&"-"),
            "an unswept row must still say the amount is unknown on this path, never `0`: {rows:?}"
        );
        assert_eq!(
            by_id("3").get("escrow"),
            Some(&"returned"),
            "a row the book swept must say the escrow is no longer held: {rows:?}"
        );
        assert_ne!(
            by_id("2"),
            by_id("3"),
            "locked and returned may not render as the same row: {rows:?}"
        );

        // ... and the verdict is the fold's, not the deadline's: a row the book has NOT swept never
        // claims the money came back, whatever the clock says.
        let live = owner_rows(&[folded_order(4, true, AS_OF + 3_600)]);
        assert_eq!(fields(&live[0]).get("escrow"), Some(&"-"), "{live:?}");
    }

    /// at the seam the report named: the fold's verdict reaches the row through the READ
    /// PATH's own carry, not through one restated beside it.

    /// `owner_rows` assembles an `OrdersView` field by field, `swept_order_ids` included, so
    /// everything built on it pins the renderer and only the renderer. Empty that set where
    /// `read_live_order_snapshot` derives it and every one of those tests stays green while the
    /// shipped command renders a swept bid exactly like a bid the book is still holding -- the whole
    /// of, back, and unobserved. So this drives `OrdersView::from_fold`, the one derivation
    /// the fold read path has, with the two rows the report is about: order 2 past its deadline
    /// with nobody having swept it, and one the book announced it swept.

    /// Both states, because either alone is satisfiable by a constant.
    #[test]
    fn the_read_paths_own_carry_separates_a_locked_row_from_a_returned_one() {
        let locked = folded_order(2, true, PAST_DEADLINE);
        let swept = dexdo_core::chain::LiveBookOrder {
            expired_by_event: true,
            ..folded_order(3, true, PAST_DEADLINE)
        };
        let order_book = format!("0:{}", "d".repeat(64));
        let target = BookTarget {
            frame_model: "qwen--qwen3--32b".to_string(),
            model_hash: "model-hash".to_string(),
            order_book: Some(order_book.clone()),
            root_model: None,
            note_addr: None,
        };

        let view = OrdersView::from_fold(
            &target,
            &order_book,
            &[&locked, &swept],
            "fold-13".to_string(),
        );
        let rows = own_orders(&view.snapshot, &owner_note())
            .into_iter()
            .map(|order| render_order_line(order, AS_OF, view.escrow_read(order)))
            .collect::<Vec<_>>();

        assert_eq!(
            rows.len(),
            2,
            "the swept row stays visible (): {rows:?}"
        );
        let by_id = |want: &str| {
            rows.iter()
                .map(|row| fields(row))
                .find(|row| row.get("order_id") == Some(&want))
                .unwrap_or_else(|| panic!("row for order {want}: {rows:?}"))
        };

        // The clock says the same of both, which is why it cannot be the signal.
        assert_eq!(by_id("2").get("expired"), Some(&"yes"), "{rows:?}");
        assert_eq!(by_id("3").get("expired"), Some(&"yes"), "{rows:?}");

        // The book is still holding this one: the owner's action is `dexdo orders expire 2`. `-`
        // also pins that this path stayed marked as the fold -- an authoritative marker over rows the
        // fold produced would print the filler `escrow=0` here, which reads as "nothing is held".
        assert_eq!(
            by_id("2").get("escrow"),
            Some(&"-"),
            "the read path must still say the amount is unknown on a row it holds: {rows:?}"
        );
        // The book announced the sweep for this one: the money is already back, nothing to do.
        assert_eq!(
            by_id("3").get("escrow"),
            Some(&"returned"),
            "the read path must carry the book's own sweep onto the row: {rows:?}"
        );
        assert_ne!(
            by_id("2"),
            by_id("3"),
            "locked and returned may not render as the same row: {rows:?}"
        );
    }

    /// The authoritative read path can never report a sweep, and must not stop reporting the number.

    /// `getOrder` has no expiry announcement to relay -- it simply stops returning a record the book
    /// removed -- so every row it produces is one the book still holds. `escrow=returned` there would
    /// be this client inventing a removal out of a read that cannot observe one.
    #[test]
    fn the_getter_path_never_reports_a_sweep() {
        let mut bid = order(9, 10_000, 3, "0:deal");
        bid.is_buy = true;
        bid.escrow = 30_744;
        let getters = view(crate::cli::provenance::ROWS_CHAIN_GETTERS, "-");
        assert_eq!(getters.escrow_read(&bid), EscrowRead::Authoritative);
        let row = render_order_line(&bid, AS_OF, getters.escrow_read(&bid));
        assert_eq!(fields(&row).get("escrow"), Some(&"0.000030744"), "{row}");

        // Same row id, same read path, with a swept id recorded: the path still decides, because a
        // getter that returned the record has already proved the book holds it.
        let mut with_sweep = view(crate::cli::provenance::ROWS_CHAIN_GETTERS, "-");
        with_sweep.swept_order_ids.insert(9);
        assert_eq!(with_sweep.escrow_read(&bid), EscrowRead::Authoritative);
    }

    /// One predicate for both views: a SELL with no deadline is malformed, never immortal
    /// liquidity, while a zero-deadline BUY is the contract's GTC bid and is not expired.
    #[test]
    fn a_zero_deadline_reads_by_side() {
        let sell = owner_rows(&[folded_order(14, false, 0)]);
        let sell = fields(&sell[0]);
        assert_eq!(sell.get("deadline"), Some(&"0"));
        assert_eq!(sell.get("deadline_utc"), Some(&"-"));
        assert_eq!(sell.get("expired"), Some(&"yes"));

        let buy = owner_rows(&[folded_order(15, true, 0)]);
        assert_eq!(fields(&buy[0]).get("expired"), Some(&"no"));
    }

    /// `dexdo orders expire` decides what it may do BEFORE it sends anything, and it decides with
    /// the same deadline predicate the `expired=` column an operator just read was rendered from.

    /// The premature arm is the money-relevant one: an order whose deadline has not passed is
    /// still holding escrow the book will not release, so the refusal names the deadline -- the
    /// operator learns WHEN this becomes possible, and that `cancel` is the way out before then.
    #[test]
    fn expire_refuses_before_the_deadline_and_names_it() {
        let mut live = order(21, 10_000, 3, "0:deal");
        live.deadline = AS_OF + 3_600;

        assert_eq!(
            expire_action(Some(&live), AS_OF),
            ExpireAction::StillLive {
                deadline: AS_OF + 3_600
            }
        );

        let refusal = expire_too_early(21, live.deadline, AS_OF);
        assert!(
            refusal.contains("order 21 is live until unix 1785682904")
                && refusal.contains("2026-08-02T15:01:44Z")
                && refusal.contains("3600 seconds after this read at unix 1785679304"),
            "the refusal must name the deadline that makes the sweep premature: {refusal}"
        );
        assert!(
            refusal.contains("no message was sent and no escrow moved")
                && refusal.contains("dexdo orders cancel 21"),
            "the refusal must say nothing moved and name the way out: {refusal}"
        );

        // A BUY may rest with the contract's GTC deadline 0. Nothing will ever sweep it, so
        // telling the operator to wait would be a lie.
        let mut gtc = live.clone();
        gtc.is_buy = true;
        gtc.deadline = 0;
        assert_eq!(
            expire_action(Some(&gtc), AS_OF),
            ExpireAction::StillLive { deadline: 0 }
        );
        let gtc_refusal = expire_too_early(21, 0, AS_OF);
        assert!(
            gtc_refusal.contains("carries deadline 0, which never expires")
                && !gtc_refusal.contains("live until"),
            "a GTC bid must not be described as merely not-yet-expired: {gtc_refusal}"
        );
    }

    /// `expireOrder` is permissionless and idempotent on chain -- a gone order is a silent no-op
    /// there -- so a replayed `dexdo orders expire` must read the same way. Reporting an error would
    /// make a safe repeat look like a failure; reporting a removal or a refund would invent a
    /// second one. Only an order past its deadline and still in the book is swept.
    #[test]
    fn expire_is_a_no_op_for_an_order_that_is_not_resting() {
        assert_eq!(expire_action(None, AS_OF), ExpireAction::NotResting);

        let mut swept = order(22, 10_000, 3, "0:deal");
        swept.deadline = PAST_DEADLINE;
        assert_eq!(expire_action(Some(&swept), AS_OF), ExpireAction::Sweep);

        // The boundary the contract draws: `_isExpired` is `block.timestamp >= deadline`, so the
        // deadline second itself is already past, and one second earlier is not.
        assert_eq!(
            expire_action(Some(&swept), PAST_DEADLINE),
            ExpireAction::Sweep
        );
        assert_eq!(
            expire_action(Some(&swept), PAST_DEADLINE - 1),
            ExpireAction::StillLive {
                deadline: PAST_DEADLINE
            }
        );
    }

    /// regression, re-armed: the order row carries `escrow`, and it carries it honestly.

    /// `cdd7d313` dropped `escrow=` from this formatter on 2026-07-12; `4cb8f496` restored it on
    /// 2026-08-02 but never landed on this branch, so the row shipped without it again. The first
    /// half below is that lost assertion, back on the real formatter.

    /// The second half is why the plain restore is not enough. `4cb8f496` printed `order.escrow`
    /// unconditionally, and the default read path is the event fold, which has no escrow to fold --
    /// `fold_snapshot_from_orders` fills a zero. Unconditional rendering therefore prints
    /// `escrow=0` over a bid that is really holding SHELL, which is worse than printing nothing:
    /// `0` is a legitimate reading (every SELL rests with it), so the operator cannot tell the
    /// filler from the fact. The fold path must say `-`.
    /// E2E-ROW: E2E-ORD-07/L0
    #[test]
    fn order_row_carries_escrow_and_never_prints_a_filler_zero_for_it() {
        // Getter path: `getOrder` returned the stored `Order`, so the number is the book's.
        let mut bid = order(7, 10_000, 3, "0:deal");
        bid.is_buy = true;
        bid.escrow = 30_744;
        let from_getters = render_order_line(&bid, AS_OF, EscrowRead::Authoritative);
        assert_eq!(
            fields(&from_getters).get("escrow"),
            Some(&"0.000030744"),
            "an authoritative read must show the escrow the bid is holding: {from_getters}"
        );

        // Fold path: same resting bid, read through the events, which never carried its escrow.
        // The struct's zero is a filler and must not be rendered as a quantity.
        let mut unknown = bid.clone();
        unknown.escrow = 0;
        let from_fold = render_order_line(&unknown, AS_OF, EscrowRead::HeldAmountUnknown);
        assert_eq!(
            fields(&from_fold).get("escrow"),
            Some(&"-"),
            "the event fold carries no escrow; it must not report the filler zero: {from_fold}"
        );

        // The live `orders list` default is exactly that fold path, so this is what an operator
        // sees today -- not a number, and specifically not `0`.
        assert_eq!(
            owner_rows(&[folded_order(7, true, 1_785_678_525)])
                .first()
                .map(|row| fields(row).get("escrow").copied()),
            Some(Some("-")),
            "the production fold-backed owner list must render escrow as unknown, not as zero"
        );
    }

    #[test]
    fn unix_utc_rendering_pins_known_instants() {
        assert_eq!(render_unix_utc(1), "1970-01-01T00:00:01Z");
        assert_eq!(render_unix_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(render_unix_utc(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(render_unix_utc(1_785_678_525), "2026-08-02T13:48:45Z");
        assert_eq!(render_unix_utc(2_147_483_647), "2038-01-19T03:14:07Z");
    }

    fn view(rows: &'static str, last_update_id: &str) -> OrdersView {
        OrdersView {
            snapshot: OrderBookSnapshot {
                frame_model: "openai/gpt-oss-20b".to_string(),
                model_hash: dexdo_core::model_hash_for("openai/gpt-oss-20b"),
                order_book: format!("0:{}", "d".repeat(64)),
                orders: Vec::new(),
                stats: None,
            },
            rows,
            last_update_id: last_update_id.to_string(),
            swept_order_ids: std::collections::BTreeSet::new(),
        }
    }

    /// third motivating example: `orders list` and `market` must both say where their rows
    /// came from, how fresh they are, and WHICH subset of the book they show -- in the same
    /// vocabulary -- so a divergence reads as indexer lag / a different scope, not as two
    /// contradictory truths.
    #[test]
    fn orders_annotates_its_source_freshness_and_scope() {
        let owner = format!("0:{}", "a".repeat(64));
        // The INPUT is the legacy spelling and the OUTPUT is the canonical one, because
        // `render_orders_context` canonicalises on purpose (: "what the client prints is one
        // form, and made that form the one every command reads back"). This test used to
        // interpolate `owner` -- its own input -- into both expectations, so it asserted that the
        // renderer echoed what it was handed, and it went red the moment the renderer started doing
        // its job. Written out as a literal rather than as `display(&owner)`: calling the canon to
        // state the expectation would make the assertion agree with whatever that function does,
        // which is not an assertion at all.

        // changed WHICH canonical form this is. The expectation was `<account>::<account>`,
        // the self-DApp reconstruction -- but the owner of a resting order is a note, and a note
        // lives in `DEXDO_DAPP_ID`. The old literal asserted that this command spelled a note
        // differently from its own neighbouring refusal, which prints `addr::display(note_addr)`.
        let owner_canonical = format!("{}::{}", dexdo_core::DEXDO_DAPP_ID, "a".repeat(64));
        assert_eq!(
            render_orders_context(
                &view(crate::cli::provenance::ROWS_CHAIN_EVENTS, "fold-13"),
                1_754_006_400,
                &owner
            ),
            format!(
                "orders source=chain lastUpdateId=fold-13 as_of=1754006400 \
                 rows=chain:order-book-events scope=owner-resting-orders owner={owner_canonical}"
            )
        );
        // The legacy getter fallback is a DIFFERENT source and says so.
        assert_eq!(
            render_orders_context(
                &view(crate::cli::provenance::ROWS_CHAIN_GETTERS, "-"),
                1_754_006_400,
                &owner
            ),
            format!(
                "orders source=chain lastUpdateId=- as_of=1754006400 rows=chain:getters \
                 scope=owner-resting-orders owner={owner_canonical}"
            )
        );
    }

    /// The price is given in whole SHELL: the book holds no other kind, and that is how it prints.
    fn order(order_id: u128, price_shell: u128, ticks: u128, tc: &str) -> OrderBookOrder {
        let price_per_tick =
            dexdo_core::price_raw_from_shell(price_shell).expect("whole SHELL price");
        OrderBookOrder {
            order_id,
            owner_note: format!("0:{}", "a".repeat(64)),
            token_contract: Some(tc.to_string()),
            is_buy: false,
            price_per_tick,
            ticks,
            escrow: 0,
            deadline: 1_785_678_280,
            flags: 0,
            timestamp: 0,
        }
    }

    /// The client renders one resting sell offer as one row carrying its whole identity tuple --
    /// order id, side, price, size, deadline and deal -- bound together rather than as independent
    /// substrings satisfiable across two different offers.

    /// E2E-ORD-01, tests/e2e/test-specification.md
    /// Partial: the row's L0 half -- the posted order appearing in the book getter with that exact
    /// tuple, and the seller's readiness bound to it -- needs the live book and is not observed
    /// here, and the adversary half (folding an unknown, stale or duplicated identity from the same
    /// snapshot) is not covered.
    #[test]
    fn one_resting_ask_renders_as_one_row_carrying_its_whole_tuple() {
        // Split one rendered row into whole key/value pairs. Comparing values as units is what
        // anchors every assertion below: as text, `order_id=11` sits inside `order_id=110` and
        // `ticks=956` sits inside `ticks=9560`, so a substring check cannot tell an offer from a
        // different offer whose fields merely start the same way.
        fn fields(line: &str) -> std::collections::BTreeMap<&str, &str> {
            line.split_whitespace()
                .filter_map(|pair| pair.split_once('='))
                .collect()
        }

        let tc_a_account = "1".repeat(64);
        let tc_a = format!("0:{tc_a_account}");
        let tc_b = format!("0:{}", "2".repeat(64));
        // Offer 11 holds the identifier; offer 12 holds the price and the size. Offer 110 is the
        // near-miss: same deal, same side, same deadline, and every one of its numbers extends the
        // matching number of offer 11 -- so it satisfies every loose substring an unanchored check
        // would look for, and only whole values tell the two apart.
        let first = order(11, 700, 956, &tc_a);
        let second = order(12, 900, 4, &tc_b);
        let near_miss = order(110, 7001, 9560, &tc_a);

        // The moment the book is observed. This row is about identity, not about time, so the
        // three offers are read while they are all still RESTING -- the same fixed observation stamp
        // the context-line tests in this module use, a year before the fixture deadline. A wall-clock
        // `now` would make the row's verdict, and this whole-row comparison, drift by the day.
        let as_of = 1_754_006_400_u64;
        // Getter-shaped rows: `getOrder` returns the stored `Order`, so their escrow is the book's.
        let lines = [
            render_order_line(&first, as_of, EscrowRead::Authoritative),
            render_order_line(&second, as_of, EscrowRead::Authoritative),
            render_order_line(&near_miss, as_of, EscrowRead::Authoritative),
        ];
        let rows: Vec<_> = lines.iter().map(|line| fields(line)).collect();

        // One comparison of the whole row: every value of the tuple is proven to belong to this
        // one record, in the rendered form, with nothing else on the row.
        // widened the row with the human deadline and the as_of-bound verdict, and restored
        // `flags`; all three belong to this record and are therefore part of the one whole-row
        // comparison, unchanged in kind. `flags=0` is this fixture's own recorded value, not a
        // placeholder -- `order_line_carries_the_folded_flags` is what proves a set value renders.
        // `owner` is a PrivateNote, a contract of the shared dexdo DApp; `token_contract` is a
        // per-deal TokenContract, a self-DApp account whose DApp half is its own account id.
        let expected = format!(
            "order_id=11 side=sell owner={} token_contract={tc_a_account}::{tc_a_account} \
             price_per_tick=700 ticks=956 escrow=0 flags=0 deadline=1785678280 \
             deadline_utc=2026-08-02T13:44:40Z expired=no",
            addr::display(&first.owner_note),
        );
        assert_eq!(
            lines[0], expected,
            "the whole tuple must be carried by one row, field for field"
        );
        let want = fields(&expected);

        // The near-miss row really is the trap this row exists to catch: its values contain offer
        // 11's without being them, so the equality above is doing work a `contains` would not.
        let near_miss_row = &rows[2];
        for key in ["order_id", "price_per_tick", "ticks"] {
            assert!(
                near_miss_row[key].starts_with(want[key]) && near_miss_row[key] != want[key],
                "the near-miss row must extend offer 11's {key} without equalling it: \
                 {} vs {}",
                near_miss_row[key],
                want[key]
            );
        }
        assert!(
            !rows[1..].contains(&want),
            "exactly one row carries this tuple: {}",
            lines.join("\n")
        );

        // The mixed-up combination is satisfied by the OUTPUT and by no single ROW.
        let mixed = [
            ("order_id", "11"),
            ("price_per_tick", "900"),
            ("ticks", "4"),
        ];
        assert!(
            mixed
                .iter()
                .all(|(key, value)| rows.iter().any(|row| row.get(*key) == Some(value))),
            "the mixed tuple must be reachable across the output, or this assertion proves nothing"
        );
        assert!(
            !rows.iter().any(|row| mixed
                .iter()
                .all(|(key, value)| row.get(*key) == Some(value))),
            "no single row may satisfy the mixed tuple: {}",
            lines.join("\n")
        );
    }

    /// `orders list` renders deadline as a human date-time and computes `expired=yes|no` against
    /// the exact `as_of` value in its context line. Zero, expired, future, and maximal/malformed
    /// boundary inputs may not masquerade as one another after field parsing.

    /// E2E-ORD-07, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ORD-07/L0
    #[ignore = "EXPECTED TO FAIL until order rows render a human expiry and an as_of-bound expired field"]
    #[test]
    fn ord_07_order_rows_render_human_expiry_and_expired_field() {
        let as_of = 1_754_006_400_u64;
        let owner = format!("0:{}", "a".repeat(64));
        let context = render_orders_context(
            &view(crate::cli::provenance::ROWS_CHAIN_EVENTS, "fold-13"),
            as_of,
            &owner,
        );
        let mut expired = order(701, 700, 4, "0:expired");
        expired.deadline = as_of - 1;
        let mut live = order(702, 700, 4, "0:live");
        live.deadline = as_of + 1;
        let mut zero = order(703, 700, 4, "0:zero");
        zero.deadline = 0;
        let mut maximal = order(704, 700, 4, "0:max");
        maximal.deadline = u64::MAX;

        let rendered = [expired, live, zero, maximal]
            .iter()
            .map(|order| render_order_line(order, as_of, EscrowRead::HeldAmountUnknown))
            .collect::<Vec<_>>();
        let parsed = rendered
            .iter()
            .map(|line| {
                line.split_whitespace()
                    .filter_map(|field| field.split_once('='))
                    .collect::<std::collections::BTreeMap<_, _>>()
            })
            .collect::<Vec<_>>();
        let human_dates = parsed.iter().all(|row| {
            row.get("expires_at")
                .is_some_and(|value| value.contains('T') && value.contains('-'))
        });
        let expiry_flags = parsed[0].get("expired") == Some(&"yes")
            && parsed[1].get("expired") == Some(&"no")
            && parsed[2].get("expired") == Some(&"yes")
            && parsed[3].get("expired") == Some(&"no");

        assert!(
            context.contains(&format!("as_of={as_of}")) && human_dates && expiry_flags,
            "E2E-ORD-07 missing capability: orders output lacks as_of-bound human expiry fields"
        );
    }

    /// The two views must be diffable key for key -- the same keys, in the same order.
    #[test]
    fn orders_and_market_annotations_share_one_vocabulary() {
        let owner = format!("0:{}", "a".repeat(64));
        let orders = render_orders_context(
            &view(crate::cli::provenance::ROWS_CHAIN_EVENTS, "fold-13"),
            1_754_006_400,
            &owner,
        );
        let market = crate::cli::provenance::render(
            "indexer",
            "indexer-77",
            1_754_006_400,
            crate::cli::provenance::ROWS_CHAIN_EVENTS,
            crate::cli::provenance::SCOPE_EXECUTABLE_ASKS,
        );
        let keys = |line: &str| {
            line.split_whitespace()
                .filter_map(|pair| pair.split_once('='))
                .map(|(key, _)| key.to_string())
                .collect::<Vec<_>>()
        };
        // `orders` adds `owner=`; the shared prefix is identical.
        let orders_keys = keys(&orders);
        assert_eq!(orders_keys[..5], keys(&market)[..]);
        assert_eq!(orders_keys.last().map(String::as_str), Some("owner"));
        // Same scope key, different value -- the reason the row sets differ.
        assert!(orders.contains("scope=owner-resting-orders"), "{orders}");
        assert!(market.contains("scope=executable-asks"), "{market}");
    }
}

/// a journal write that fails must not take away the confirmation of a settled cancellation.

/// Gated on `test` alone and not on the chain build, so these run in `cargo test --workspace` -- the gate
/// CI actually executes. The the chain tier is compiled there with `--no-run`, and the defect
/// this closes is one an operator meets on the money path.
#[cfg(test)]
mod issue_1554_a_failed_journal_write_is_an_outcome_not_a_refusal {
    use super::{subscription_journal_report, PRODUCTION_ENDS_AT};

    /// The `try_acquire` refusal, word for word from `buyer.rs:1050-1056`. It is the first of the
    /// three sources and the worst: it fires exactly when a buyer is running on this note, and it
    /// says "no BOC was sent" to an operator whose BOC was sent, accepted and confirmed.
    fn the_lock_refusal() -> anyhow::Error {
        anyhow::anyhow!(
            "buyer note 0:1111 already has another money submission awaiting by-fact \
             reconciliation; no BOC was sent (/data/note.money: pool lock is already held)"
        )
    }

    /// The two outcomes that existed keep their exact words, because operators and scripts read
    /// this field: a repair that renamed them would be a second defect.
    #[test]
    fn the_two_outcomes_that_already_existed_are_unchanged() {
        assert_eq!(subscription_journal_report(7, Ok(true)).0, "closed");
        assert_eq!(subscription_journal_report(7, Ok(false)).0, "none");
        assert!(subscription_journal_report(7, Ok(true)).1.is_none());
        assert!(subscription_journal_report(7, Ok(false)).1.is_none());
    }

    /// The repair itself: a failed write yields a value to PRINT, not an error to propagate, and it
    /// is distinguishable from both of the outcomes that already existed. Before this call
    /// site was `?`, so there was no third value to ask for -- the line was never reached.
    #[test]
    fn a_journal_that_could_not_be_written_is_a_third_printable_outcome() {
        let (field, reason) = subscription_journal_report(4242, Err(the_lock_refusal()));
        assert_eq!(
            field, "not-closed",
            "a failed journal write must not be reported as either of the outcomes that succeeded"
        );
        assert_ne!(field, "closed");
        assert_ne!(field, "none");
        assert!(
            reason.is_some(),
            "the operator is told the field's value and never why it has that value"
        );
    }

    /// The reason says the chain fact first, names the order, carries the underlying failure, and
    /// warns that the failure's own words were written for a path that submits nothing. That last
    /// sentence is the whole point: the operator is about to read "no BOC was sent" about a BOC
    /// that was sent.
    #[test]
    fn the_reason_states_the_chain_fact_before_it_repeats_a_message_written_for_another_path() {
        let (_, reason) = subscription_journal_report(4242, Err(the_lock_refusal()));
        let reason = reason.expect("a failed write carries its reason");
        for expected in [
            "order_id=4242",
            "confirmed on chain",
            "only the write to this note's local subscription journal",
            "dexdo subscription status",
            "do not describe this one",
            // the underlying failure survives intact -- it is what the operator acts on
            "no BOC was sent",
            "pool lock is already held",
        ] {
            assert!(
                reason.contains(expected),
                "the journal-failure reason is missing {expected:?}: {reason}"
            );
        }
    }

    /// The wiring, in the production text, because the arms need a chain and cannot run offline.

    /// What is pinned is not an order of lines but a SHAPE: the fallible correction is an ARGUMENT
    /// of the report, and no `?` stands between the report and the confirmation `println!`. Restore
    /// the `?` in either form -- on the correction, or on the report -- and this fails.
    #[test]
    fn neither_confirming_arm_propagates_the_journal_failure_past_its_confirmation() {
        let production = include_str!("orders.rs")
            .split_once(PRODUCTION_ENDS_AT)
            .expect("orders unit-test module boundary")
            .0;

        // The window has to prove it can find anything before its emptiness is read as evidence.

        // This is what the defect actually did: it did not refuse, it reported ABSENCE. A second
        // gated module declared near the top of the file moved the boundary from line 909 to 163,
        // the `Cancel` arm at 788 fell outside, and the scan below announced a missing string that
        // nobody had touched. A collapsed window and a genuinely missing arm look identical from
        // here, so the window is checked against a landmark that must be inside it. Now a collapse
        // fails loudly and by its own cause.
        assert!(
            production.contains("OrdersCommand::Cancel { order_id }"),
            "the production window does not contain the Cancel arm, so it is not the production \
             text: the boundary anchor matched something else and every check below would report \
             absence it cannot distinguish from a collapsed window ({} bytes of {})",
            production.len(),
            include_str!("orders.rs").len()
        );

        for (arm, printed) in [
            ("OrdersCommand::Cancel { order_id }", "\"cancel confirmed"),
            ("OrdersCommand::Expire { order_id }", "\"expire confirmed"),
        ] {
            let body = production.split_once(arm).expect("the arm").1;
            let end = body.find("\n        OrdersCommand::").unwrap_or(body.len());
            let body = &body[..end];
            let reports_at = body
                .find("subscription_journal_report(")
                .unwrap_or_else(|| panic!("{arm}: the journal outcome must be reported, not `?`d"));
            let prints_at = body
                .find(printed)
                .unwrap_or_else(|| panic!("{arm}: the arm must print its confirmation"));
            assert!(
                reports_at < prints_at,
                "{arm}: the confirmation is printed before the journal outcome it reports"
            );
            let folded = &body[reports_at..prints_at];
            assert!(
                folded.contains("mark_cancelled_subscription_order_terminal("),
                "{arm}: the correction is not the argument this report is built from: {folded}"
            );
            assert!(
                !folded.contains('?'),
                "{arm}: a `?` between the journal correction and the confirmation takes the \
                 confirmation away from an operator whose refund has already landed: {folded}"
            );
        }
    }

    /// the argument check comes BEFORE the manifest is read.

    /// Hoisting `manifest_path()` to the top of the command to stop re-reading it put an
    /// environment read in front of the `--note-addr` refusal, so a command that was going to be
    /// refused on its arguments reported a missing deployment instead. The sibling guards in
    /// `buyer.rs` and `markets.rs` cover their own early returns; this one was left uncovered, and
    /// the review found it there.
    #[test]
    fn the_note_addr_check_comes_before_the_manifest_is_read() {
        let body = crate::cli::source_probe::code_of(
            include_str!("orders.rs"),
            "pub(crate) async fn run_orders(args: OrdersArgs)",
        );
        let argument = body
            .find("orders requires --note-addr")
            .expect("the argument refusal is in this command");
        let manifest = body
            .find("let manifest_path = crate::cli::commands::manifest_path()?;")
            .expect("the manifest path is bound once");
        assert!(
            argument < manifest,
            "a missing --note-addr would be reported as a missing manifest: the manifest is read \
             at {manifest}, before the argument check at {argument}"
        );
    }
}
