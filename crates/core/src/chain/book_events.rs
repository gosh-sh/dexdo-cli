//! Authoritative live-order state folded from `InferenceOrderBook` ext-out events.

use crate::address::display_self_dapp;
use crate::market::order_deadline_is_live;
use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use gosh_ackinacki::sdk::Address;
use gosh_ackinacki::wallet::query::fetch_dapp_id;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use tvm_abi::token::TokenValue;
use tvm_abi::{Contract, Event};
use tvm_types::SliceData;

use super::client::{
    fetch_all_ext_out_messages, fetch_ext_out_page, is_transient_transport_failure, ExtOutPage,
    RequestGate,
};
use super::contracts_provision::INFERENCE_ORDERBOOK_ABI;
use crate::market::{BuyerOrderFact, BuyerOrderFactKind};

/// One order reconstructed from the book event stream.

/// The fold holds an order until a **terminal** event takes it out of the book for good -- a cancel or
/// the fill that consumes its last tick. `InferenceOrderExpired` is not one of those: it sets
/// [`LiveBookOrder::expired_by_event`] and the row stays, because the two views built on this fold have
/// opposite jobs. `dexdo market` must never offer an expired order; `dexdo orders list` must
/// still show its owner an order that may be sitting in the book holding escrow (, whose explicit

/// records the asymmetry as intended rather than as a bug in either view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveBookOrder {
    pub order_id: u128,
    pub is_buy: bool,
    pub price: u128,
    pub ticks_remaining: u128,
    pub note: String,
    pub token_contract: String,
    pub deadline: u64,
    /// The order's deal-shape flags exactly as `InferenceOrderPlaced` announced them.

    /// Unlike escrow -- which the placement event does not carry at all, so the fold has nothing to
    /// report and says `-` -- `flags` is a declared `uint8` field of the event
    /// (`InferenceOrderBook.abi.json`, `InferenceOrderPlaced`), so folding it is reporting what the
    /// book said rather than filling a number in. It decides what the order IS (AON, IOC,
    /// subscription, TEE), and an operator reading a row cannot tell an all-or-nothing order from an
    /// ordinary one without it.
    pub flags: u8,
    /// `InferenceOrderExpired` named this exact order id: the book has removed it and (for a bid)
    /// refunded its escrow. Distinguishes an order the book itself expired from one merely hidden by
    /// the deadline predicate, which per E2E-CXL-01 may sit in the book indefinitely because expiry is
    /// lazy and nobody has called `expireOrder`.
    pub expired_by_event: bool,
}

impl LiveBookOrder {
    /// Still matchable at `now_unix`: the book has not expired it and its deadline has not passed.
    pub fn is_live_at(&self, now_unix: u64) -> bool {
        !self.expired_by_event && order_deadline_is_live(self.is_buy, self.deadline, now_unix)
    }
}

/// One `InferenceFilled` identity record named by a buyer note.

/// This is a candidate, not a recovery result: the event says which deal was created, but only the
/// [`RealChainBackend`](super::client::RealChainBackend) wrapper returns it after the named
/// `TokenContract` still agrees through `getParties` and reports `funded=true` through `getState`.
/// `clearing_price` is kept as an exact decimal string because the compiled ABI declares a full
/// `uint256`, while the remaining numeric fields are `uint128`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookFillCandidate {
    pub maker_id: u128,
    pub taker_id: u128,
    pub ticks: u128,
    pub clearing_price: String,
    pub seller_token_contract: String,
    pub buyer_note: String,
    pub seller_note: String,
}

/// Incremental fold state. Pass a previous value back to avoid replaying known history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookEventFold {
    orders: BTreeMap<u128, LiveBookOrder>,
    cancel_rejections: BTreeMap<(u128, String), u8>,
    last_seen_id: Option<String>,
}

impl BookEventFold {
    /// Orders still matchable at `now_unix` -- the only set a view may present as live or executable.

    /// Takes the observation time rather than reading the clock itself so a view cannot accidentally
    /// answer "live" for one instant and render for another.
    pub fn live_orders_at(&self, now_unix: u64) -> impl Iterator<Item = &LiveBookOrder> {
        self.orders
            .values()
            .filter(move |order| order.is_live_at(now_unix))
    }

    /// Every order the fold still holds, expired ones included -- the owner-facing set.
    pub fn all_orders(&self) -> impl Iterator<Item = &LiveBookOrder> {
        self.orders.values()
    }

    pub fn live_sell_for_token_contract_at(
        &self,
        token_contract: &str,
        now_unix: u64,
    ) -> Option<&LiveBookOrder> {
        self.live_orders_at(now_unix)
            .find(|order| !order.is_buy && order.token_contract == token_contract)
    }

    pub fn last_seen_id(&self) -> Option<&str> {
        self.last_seen_id.as_deref()
    }

    /// Start an incremental read immediately after an already-observed ext-out message.
    pub fn after_event_marker(event_marker: Option<String>) -> Self {
        Self {
            last_seen_id: event_marker,
            ..Self::default()
        }
    }

    /// Exact terminal cancel rejection observed in this fold interval.
    pub fn cancel_rejection_reason(&self, order_id: u128, owner_note: &str) -> Option<u8> {
        self.cancel_rejections
            .iter()
            .find_map(|((seen_order_id, seen_owner), reason)| {
                (*seen_order_id == order_id && seen_owner.eq_ignore_ascii_case(owner_note))
                    .then_some(*reason)
            })
    }

    fn apply(&mut self, event: BookEvent) {
        match event {
            BookEvent::Placed(order) => {
                self.orders.insert(order.order_id, order);
            }
            BookEvent::Cancelled { order_id } => {
                self.orders.remove(&order_id);
            }
            BookEvent::CancelRejected {
                order_id,
                reason,
                note,
            } => {
                self.cancel_rejections.insert((order_id, note), reason);
            }
            BookEvent::Filled {
                maker_id,
                taker_id,
                ticks,
            } => {
                // `InferenceFilled` names BOTH sides of the trade. Folding only `makerId` left an
                // order that filled as the TAKER resting in the view forever -- a fill is its only exit
                // and the fold never applied it, so `orders list` kept showing a FILLED order.
                self.fill(maker_id, ticks);
                self.fill(taker_id, ticks);
            }
            BookEvent::Expired {
                order_id,
                is_buy,
                note,
                token_contract,
            } => {
                let Some(order) = self.orders.get_mut(&order_id) else {
                    // `expireOrder` is permissionless and silent in both failure directions
                    // (`InferenceOrderBook.sol:1516-1519`), so an expiry naming an order this fold
                    // never held -- or already removed -- is a no-op, not an error.
                    return;
                };
                tracing::debug!(
                    order_id = %order_id,
                    is_buy,
                    note = %crate::address::display(&note),
                    token_contract = %display_self_dapp(&token_contract),
                    "order-book expiry event applied"
                );
                // Idempotent by construction: a duplicate or overlapping replay of the same expiry
                // sets the same flag and removes the order from the live set exactly once.
                order.expired_by_event = true;
            }
        }
    }

    /// Apply one side of a fill the way the book itself does -- by that order's OWN side.

    /// A SELL is a one-deal slot and leaves the book WHOLE on any match, partial included, in
    /// either role. As the MAKER, `InferenceOrderBook.sol:1087-1090` calls `_removeFromBook(cur)`
    /// without ever looking at how much was taken:

    /// ```solidity
    /// SELL offer = one-deal slot -> consumed on match (taker BUY), even
    /// on partial. BUY maker (taker SELL) is reduced (spans deals).
    /// if (takerIsBuy) {
    /// _removeFromBook(cur); // maker SELL: no buyer escrow to return
    /// ```

    /// As the TAKER, `:1103-1104` forces `remaining = 0` after its single fill, and
    /// `_finalizeTaker:1191-1201` rests an ask only while `remaining > 0` -- so a filled taker SELL
    /// is never re-inserted either. Only a BUY spans deals and is REDUCED: the maker BUY by
    /// `:1091-1097` (removed, with its residual escrow refunded, once `mk.amount == trade`), the
    /// taker BUY by `:1099` and the `amount: remaining` it rests with at `:1184-1190`.

