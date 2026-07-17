//! Authoritative live-order state folded from `InferenceOrderBook` ext-out events.

use anyhow::{anyhow, Result};
use base64::Engine as _;
use gosh_ackinacki::wallet::query::fetch_dapp_id;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::time::Duration;
use tvm_abi::token::TokenValue;
use tvm_abi::{Contract, Event};
use tvm_types::SliceData;

use super::client::{fetch_ext_out_page, ExtOutPage};
use super::contracts_provision::INFERENCE_ORDERBOOK_ABI;

const PAGE_SIZE: u32 = 50;
const READ_BACKOFF: [Duration; 4] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

/// One still-live order reconstructed from the book event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveBookOrder {
    pub order_id: u128,
    pub is_buy: bool,
    pub price: u128,
    pub ticks_remaining: u128,
    pub note: String,
    pub token_contract: String,
    pub deadline: u64,
}

/// Incremental fold state. Pass a previous value back to avoid replaying known history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookEventFold {
    orders: BTreeMap<u128, LiveBookOrder>,
    last_seen_id: Option<String>,
}

impl BookEventFold {
    pub fn live_orders(&self) -> impl Iterator<Item = &LiveBookOrder> {
        self.orders.values()
    }

    pub fn live_sell_for_token_contract(&self, token_contract: &str) -> Option<&LiveBookOrder> {
        self.orders
            .values()
            .find(|order| !order.is_buy && order.token_contract == token_contract)
    }

    pub fn last_seen_id(&self) -> Option<&str> {
        self.last_seen_id.as_deref()
    }

    fn apply(&mut self, event: BookEvent) {
        match event {
            BookEvent::Placed(order) => {
                self.orders.insert(order.order_id, order);
            }
            BookEvent::Cancelled { order_id } => {
                self.orders.remove(&order_id);
            }
            BookEvent::Filled { maker_id, ticks } => {
                let remove = self.orders.get_mut(&maker_id).is_some_and(|order| {
                    order.ticks_remaining = order.ticks_remaining.saturating_sub(ticks);
                    order.ticks_remaining == 0
                });
                if remove {
                    self.orders.remove(&maker_id);
                }
            }
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

enum BookEvent {
    Placed(LiveBookOrder),
    Cancelled { order_id: u128 },
    Filled { maker_id: u128, ticks: u128 },
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
    http: &reqwest::Client,
    endpoint: &str,
    account_id: &str,
    fold: BookEventFold,
) -> Result<BookEventFold> {
    let dapp_id = fetch_dapp_id(http, endpoint, account_id).await?;
    fold_book_event_pages(fold, |before| {
        let dapp_id = dapp_id.clone();
        async move {
            for delay in READ_BACKOFF {
                match fetch_ext_out_page(
                    http,
                    endpoint,
                    account_id,
                    &dapp_id,
                    PAGE_SIZE,
                    before.as_deref(),
                )
                .await
                {
                    Ok(page) => return Ok(book_event_page(page)),
                    Err(error) if is_transient_read(&error) => {
                        tokio::time::sleep(delay).await;
                    }
                    Err(error) => return Err(error),
                }
            }
            fetch_ext_out_page(
                http,
                endpoint,
                account_id,
                &dapp_id,
                PAGE_SIZE,
                before.as_deref(),
            )
            .await
            .map(book_event_page)
        }
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

fn is_transient_read(error: &anyhow::Error) -> bool {
    error.downcast_ref::<reqwest::Error>().is_some_and(|error| {
        error.is_connect()
            || error.is_timeout()
            || error.is_body()
            || error
                .status()
                .is_some_and(|status| status.is_server_error() || status.as_u16() == 429)
    })
}

fn decode_book_event(body_b64: &str) -> Result<Option<BookEvent>> {
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
        "InferenceOrderPlaced" | "InferenceOrderCancelled" | "InferenceFilled"
    ) {
        return Ok(None);
    }
    let tokens = event
        .decode_input(slice, true)
        .map_err(|error| anyhow!("decode {} body: {error}", event.name))?;
    match event.name.as_str() {
        "InferenceOrderPlaced" => Ok(Some(BookEvent::Placed(LiveBookOrder {
            order_id: named_u128(&tokens, "orderId")?,
            is_buy: named_bool(&tokens, "isBuy")?,
            price: named_u128(&tokens, "price")?,
            ticks_remaining: named_u128(&tokens, "ticks")?,
            note: named_address(&tokens, "note")?,
            token_contract: named_address(&tokens, "tokenContract")?,
            deadline: named_u64(&tokens, "deadline")?,
        }))),
        "InferenceOrderCancelled" => Ok(Some(BookEvent::Cancelled {
            order_id: named_u128(&tokens, "orderId")?,
        })),
        "InferenceFilled" => Ok(Some(BookEvent::Filled {
            maker_id: named_u128(&tokens, "makerId")?,
            ticks: named_u128(&tokens, "ticks")?,
        })),
        _ => unreachable!(),
    }
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

fn named_u64(tokens: &[tvm_abi::Token], name: &str) -> Result<u64> {
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
    use serde_json::Value;
    use std::collections::VecDeque;

    const TC_A: &str = "0:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const TC_B: &str = "0:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const NOTE: &str = "0:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

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
        encode_event(
            "InferenceOrderPlaced",
            serde_json::json!({
                "orderId": id.to_string(),
                "isBuy": is_buy,
                "price": "700",
                "ticks": ticks.to_string(),
                "note": NOTE,
                "tokenContract": token_contract,
                "deadline": "1900000000"
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
        encode_event(
            "InferenceFilled",
            serde_json::json!({
                "makerId": maker_id.to_string(),
                "takerId": "99",
                "ticks": ticks.to_string(),
                "clearingPrice": "700",
                "sellerTC": TC_A,
                "buyerNote": NOTE,
                "sellerNote": NOTE
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
                .live_sell_for_token_contract(TC_A)
                .map(|order| order.order_id),
            Some(7)
        );
    }

    #[tokio::test]
    async fn event_fold_clears_on_cancel() {
        let folded = fold(vec![
            message(1, placed(7, false, 10, TC_A)),
            message(2, cancelled(7)),
        ])
        .await;
        assert!(folded.live_sell_for_token_contract(TC_A).is_none());
    }

    #[tokio::test]
    async fn event_fold_clears_on_full_fill() {
        let folded = fold(vec![
            message(1, placed(7, false, 10, TC_A)),
            message(2, filled(7, 10)),
        ])
        .await;
        assert!(folded.live_sell_for_token_contract(TC_A).is_none());
    }

    #[tokio::test]
    async fn event_fold_reduces_on_partial_fill() {
        let folded = fold(vec![
            message(1, placed(7, false, 10, TC_A)),
            message(2, filled(7, 4)),
        ])
        .await;
        assert_eq!(
            folded
                .live_sell_for_token_contract(TC_A)
                .map(|order| order.ticks_remaining),
            Some(6)
        );
    }

    #[tokio::test]
    async fn event_fold_ignores_buy_orders() {
        let folded = fold(vec![message(1, placed(7, true, 10, TC_A))]).await;
        assert!(folded.live_sell_for_token_contract(TC_A).is_none());
        assert_eq!(folded.live_orders().count(), 1);
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
        assert_eq!(folded.live_orders().count(), 2);
        assert!(folded.live_sell_for_token_contract(TC_A).is_some());
        assert!(folded.live_sell_for_token_contract(TC_B).is_some());
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
        assert_eq!(second.live_orders().count(), 2);
        assert!(second.live_sell_for_token_contract(TC_A).is_some());
        assert!(second.live_sell_for_token_contract(TC_B).is_some());
    }
}