    /// decrementing both sides alike left a partially-filled ask resting in this view with a
    /// positive remainder that no longer existed on chain. A fill is an ask's only exit here, so
    /// nothing could ever take it out again -- no cancel was sent, no deadline had passed -- and
    /// `market` went on offering liquidity no buyer could take while `orders list` showed its owner
    /// an order they could not cancel.
    fn fill(&mut self, order_id: u128, ticks: u128) {
        let empty = self.orders.get_mut(&order_id).is_some_and(|order| {
            if !order.is_buy {
                return true;
            }
            order.ticks_remaining = order.ticks_remaining.saturating_sub(ticks);
            order.ticks_remaining == 0
        });
        if empty {
            self.orders.remove(&order_id);
        }
    }
}

/// One raw ext-out event supplied by a page reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookEventMessage {
    pub id: String,
    pub created_at: u64,
    pub cursor: String,
    pub body: String,
}

/// One newest-to-oldest GraphQL page. `previous_cursor` requests the next older page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookEventPage {
    pub messages: Vec<BookEventMessage>,
    pub previous_cursor: Option<String>,
}

#[cfg_attr(test, derive(Debug))]
enum BookEvent {
    Placed(LiveBookOrder),
    Cancelled {
        order_id: u128,
    },
    CancelRejected {
        order_id: u128,
        reason: u8,
        note: String,
    },
    Filled {
        maker_id: u128,
        taker_id: u128,
        ticks: u128,
    },
    /// The book removed an order that had passed its deadline. Its side, owner note and
    /// TokenContract are carried for diagnostics: they say WHICH order the book dropped and,
    /// for a bid, that its escrow has already been refunded.
    Expired {
        order_id: u128,
        is_buy: bool,
        note: String,
        token_contract: String,
    },
}

/// Fold pages returned by an async closure. The closure receives the GraphQL `before` cursor.

/// Pages may overlap. Message ids are deduplicated, events are applied in chronological order, and
/// an existing fold stops once its `last_seen_id` is reached. A missing prior id fails closed rather
/// than replaying incomplete history into an existing state.
pub async fn fold_book_event_pages<F, Fut>(
    mut fold: BookEventFold,
    mut fetch_page: F,
) -> Result<BookEventFold>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<BookEventPage>>,
{
    let since_id = fold.last_seen_id.clone();
    let mut before = None;
    let mut found_since = false;
    let mut seen_ids = BTreeSet::new();
    let mut messages = Vec::new();

    loop {
        let page = fetch_page(before.clone()).await?;
        for message in page.messages {
            if since_id.as_deref() == Some(message.id.as_str()) {
                found_since = true;
            }
            if seen_ids.insert(message.id.clone()) {
                messages.push(message);
            }
        }
        if since_id.is_some() && found_since {
            break;
        }
        let Some(previous) = page.previous_cursor else {
            break;
        };
        if before.as_deref() == Some(previous.as_str()) {
            return Err(anyhow!("order-book ext-out pagination made no progress"));
        }
        before = Some(previous);
    }

    if since_id.is_some() && !found_since {
        return Err(anyhow!(
            "order-book ext-out history no longer contains last-seen id {}",
            since_id.as_deref().unwrap_or_default()
        ));
    }

    messages.sort_by(|left, right| {
        (left.created_at, &left.cursor).cmp(&(right.created_at, &right.cursor))
    });
    let newest_id = messages.last().map(|message| message.id.clone());
    let start = since_id
        .as_deref()
        .and_then(|id| messages.iter().position(|message| message.id == id))
        .map_or(0, |position| position + 1);
    for message in messages.into_iter().skip(start) {
        if let Some(event) = decode_book_event(&message.body)? {
            fold.apply(event);
        }
    }
    if newest_id.is_some() {
        fold.last_seen_id = newest_id;
    }
    Ok(fold)
}

pub(super) async fn read_book_event_fold(
    gate: &RequestGate,
    http: &reqwest::Client,
    endpoint: &str,
    account_id: &str,
    fold: BookEventFold,
) -> Result<BookEventFold> {
    // review finding 1: this dapp-id lookup is a REQUEST and it was going out ungated -- and
    // invisibly so, because it is neither a `.client()` site nor a new `http: &reqwest::Client`
    // signature, so neither ratchet counter would ever have shown it.
    gate.admit().await;
    let dapp_id = fetch_dapp_id(http, endpoint, account_id).await?;
    fold_book_event_pages(fold, |before| {
        let dapp_id = dapp_id.clone();
        async move {
            for delay in crate::params::BOOK_EVENT_READ_BACKOFFS {
                match fetch_ext_out_page(
                    gate,
                    http,
                    endpoint,
                    account_id,
                    &dapp_id,
                    crate::params::BOOK_EVENT_PAGE_SIZE,
                    before.as_deref(),
                )
                .await
                {
                    Ok(page) => return Ok(book_event_page(page)),
                    Err(error) if is_transient_transport_failure(&error) => {
                        tokio::time::sleep(delay).await;
                    }
                    Err(error) => return Err(error),
                }
            }
            fetch_ext_out_page(
                gate,
                http,
                endpoint,
                account_id,
                &dapp_id,
                crate::params::BOOK_EVENT_PAGE_SIZE,
                before.as_deref(),
            )
            .await
            .map(book_event_page)
        }
    })
    .await
}

/// Read full-ABI fill identities for one normalized buyer note without retaining unrelated fill
/// identities. The shared ext-out pager supplies each deduplicated message to the decoder while it
/// walks history; only matching `InferenceFilled` records enter the returned candidate vector.
pub(super) async fn read_book_fill_candidates(
    gate: &RequestGate,
    http: &reqwest::Client,
    endpoint: &str,
    account_id: &str,
    buyer_note: &str,
) -> Result<Vec<BookFillCandidate>> {
    let buyer_note = normalize_book_address(buyer_note)
        .with_context(|| format!("normalize requested buyer note {buyer_note}"))?;
    fetch_all_ext_out_messages(gate, http, endpoint, account_id, move |message| {
        decode_book_fill_candidate(&message.body, &buyer_note)
            .with_context(|| format!("decode InferenceOrderBook event {}", message.id))
    })
    .await
}

fn book_event_page(page: ExtOutPage) -> BookEventPage {
    BookEventPage {
        messages: page
            .messages
            .into_iter()
            .map(|message| BookEventMessage {
                id: message.id,
                created_at: message.created_at,
                cursor: message.cursor,
                body: message.body,
            })
            .collect(),
        previous_cursor: page.previous_cursor,
    }
}

fn decode_book_event_input(
    body_b64: &str,
    accepted_names: &[&str],
) -> Result<Option<(String, Vec<tvm_abi::Token>)>> {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(body_b64.trim()) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let cell = match tvm_types::read_single_root_boc(&bytes) {
        Ok(cell) => cell,
        Err(_) => return Ok(None),
    };
    let slice = match SliceData::load_cell(cell) {
        Ok(slice) => slice,
        Err(_) => return Ok(None),
    };
    let id = match Event::decode_id(slice.clone()) {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    let contract = Contract::load(INFERENCE_ORDERBOOK_ABI.as_bytes())
        .map_err(|error| anyhow!("load InferenceOrderBook ABI: {error}"))?;
    let event = match contract.event_by_id(id) {
        Ok(event) => event,
        Err(_) => return Ok(None),
    };
    if !accepted_names.contains(&event.name.as_str()) {
        return Ok(None);
    }
    let tokens = event
        .decode_input(slice, true)
        .map_err(|error| anyhow!("decode {} body: {error}", event.name))?;
    Ok(Some((event.name.clone(), tokens)))
}

fn normalize_book_address(raw: &str) -> Result<String> {
    Ok(Address::parse(raw)?.with_workchain().to_ascii_lowercase())
}

fn decode_book_fill_candidate(
    body_b64: &str,
    normalized_buyer_note: &str,
) -> Result<Option<BookFillCandidate>> {
    let Some((_, tokens)) = decode_book_event_input(body_b64, &["InferenceFilled"])? else {
        return Ok(None);
    };
    let buyer_note = normalize_book_address(&named_address(&tokens, "buyerNote")?)?;
    if buyer_note != normalized_buyer_note {
        return Ok(None);
    }
    Ok(Some(BookFillCandidate {
        maker_id: named_u128(&tokens, "makerId")?,
        taker_id: named_u128(&tokens, "takerId")?,
        ticks: named_u128(&tokens, "ticks")?,
        clearing_price: named_uint_decimal(&tokens, "clearingPrice")?,
        seller_token_contract: normalize_book_address(&named_address(&tokens, "sellerTC")?)?,
        buyer_note,
        seller_note: normalize_book_address(&named_address(&tokens, "sellerNote")?)?,
    }))
}

fn decode_book_event(body_b64: &str) -> Result<Option<BookEvent>> {
    let Some((event_name, tokens)) = decode_book_event_input(
        body_b64,
        &[
            "InferenceOrderPlaced",
            "InferenceOrderCancelled",
            "InferenceOrderCancelRejected",
            "InferenceFilled",
            "InferenceOrderExpired",
        ],
    )?
    else {
        return Ok(None);
    };
    match event_name.as_str() {
        "InferenceOrderPlaced" => Ok(Some(BookEvent::Placed(LiveBookOrder {
            order_id: named_u128(&tokens, "orderId")?,
            is_buy: named_bool(&tokens, "isBuy")?,
            price: named_u128(&tokens, "price")?,
            ticks_remaining: named_u128(&tokens, "ticks")?,
            note: named_address(&tokens, "note")?,
            token_contract: named_address(&tokens, "tokenContract")?,
            deadline: named_u64(&tokens, "deadline")?,
            flags: named_u8(&tokens, "flags")?,
            expired_by_event: false,
        }))),
        "InferenceOrderCancelled" => Ok(Some(BookEvent::Cancelled {
            order_id: named_u128(&tokens, "orderId")?,
        })),
        "InferenceOrderCancelRejected" => Ok(Some(BookEvent::CancelRejected {
            order_id: named_u128(&tokens, "orderId")?,
            reason: named_u8(&tokens, "reason")?,
            note: named_address(&tokens, "note")?,
        })),
        "InferenceFilled" => Ok(Some(BookEvent::Filled {
            maker_id: named_u128(&tokens, "makerId")?,
            taker_id: named_u128(&tokens, "takerId")?,
            ticks: named_u128(&tokens, "ticks")?,
        })),
        "InferenceOrderExpired" => Ok(Some(BookEvent::Expired {
            order_id: named_u128(&tokens, "orderId")?,
            is_buy: named_bool(&tokens, "isBuy")?,
            note: named_address(&tokens, "note")?,
            token_contract: named_address(&tokens, "tokenContract")?,
        })),
        _ => unreachable!(),
    }
}

/// Decode one book ext-out body as an owner-facing BUY outcome fact.

/// Every outcome the book can hand a buyer has its own event and every one of them names the owning note,
/// so a durable submit record is resolved by the outcome that happened -- never by an order's absence, which
/// says nothing about WHICH way it left the book. `InferenceOrderPlaced`/`InferenceOrderExpired` also carry
/// the side and are kept only for BUYs; `InferenceOrderCancelled`/`InferenceOrderRejected` do not, so the
/// caller correlates them through the order id it learned from the placement and its own submit window.
/// `created_at` is supplied by the caller from the message envelope.
pub(super) fn decode_buyer_order_fact(
    body_b64: &str,
    created_at: i64,
) -> Result<Option<BuyerOrderFact>> {
    let bytes = match base64::engine::general_purpose::STANDARD.decode(body_b64.trim()) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let cell = match tvm_types::read_single_root_boc(&bytes) {
        Ok(cell) => cell,
        Err(_) => return Ok(None),
    };
    let slice = match SliceData::load_cell(cell) {
        Ok(slice) => slice,
        Err(_) => return Ok(None),
    };
    let id = match Event::decode_id(slice.clone()) {
        Ok(id) => id,
        Err(_) => return Ok(None),
    };
    let contract = Contract::load(INFERENCE_ORDERBOOK_ABI.as_bytes())
        .map_err(|error| anyhow!("load InferenceOrderBook ABI: {error}"))?;
    let event = match contract.event_by_id(id) {
        Ok(event) => event,
        Err(_) => return Ok(None),
    };
    if !matches!(
        event.name.as_str(),
        "InferenceOrderPlaced"
            | "InferenceOrderCancelled"
            | "InferenceOrderExpired"
            | "InferenceRefunded"
            | "InferenceOrderRejected"
    ) {
        return Ok(None);
    }
    let tokens = event
        .decode_input(slice, true)
        .map_err(|error| anyhow!("decode {} body: {error}", event.name))?;
    let kind = match event.name.as_str() {
        "InferenceOrderPlaced" => {
            if !named_bool(&tokens, "isBuy")? {
                return Ok(None);
            }
            BuyerOrderFactKind::Placed {
                order_id: named_u128(&tokens, "orderId")?,
                price_per_tick: named_u128(&tokens, "price")?,
                ticks: named_u128(&tokens, "ticks")?,
                deadline: named_u64(&tokens, "deadline")?,
            }
        }
        "InferenceOrderCancelled" => BuyerOrderFactKind::Cancelled {
            order_id: named_u128(&tokens, "orderId")?,
            refunded: named_u128(&tokens, "refunded")?,
        },
        "InferenceOrderExpired" => {
            if !named_bool(&tokens, "isBuy")? {
                return Ok(None);
            }
            BuyerOrderFactKind::Expired {
                order_id: named_u128(&tokens, "orderId")?,
            }
        }
        // the money half of an expiry. `InferenceOrderExpired` carries no amount -- the
        // contract splits the two deliberately (`InferenceOrderBook.sol:387-393`) -- so without this
        // arm the client could see that a bid was swept and never that its escrow came back, which
        // is precisely the pair of facts an `expired` verdict needs. It carries no `isBuy`, so the
        // side is not filtered here: an ask holds no escrow and the book never refunds one, and the
        // caller correlates by the order id it already knows is its own bid.
        "InferenceRefunded" => BuyerOrderFactKind::Refunded {
            order_id: named_u128(&tokens, "orderId")?,
            amount: named_u128(&tokens, "amount")?,
        },
        "InferenceOrderRejected" => BuyerOrderFactKind::Rejected {
            reason: u8::try_from(named_u128(&tokens, "reason")?).unwrap_or(u8::MAX),
            refund: named_u128(&tokens, "refund")?,
        },
        _ => unreachable!(),
    };
    Ok(Some(BuyerOrderFact {
        created_at,
        note: named_address(&tokens, "note")?,
        kind,
    }))
}

fn named_u128(tokens: &[tvm_abi::Token], name: &str) -> Result<u128> {
    tokens
        .iter()
        .find_map(|token| match (&*token.name, &token.value) {
            (got, TokenValue::Uint(value)) if got == name => value.number.to_string().parse().ok(),
            _ => None,
        })
        .ok_or_else(|| anyhow!("event body missing or invalid {name}"))
}

fn named_uint_decimal(tokens: &[tvm_abi::Token], name: &str) -> Result<String> {
    tokens
        .iter()
        .find_map(|token| match (&*token.name, &token.value) {
            (got, TokenValue::Uint(value)) if got == name => Some(value.number.to_string()),
            _ => None,
        })
        .ok_or_else(|| anyhow!("event body missing or invalid {name}"))
}

fn named_u64(tokens: &[tvm_abi::Token], name: &str) -> Result<u64> {
    tokens
        .iter()
        .find_map(|token| match (&*token.name, &token.value) {
            (got, TokenValue::Uint(value)) if got == name => value.number.to_string().parse().ok(),
            _ => None,
        })
        .ok_or_else(|| anyhow!("event body missing or invalid {name}"))
}

/// Same shape as the other `named_*` readers, parsed at the width the ABI declares. `flags` is a
/// `uint8` there, so a value that does not fit means the event shape moved under us -- a decode
/// failure, not a silent zero, which is exactly the number a "no flags set" order legitimately has.
fn named_u8(tokens: &[tvm_abi::Token], name: &str) -> Result<u8> {
    tokens
        .iter()
        .find_map(|token| match (&*token.name, &token.value) {
            (got, TokenValue::Uint(value)) if got == name => value.number.to_string().parse().ok(),
            _ => None,
        })
        .ok_or_else(|| anyhow!("event body missing or invalid {name}"))
}

fn named_bool(tokens: &[tvm_abi::Token], name: &str) -> Result<bool> {
    tokens
        .iter()
        .find_map(|token| match (&*token.name, &token.value) {
            (got, TokenValue::Bool(value)) if got == name => Some(*value),
            _ => None,
        })
        .ok_or_else(|| anyhow!("event body missing or invalid {name}"))
}

fn named_address(tokens: &[tvm_abi::Token], name: &str) -> Result<String> {
    tokens
        .iter()
        .find_map(|token| match (&*token.name, &token.value) {
            (got, TokenValue::Address(value)) if got == name => Some(format!("{value}")),
            _ => None,
        })
        .ok_or_else(|| anyhow!("event body missing or invalid {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::Value;
    use std::collections::VecDeque;

    const TC_A: &str = "0:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TC_B: &str = "0:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const NOTE: &str = "0:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const FOREIGN_NOTE: &str = "0:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const ZERO_ADDRESS: &str = "0:0000000000000000000000000000000000000000000000000000000000000000";

    /// The incident, to the second: SELL 11's deadline, and the moment the buyer acted on it
    /// 779 seconds later while the fold-backed views still showed 956 ticks of liquidity.
    const PAST_DEADLINE: u64 = 1_785_678_525;
    const NOW: u64 = 1_785_679_304;
    const FUTURE_DEADLINE: u64 = 1_900_000_000;

    fn encode_event(name: &str, fields: Value) -> String {
        use tvm_abi::token::Tokenizer;
        use tvm_types::{BuilderData, IBitstring as _};

        let contract = Contract::load(INFERENCE_ORDERBOOK_ABI.as_bytes()).expect("load IOB ABI");
        let event = contract.event(name).expect("event by name");
        let tokens =
            Tokenizer::tokenize_all_params(&event.inputs, &fields).expect("tokenize event");
        let mut prefix = BuilderData::new();
        prefix.append_u32(event.get_id()).expect("event selector");
        let builder =
            TokenValue::pack_values_into_chain(&tokens, vec![prefix.into()], &event.abi_version)
                .expect("encode event body");
        let cell = builder.into_cell().expect("event cell");
        base64::engine::general_purpose::STANDARD
            .encode(tvm_types::write_boc(&cell).expect("event BOC"))
    }

    fn placed(id: u128, is_buy: bool, ticks: u128, token_contract: &str) -> String {
        placed_at(id, is_buy, ticks, token_contract, FUTURE_DEADLINE)
    }

    fn placed_at(
        id: u128,
        is_buy: bool,
        ticks: u128,
        token_contract: &str,
        deadline: u64,
    ) -> String {
        encode_event(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": id.to_string(),
                "isBuy": is_buy,
                "price": "700",
                "ticks": ticks.to_string(),
                "note": NOTE,
                "tokenContract": token_contract,
                "deadline": deadline.to_string(),
                "flags": 0
            }),
        )
    }

    fn placed_with_flags(id: u128, is_buy: bool, token_contract: &str, order_flags: u8) -> String {
        encode_event(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": id.to_string(),
                "isBuy": is_buy,
                "price": "700",
                "ticks": "10",
                "note": NOTE,
                "tokenContract": token_contract,
                "deadline": FUTURE_DEADLINE.to_string(),
                "flags": order_flags
            }),
        )
    }

    fn cancelled(id: u128) -> String {
        encode_event(
            "InferenceOrderCancelled",
            serde_json::json!({
                "orderId": id.to_string(),
                "refunded": "0",
                "note": NOTE
            }),
        )
    }

    fn filled(maker_id: u128, ticks: u128) -> String {
        filled_with_identities(maker_id, 99, ticks, NOTE, NOTE)
    }

    fn filled_with_identities(
        maker_id: u128,
        taker_id: u128,
        ticks: u128,
        buyer_note: &str,
        seller_note: &str,
    ) -> String {
        encode_event(
            "InferenceFilled",
            serde_json::json!({
                "makerId": maker_id.to_string(),
                "takerId": taker_id.to_string(),
                "ticks": ticks.to_string(),
                "clearingPrice": "700",
                "sellerTC": TC_A,
                "buyerNote": buyer_note,
                "sellerNote": seller_note
            }),
        )
    }

    fn filled_between(maker_id: u128, taker_id: u128, ticks: u128) -> String {
        filled_with_identities(maker_id, taker_id, ticks, NOTE, NOTE)
    }

    #[allow(dead_code)]
    fn __unused_tail(
        maker_id: u128,
        taker_id: u128,
        ticks: u128,
        buyer_note: &str,
        seller_note: &str,
    ) -> String {
        encode_event(
            "InferenceFilled",
            serde_json::json!({
                "makerId": maker_id.to_string(),
                "takerId": taker_id.to_string(),
                "ticks": ticks.to_string(),
                "clearingPrice": "700",
                "sellerTC": TC_A,
                "buyerNote": buyer_note,
                "sellerNote": seller_note
            }),
        )
    }

    fn expired(order_id: u128, is_buy: bool, note: &str, token_contract: &str) -> String {
        encode_event(
            "InferenceOrderExpired",
            serde_json::json!({
                "orderId": order_id.to_string(),
                "isBuy": is_buy,
                "note": note,
                "tokenContract": token_contract
            }),
        )
    }

    fn refunded(order_id: u128, note: &str, amount: u128) -> String {
        encode_event(
            "InferenceRefunded",
            serde_json::json!({
                "orderId": order_id.to_string(),
                "note": note,
                "amount": amount.to_string()
            }),
        )
    }

    fn message(sequence: u64, body: String) -> BookEventMessage {
        BookEventMessage {
            id: format!("message-{sequence}"),
            created_at: sequence,
            cursor: format!("cursor-{sequence:03}"),
            body,
        }
    }

    async fn fold(messages: Vec<BookEventMessage>) -> BookEventFold {
        let mut pages = VecDeque::from([BookEventPage {
            messages,
            previous_cursor: None,
        }]);
        fold_book_event_pages(BookEventFold::default(), move |_| {
            let page = pages.pop_front().expect("requested page");
            async move { Ok(page) }
        })
        .await
        .expect("fold events")
    }

    #[tokio::test]
    async fn event_fold_reports_live_sell_for_tc() {
        let folded = fold(vec![message(1, placed(7, false, 10, TC_A))]).await;
        assert_eq!(
            folded
                .live_sell_for_token_contract_at(TC_A, NOW)
                .map(|order| order.order_id),
            Some(7)
        );
    }

    /// the fold must keep the placement's own `flags`, not a zero.

    /// Driven through the real ABI encoder and the real decoder rather than by constructing a
    /// `LiveBookOrder`, because the defect being pinned is exactly that the decoder read every other
    /// declared field of `InferenceOrderPlaced` and dropped this one on the floor. A test that built
    /// the struct itself would hold whatever it was handed and could never see that.

    /// Both a set and a cleared value are folded in the same run: asserting only the non-zero case
    /// cannot tell a decoder that reads the field from one that hardcodes the value being asserted.
    #[tokio::test]
    async fn event_fold_keeps_the_placements_own_flags() {
        let shaped = crate::market::flags::AON | crate::market::flags::SUBSCRIPTION;
        let folded = fold(vec![
            message(1, placed_with_flags(7, false, TC_A, shaped)),
            message(2, placed_with_flags(8, false, TC_B, 0)),
        ])
        .await;

        let flags_by_id = |want: u128| {
            folded
                .all_orders()
                .find(|order| order.order_id == want)
                .map(|order| order.flags)
        };
        assert_eq!(
            flags_by_id(7),
            Some(shaped),
            "a shaped order must fold with the flags the book announced"
        );
        assert_eq!(
            flags_by_id(8),
            Some(0),
            "an unshaped order folds with a genuine zero, read from the same field"
        );
    }

    #[tokio::test]
    async fn event_fold_clears_on_cancel() {
        let folded = fold(vec![
            message(1, placed(7, false, 10, TC_A)),
            message(2, cancelled(7)),
        ])
        .await;
        assert!(folded.live_sell_for_token_contract_at(TC_A, NOW).is_none());
        // A cancel is terminal in a way an expiry is not: the row leaves the owner view too.
        assert_eq!(folded.all_orders().count(), 0);
    }

    #[tokio::test]
    async fn event_fold_clears_on_full_fill() {
        let folded = fold(vec![
            message(1, placed(7, false, 10, TC_A)),
            message(2, filled(7, 10)),
        ])
        .await;
        assert!(folded.live_sell_for_token_contract_at(TC_A, NOW).is_none());
    }

    #[test]
    fn book_fill_candidate_decoder_preserves_full_abi_and_filters_normalized_buyer() {
        let max_uint256 =
            "115792089237316195423570985008687907853269984665640564039457584007913129639935";
        let body = encode_event(
            "InferenceFilled",
            serde_json::json!({
                "makerId": "17",
                "takerId": "18",
                "ticks": "4",
                "clearingPrice": max_uint256,
                "sellerTC": TC_A,
                "buyerNote": NOTE,
                "sellerNote": FOREIGN_NOTE,
            }),
        );
        let requested = normalize_book_address(&NOTE.to_ascii_uppercase())
            .expect("normalize requested buyer note");
        let candidate = decode_book_fill_candidate(&body, &requested)
            .expect("decode full InferenceFilled")
            .expect("matching buyer note is retained");

        assert_eq!(
            candidate,
            BookFillCandidate {
                maker_id: 17,
                taker_id: 18,
                ticks: 4,
                clearing_price: max_uint256.to_string(),
                seller_token_contract: TC_A.to_string(),
                buyer_note: NOTE.to_string(),
                seller_note: FOREIGN_NOTE.to_string(),
            }
        );

        let foreign = normalize_book_address(FOREIGN_NOTE).expect("normalize foreign buyer note");
        assert!(
            decode_book_fill_candidate(&body, &foreign)
                .expect("decode non-matching fill")
                .is_none(),
            "the history walk must not retain a fill for another buyer note"
        );
        assert!(
            decode_book_fill_candidate(&placed(19, true, 1, TC_B), &requested)
                .expect("skip a non-fill event")
                .is_none(),
            "the identity read retains candidates, not unrelated book history"
        );
    }

    /// Both maker/taker ids and both owner notes are authenticated correlation facts. The valid
    /// resting-BUY/arriving-SELL event removes the owned maker; the same ids attributed to a
    /// foreign buyer note are the adversary and must leave every row untouched.

    /// E2E-MATCH-12, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-MATCH-12/L0
    #[ignore = "EXPECTED TO FAIL until BookEvent::Filled retains and checks maker, taker, buyer-note, and seller-note identities"]
    #[tokio::test]
    async fn match_12_fill_requires_maker_taker_and_note_identity() {
        let valid = fold(vec![
            message(1, placed(17, true, 4, TC_A)),
            message(2, placed(99, false, 9, TC_B)),
            message(3, filled_with_identities(17, 18, 4, NOTE, FOREIGN_NOTE)),
        ])
        .await;
        let valid_removed_exact_maker =
            !valid.live_orders_at(NOW).any(|order| order.order_id == 17)
                && valid.live_orders_at(NOW).any(|order| order.order_id == 99);

        let foreign = fold(vec![
            message(1, placed(17, true, 4, TC_A)),
            message(2, placed(99, false, 9, TC_B)),
            message(3, filled_with_identities(17, 18, 4, FOREIGN_NOTE, NOTE)),
        ])
        .await;
        let foreign_untouched = foreign
            .live_orders_at(NOW)
            .any(|order| order.order_id == 17)
            && foreign
                .live_orders_at(NOW)
                .any(|order| order.order_id == 99);

        assert!(
            valid_removed_exact_maker && foreign_untouched,
            "E2E-MATCH-12 missing capability: fill mutated without maker/taker/note identity agreement"
        );
    }

    /// The decoder must preserve both order identities in both arriving-side directions. Swapping
    /// the maker/taker ids or either participant note is the adversary; a lossy maker-only record
    /// cannot distinguish those events.

    /// E2E-MATCH-13, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-MATCH-13/L0
    #[ignore = "EXPECTED TO FAIL until the client fill record preserves taker id and both participant notes"]
    #[test]
    fn match_13_fill_preserves_both_order_identities_in_both_directions() {
        let arriving_sell =
            decode_book_event(&filled_with_identities(27, 28, 2, NOTE, FOREIGN_NOTE))
                .expect("decode arriving SELL")
                .map(|event| format!("{event:?}"))
                .unwrap_or_default();
        let arriving_buy =
            decode_book_event(&filled_with_identities(37, 38, 2, NOTE, FOREIGN_NOTE))
                .expect("decode arriving BUY")
                .map(|event| format!("{event:?}"))
                .unwrap_or_default();

        let preserves = [
            (&arriving_sell, 27_u128, 28_u128),
            (&arriving_buy, 37_u128, 38_u128),
        ]
        .into_iter()
        .all(|(decoded, maker, taker)| {
            decoded.contains(&format!("maker_id: {maker}"))
                && decoded.contains(&format!("taker_id: {taker}"))
                && decoded.contains(NOTE)
                && decoded.contains(FOREIGN_NOTE)
        });
        assert!(
            preserves,
            "E2E-MATCH-13 missing capability: decoded fill discarded one or more order/note identities"
        );
    }

    /// A local maker row and a local arriving/taker row are removed by their respective identity
    /// in separate runs. Duplicate, swapped, and foreign-note fills may not remove the bystander.

    /// E2E-ORD-06, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ORD-06/L0
    #[ignore = "EXPECTED TO FAIL until the order projection handles the local taker id as well as maker id"]
    #[tokio::test]
    async fn ord_06_fill_removes_exact_maker_or_taker_side_once() {
        let maker = fold(vec![
            message(1, placed(47, false, 3, TC_A)),
            message(2, placed(49, false, 5, TC_B)),
            message(3, filled_with_identities(47, 48, 3, NOTE, NOTE)),
            message(4, filled_with_identities(47, 48, 3, NOTE, NOTE)),
        ])
        .await;
        let maker_exact = !maker.live_orders_at(NOW).any(|order| order.order_id == 47)
            && maker.live_orders_at(NOW).any(|order| order.order_id == 49);

        let taker = fold(vec![
            message(1, placed(58, true, 3, TC_A)),
            message(2, placed(59, false, 5, TC_B)),
            message(3, filled_with_identities(57, 58, 3, NOTE, FOREIGN_NOTE)),
        ])
        .await;
        let taker_exact = !taker.live_orders_at(NOW).any(|order| order.order_id == 58)
            && taker.live_orders_at(NOW).any(|order| order.order_id == 59);

        let swapped_foreign = fold(vec![
            message(1, placed(69, false, 5, TC_B)),
            message(2, filled_with_identities(69, 68, 5, FOREIGN_NOTE, NOTE)),
        ])
        .await;
        let foreign_preserved = swapped_foreign
            .live_orders_at(NOW)
            .any(|order| order.order_id == 69);

        assert!(
            maker_exact && taker_exact && foreign_preserved,
            "E2E-ORD-06 missing capability: fill projection did not isolate exact maker/taker identity"
        );
    }

    /// Crossing the deadline alone keeps the raw row present. Only its matching expiry event may
    /// remove it; an unrelated event is the adversary and leaves the expired row visible.

    /// E2E-ORD-08, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ORD-08/L0
    #[tokio::test]
    async fn ord_08_expired_row_stays_visible_until_its_matching_expiry_event() {
        let before_event = fold(vec![message(1, placed(77, false, 2, TC_A))]).await;
        let visible_before = before_event
            .live_orders_at(NOW)
            .any(|order| order.order_id == 77 && order.deadline < 2_000_000_000);
        let unrelated = fold(vec![
            message(1, placed(77, false, 2, TC_A)),
            message(2, expired(78, false, NOTE, TC_B)),
        ])
        .await;
        let unrelated_preserved = unrelated
            .live_orders_at(NOW)
            .any(|order| order.order_id == 77);
        let removed = fold(vec![
            message(1, placed(77, false, 2, TC_A)),
            message(2, expired(77, false, NOTE, TC_A)),
        ])
        .await;
        let matching_removed = !removed
            .live_orders_at(NOW)
            .any(|order| order.order_id == 77);

        assert!(
            visible_before && unrelated_preserved && matching_removed,
            "E2E-ORD-08 missing capability: matching expiry event did not remove the visible expired row"
        );
    }

    /// Wall-clock age never deletes a projected row. A matching authoritative expiry removes it
    /// once; replay and a foreign order id cannot remove the unrelated bystander.

    /// E2E-ORD-10, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-ORD-10/L0
    #[tokio::test]
    async fn ord_10_matching_expiry_event_is_the_only_removal_authority() {
        let folded = fold(vec![
            message(1, placed(87, false, 2, TC_A)),
            message(2, placed(88, false, 3, TC_B)),
            message(3, expired(87, false, NOTE, TC_A)),
            message(4, expired(87, false, NOTE, TC_A)),
            message(5, expired(99, false, FOREIGN_NOTE, TC_B)),
        ])
        .await;
        let exact_once = !folded.live_orders_at(NOW).any(|order| order.order_id == 87)
            && folded.live_orders_at(NOW).any(|order| order.order_id == 88)
            && folded.live_orders_at(NOW).count() == 1;
        assert!(
            exact_once,
            "E2E-ORD-10 missing capability: authoritative expiry was not one exact idempotent removal"
        );
    }

    /// A positive residual expiry carries one exact refund fact and one exact removal; a zero
    /// residual carries only removal. Removing either positive fact or inventing a zero refund is
    /// the adversary.

    /// E2E-CXL-14, `tests/e2e/test-specification.md`.
    /// E2E-ROW: E2E-CXL-14/L0
    #[ignore = "EXPECTED TO FAIL until the client fold decodes and correlates InferenceRefunded plus InferenceOrderExpired"]
    #[tokio::test]
    async fn cxl_14_expired_buy_refund_fold_is_exact_and_idempotent() {
        let positive_amount = 4_100_u128;
        let refund = decode_book_event(&refunded(97, NOTE, positive_amount))
            .expect("decode positive refund")
            .map(|event| format!("{event:?}"))
            .unwrap_or_default();
        let expiry = decode_book_event(&expired(97, true, NOTE, TC_A))
            .expect("decode positive expiry")
            .map(|event| format!("{event:?}"))
            .unwrap_or_default();
        let positive_facts = refund.contains("order_id: 97")
            && refund.contains(&format!("amount: {positive_amount}"))
            && expiry.contains("order_id: 97");

        let positive = fold(vec![
            message(1, placed(97, true, 4, TC_A)),
            message(2, refunded(97, NOTE, positive_amount)),
            message(3, expired(97, true, NOTE, TC_A)),
            message(4, refunded(97, NOTE, positive_amount)),
            message(5, expired(97, true, NOTE, TC_A)),
            message(6, placed(98, false, 1, TC_B)),
        ])
        .await;
        let exact_removal = !positive
            .live_orders_at(NOW)
            .any(|order| order.order_id == 97)
            && positive
                .live_orders_at(NOW)
                .any(|order| order.order_id == 98);

        let zero_expiry = decode_book_event(&expired(107, true, NOTE, TC_A))
            .expect("decode zero-refund expiry")
            .map(|event| format!("{event:?}"))
            .unwrap_or_default();
        let zero_has_no_invented_refund =
            zero_expiry.contains("order_id: 107") && !zero_expiry.contains("amount:");

        assert!(
            positive_facts && exact_removal && zero_has_no_invented_refund,
            "E2E-CXL-14 missing capability: expiry/refund fold lost or invented exact refund money"
        );
    }

    /// by-fact: a SELL for 10 ticks had 2 taken by a buyer, and `orders list` went on showing
    /// it as resting with 8 while the indexer had no asks at all, `market` said "no resting asks
    /// yet" and the deal contract was already destroyed.

    /// The rule under test is the CONTRACT's, not the remainder arithmetic's: a maker SELL is a
    /// one-deal slot and `InferenceOrderBook.sol:1087-1090` removes it WHOLE on any match with a
    /// taker BUY -- `if (takerIsBuy) { _removeFromBook(cur); }`, with no reference to `trade`. So
    /// after a partial fill the ask must be gone from every view, not reduced: a fill is its only
    /// exit from this fold, and a positive remainder here can never be taken out again.
    #[tokio::test]
    async fn event_fold_removes_a_partially_filled_sell_whole() {
        let folded = fold(vec![
            message(1, placed(38, false, 10, TC_A)),
            message(2, filled(38, 2)),
        ])
        .await;

        assert!(folded.live_sell_for_token_contract_at(TC_A, NOW).is_none());
        assert_eq!(folded.live_orders_at(NOW).count(), 0);
        // ...and it is gone from the OWNER-facing set too. A partially-filled ask holds no escrow
        // and no longer exists on chain, so unlike an expired row there is nothing left for
        // its owner to act on: showing it invites a cancel the book must refuse.
        assert_eq!(folded.all_orders().count(), 0);
    }

    /// The other half of the same contract branch: a maker BUY spans deals, so a taker SELL that
    /// takes part of it leaves the rest resting -- `InferenceOrderBook.sol:1091-1097` refunds and
    /// removes only when `mk.amount == trade`, and otherwise writes `amount = mk.amount - trade`.
    #[tokio::test]
    async fn event_fold_reduces_a_partially_filled_maker_buy() {
        let folded = fold(vec![
            message(1, placed(20, true, 10, TC_A)),
            message(2, filled_between(20, 21, 4)),
        ])
        .await;

        assert_eq!(
            folded
                .all_orders()
                .map(|order| (order.order_id, order.ticks_remaining))
                .collect::<Vec<_>>(),
            vec![(20, 6)]
        );
    }

    #[tokio::test]
    async fn event_fold_ignores_buy_orders() {
        let folded = fold(vec![message(1, placed(7, true, 10, TC_A))]).await;
        assert!(folded.live_sell_for_token_contract_at(TC_A, NOW).is_none());
        assert_eq!(folded.live_orders_at(NOW).count(), 1);
    }

    /// by-fact: order 10 filled as the TAKER, the indexer had it `FILLED` with `ticks: 0` and
    /// the book had no bid -- yet `orders list` kept printing it as resting. `InferenceFilled` names
    /// both sides; folding only `makerId` left the taker with no exit from the view at all.
    #[tokio::test]
    async fn event_fold_clears_an_order_that_filled_as_the_taker() {
        let folded = fold(vec![
            message(1, placed(10, true, 19, TC_A)),
            message(2, filled_between(99, 10, 19)),
        ])
        .await;
        assert_eq!(folded.all_orders().count(), 0);
        assert_eq!(folded.live_orders_at(NOW).count(), 0);
    }

    #[tokio::test]
    async fn event_fold_reduces_a_taker_side_partial_fill() {
        let folded = fold(vec![
            message(1, placed(10, true, 19, TC_A)),
            message(2, filled_between(99, 10, 4)),
        ])
        .await;
        assert_eq!(
            folded
                .all_orders()
                .map(|order| (order.order_id, order.ticks_remaining))
                .collect::<Vec<_>>(),
            vec![(10, 15)]
        );
    }

    /// by-fact: SELL 11 for 956 ticks was past its deadline and nobody had called
    /// `expireOrder`, so no expiry event existed to consume -- and `market` still offered it. Expiry
    /// is lazy (E2E-CXL-01), so the deadline predicate is what has to hold here.
    #[tokio::test]
    async fn event_fold_hides_an_expired_ask_before_any_expiry_event() {
        let folded = fold(vec![message(
            1,
            placed_at(11, false, 956, TC_A, PAST_DEADLINE),
        )])
        .await;

        assert_eq!(folded.live_orders_at(NOW).count(), 0);
        assert!(folded.live_sell_for_token_contract_at(TC_A, NOW).is_none());
        // ...and it is NOT dropped:'s explicit non-goal keeps it in the owner-facing set,
        // because an expired order may still be sitting in the book holding escrow.
        let retained = folded.all_orders().collect::<Vec<_>>();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].order_id, 11);
        assert!(!retained[0].expired_by_event);
    }

    /// The book's `_isExpired` is `>=`, so the deadline second itself is already expired.
    #[tokio::test]
    async fn event_fold_deadline_boundary_matches_the_contract() {
        let folded = fold(vec![message(
            1,
            placed_at(11, false, 956, TC_A, PAST_DEADLINE),
        )])
        .await;

        assert_eq!(folded.live_orders_at(PAST_DEADLINE - 1).count(), 1);
        assert_eq!(folded.live_orders_at(PAST_DEADLINE).count(), 0);
    }

    /// the fold consumes the authoritative expiry event. The deadline here is in the FUTURE,
    /// so only the event can explain the removal.
    #[tokio::test]
    async fn event_fold_consumes_the_expiry_event() {
        let folded = fold(vec![
            message(1, placed(11, false, 956, TC_A)),
            message(2, expired(11, false, NOTE, TC_A)),
        ])
        .await;

        assert_eq!(folded.live_orders_at(NOW).count(), 0);
        assert!(folded.live_sell_for_token_contract_at(TC_A, NOW).is_none());
        let retained = folded.all_orders().collect::<Vec<_>>();
        assert_eq!(retained.len(), 1);
        // Removal by event is distinguishable from a row merely hidden by the deadline predicate:
        // the book has dropped this one for good and (for a bid) already refunded its escrow.
        assert!(retained[0].expired_by_event);
    }

    /// A bid expiry carries `tokenContract == address(0)` and `isBuy == true`; the fold must accept
    /// that shape and remove the exact order id.
    #[tokio::test]
    async fn event_fold_consumes_a_bid_expiry_event() {
        let folded = fold(vec![
            message(1, placed(12, true, 2, TC_A)),
            message(2, expired(12, true, NOTE, ZERO_ADDRESS)),
        ])
        .await;

        assert_eq!(folded.live_orders_at(NOW).count(), 0);
        assert!(folded.all_orders().all(|order| order.expired_by_event));
    }

    /// Pages overlap by design, and `expireOrder` is permissionless. Replaying the same expiry --
    /// or an expiry for an order this fold never held -- must land on the same state exactly once.
    #[tokio::test]
    async fn event_fold_expiry_is_idempotent_across_overlapping_replay() {
        let once = fold(vec![
            message(1, placed(11, false, 956, TC_A)),
            message(2, expired(11, false, NOTE, TC_A)),
        ])
        .await;
        let replayed = fold(vec![
            message(1, placed(11, false, 956, TC_A)),
            message(2, expired(11, false, NOTE, TC_A)),
            message(3, expired(11, false, NOTE, TC_A)),
            message(4, expired(404, false, NOTE, TC_B)),
        ])
        .await;

        assert_eq!(once.all_orders().count(), 1);
        assert_eq!(
            replayed.all_orders().collect::<Vec<_>>(),
            once.all_orders().collect::<Vec<_>>()
        );
        assert_eq!(replayed.live_orders_at(NOW).count(), 0);
    }

    /// The decoder's existing policy: a body that is not a decodable book event is skipped, but a
    /// body that IS one and cannot be read fails the whole fold rather than silently losing state.
    /// E2E-ROW: E2E-ORD-24/L0
    #[tokio::test]
    async fn event_fold_fails_closed_on_a_malformed_expiry_body() {
        use tvm_types::{BuilderData, IBitstring as _};

        let contract = Contract::load(INFERENCE_ORDERBOOK_ABI.as_bytes()).expect("load IOB ABI");
        let event = contract.event("InferenceOrderExpired").expect("event");
        let mut header_only = BuilderData::new();
        header_only
            .append_u32(event.get_id())
            .expect("event selector");
        let cell = header_only.into_cell().expect("event cell");
        let body = base64::engine::general_purpose::STANDARD
            .encode(tvm_types::write_boc(&cell).expect("event BOC"));

        let error = fold_book_event_pages(BookEventFold::default(), move |_| {
            let body = body.clone();
            async move {
                Ok(BookEventPage {
                    messages: vec![message(1, body)],
                    previous_cursor: None,
                })
            }
        })
        .await
        .expect_err("a truncated expiry body must fail the fold");
        assert!(
            format!("{error:#}").contains("InferenceOrderExpired"),
            "{error:#}"
        );
    }

    /// A SELL commits no collateral, so `PrivateNote` refuses `ttl == 0`: a zero-deadline ask is
    /// malformed, never immortal liquidity. A zero-deadline BUY is the contract's GTC bid.
    #[tokio::test]
    async fn event_fold_zero_deadline_is_malformed_for_a_sell_and_gtc_for_a_buy() {
        let sell = fold(vec![message(1, placed_at(11, false, 956, TC_A, 0))]).await;
        assert_eq!(sell.live_orders_at(NOW).count(), 0);
        assert!(sell.live_sell_for_token_contract_at(TC_A, NOW).is_none());
        assert_eq!(sell.all_orders().count(), 1);

        let buy = fold(vec![message(1, placed_at(12, true, 2, TC_A, 0))]).await;
        assert_eq!(buy.live_orders_at(NOW).count(), 1);
    }

    #[test]
    fn inference_order_book_event_abi_shape_is_pinned() {
        let abi: Value = serde_json::from_str(INFERENCE_ORDERBOOK_ABI).expect("parse IOB ABI");
        let events = abi["events"].as_array().expect("events[]");
        let shape = |name: &str| {
            events
                .iter()
                .find(|event| event["name"] == name)
                .expect("event present")
                .get("inputs")
                .and_then(Value::as_array)
                .expect("inputs[]")
                .iter()
                .map(|input| {
                    (
                        input["name"].as_str().unwrap_or("").to_string(),
                        input["type"].as_str().unwrap_or("").to_string(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            shape("InferenceOrderPlaced"),
            vec![
                ("orderId".into(), "uint128".into()),
                ("isBuy".into(), "bool".into()),
                ("price".into(), "uint256".into()),
                ("ticks".into(), "uint128".into()),
                ("note".into(), "address".into()),
                ("tokenContract".into(), "address".into()),
                ("deadline".into(), "uint64".into()),
                ("flags".into(), "uint8".into()),
            ]
        );
        assert_eq!(
            shape("InferenceOrderCancelled"),
            vec![
                ("orderId".into(), "uint128".into()),
                ("refunded".into(), "uint128".into()),
                ("note".into(), "address".into()),
            ]
        );
        assert_eq!(
            shape("InferenceFilled"),
            vec![
                ("makerId".into(), "uint128".into()),
                ("takerId".into(), "uint128".into()),
                ("ticks".into(), "uint128".into()),
                ("clearingPrice".into(), "uint256".into()),
                ("sellerTC".into(), "address".into()),
                ("buyerNote".into(), "address".into()),
                ("sellerNote".into(), "address".into()),
            ]
        );
        assert_eq!(
            shape("InferenceOrderExpired"),
            vec![
                ("orderId".into(), "uint128".into()),
                ("isBuy".into(), "bool".into()),
                ("note".into(), "address".into()),
                ("tokenContract".into(), "address".into()),
            ]
        );
        assert_eq!(
            shape("InferenceOrderRejected"),
            vec![
                ("reason".into(), "uint8".into()),
                ("note".into(), "address".into()),
                ("tokenContract".into(), "address".into()),
                ("refund".into(), "uint128".into()),
            ]
        );
    }

    /// every outcome the book can hand a buyer decodes to an owner-named fact, encoded here with the
    /// head's own compiled ABI so a decoder that drifts from the deployed event shape fails offline instead
    /// of silently reporting "no outcome" against a live book.
    #[test]
    fn buyer_order_facts_decode_every_owner_facing_outcome() {
        let placed = decode_buyer_order_fact(&placed(12, true, 4, TC_A), 1_700)
            .expect("decode placement")
            .expect("a buy placement is an owner-facing fact");
        assert_eq!(placed.created_at, 1_700);
        assert_eq!(placed.note, NOTE);
        assert_eq!(
            placed.kind,
            BuyerOrderFactKind::Placed {
                order_id: 12,
                price_per_tick: 700,
                ticks: 4,
                deadline: 1_900_000_000,
            }
        );
        assert_eq!(placed.order_id(), Some(12));

        let cancelled = decode_buyer_order_fact(&cancelled(12), 1_800)
            .expect("decode cancellation")
            .expect("a cancellation is an owner-facing fact");
        assert_eq!(cancelled.note, NOTE);
        assert_eq!(
            cancelled.kind,
            BuyerOrderFactKind::Cancelled {
                order_id: 12,
                refunded: 0,
            }
        );

        let expired = decode_buyer_order_fact(
            &encode_event(
                "InferenceOrderExpired",
                serde_json::json!({
                    "orderId": "12",
                    "isBuy": true,
                    "note": NOTE,
                    "tokenContract": TC_A,
                }),
            ),
            1_900,
        )
        .expect("decode expiry")
        .expect("an expiry sweep is an owner-facing fact");
        assert_eq!(expired.kind, BuyerOrderFactKind::Expired { order_id: 12 });

        let rejected = decode_buyer_order_fact(
            &encode_event(
                "InferenceOrderRejected",
                serde_json::json!({
                    "reason": 7,
                    "note": NOTE,
                    "tokenContract": TC_A,
                    "refund": "10250000000",
                }),
            ),
            2_000,
        )
        .expect("decode rejection")
        .expect("a rejection is an owner-facing fact");
        assert_eq!(
            rejected.kind,
            BuyerOrderFactKind::Rejected {
                reason: 7,
                refund: 10_250_000_000,
            },
            "a rejected submit never received an order id, so the refund is what identifies it"
        );
        assert_eq!(rejected.order_id(), None);
    }

    /// The seller side shares these events. A buyer's record must never be resolved by an ask's placement
    /// or by an ask being expired out of the book.
    #[test]
    fn buyer_order_facts_ignore_the_seller_side_and_unrelated_events() {
        assert!(
            decode_buyer_order_fact(&placed(7, false, 10, TC_A), 1)
                .expect("decode")
                .is_none(),
            "an ask placement is not a buyer outcome"
        );
        assert!(
            decode_buyer_order_fact(
                &encode_event(
                    "InferenceOrderExpired",
                    serde_json::json!({
                        "orderId": "7",
                        "isBuy": false,
                        "note": NOTE,
                        "tokenContract": TC_A,
                    }),
                ),
                1,
            )
            .expect("decode")
            .is_none(),
            "an expired ask is not a buyer outcome"
        );
        assert!(
            decode_buyer_order_fact(&filled(7, 10), 1)
                .expect("decode")
                .is_none(),
            "fills keep their own owner-facing note event as the buyer's proof"
        );
    }

    #[tokio::test]
    async fn event_fold_pages_all_previous_pages() {
        let mut pages = VecDeque::from([
            BookEventPage {
                messages: vec![message(2, placed(8, false, 5, TC_B))],
                previous_cursor: Some("older".into()),
            },
            BookEventPage {
                messages: vec![message(1, placed(7, false, 10, TC_A))],
                previous_cursor: None,
            },
        ]);
        let mut requested = Vec::new();
        let folded = fold_book_event_pages(BookEventFold::default(), |before| {
            requested.push(before);
            let page = pages.pop_front().expect("requested page");
            async move { Ok(page) }
        })
        .await
        .expect("fold pages");
        assert_eq!(requested, vec![None, Some("older".into())]);
        assert_eq!(folded.live_orders_at(NOW).count(), 2);
        assert!(folded.live_sell_for_token_contract_at(TC_A, NOW).is_some());
        assert!(folded.live_sell_for_token_contract_at(TC_B, NOW).is_some());
    }

    #[tokio::test]
    async fn event_fold_resumes_after_last_seen_id() {
        let first = fold(vec![message(1, placed(7, false, 10, TC_A))]).await;
        assert_eq!(first.last_seen_id(), Some("message-1"));
        let second = fold_book_event_pages(first, |_| async {
            Ok(BookEventPage {
                messages: vec![
                    message(1, placed(7, false, 10, TC_A)),
                    message(2, placed(8, false, 5, TC_B)),
                ],
                previous_cursor: Some("not-requested".into()),
            })
        })
        .await
        .expect("resume fold");
        assert_eq!(second.last_seen_id(), Some("message-2"));
        assert_eq!(second.live_orders_at(NOW).count(), 2);
        assert!(second.live_sell_for_token_contract_at(TC_A, NOW).is_some());
        assert!(second.live_sell_for_token_contract_at(TC_B, NOW).is_some());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// 's invariant, over an arbitrary event stream folded for real: whatever the book did,
        /// every SELL this fold reports as live has ticks left, a TokenContract, a finite deadline
        /// the observation time has not reached, and no expiry event against it. If any reachable
        /// stream can break one of those, `market` can offer liquidity no buyer can take.
        #[test]
        fn every_live_sell_is_positive_dated_and_unexpired(
            deadlines in proptest::collection::vec(0u64..=(NOW + 600), 1..6),
            ticks in proptest::collection::vec(0u128..40, 1..6),
            fills in proptest::collection::vec(0u128..40, 0..6),
            expiries in proptest::collection::vec(0usize..6, 0..6),
            cancels in proptest::collection::vec(0usize..6, 0..4),
        ) {
            let mut messages = Vec::new();
            let mut sequence = 0u64;
            let mut next = |body: String, sequence: &mut u64| {
                *sequence += 1;
                messages.push(message(*sequence, body));
            };
            for (index, deadline) in deadlines.iter().enumerate() {
                let id = index as u128;
                let is_buy = index % 3 == 0;
                let tc = if index % 2 == 0 { TC_A } else { TC_B };
                next(
                    placed_at(id, is_buy, ticks[index % ticks.len()], tc, *deadline),
                    &mut sequence,
                );
            }
            for (index, amount) in fills.iter().enumerate() {
                next(
                    filled_between(index as u128, (index + 1) as u128, *amount),
                    &mut sequence,
                );
            }
            for index in &expiries {
                next(expired(*index as u128, index % 3 == 0, NOTE, TC_A), &mut sequence);
            }
            for index in &cancels {
                next(cancelled(*index as u128), &mut sequence);
            }

            let folded = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("test runtime")
                .block_on(fold(messages));

            for order in folded.live_orders_at(NOW).filter(|order| !order.is_buy) {
                prop_assert!(order.ticks_remaining > 0, "{:?}", order);
                prop_assert!(!order.token_contract.is_empty(), "{:?}", order);
                prop_assert_ne!(order.deadline, 0, "{:?}", order);
                prop_assert!(NOW < order.deadline, "{:?}", order);
                prop_assert!(!order.expired_by_event, "{:?}", order);
            }
        }
    }
}
