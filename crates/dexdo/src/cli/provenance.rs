//! Data-source and freshness annotation for the read-only order-book views.

//! `dexdo orders list` and `dexdo market` read the same book through different paths and show
//! different subsets of it, so they legitimately disagree. Until now neither said where its rows
//! came from or how fresh they were, so the divergence read as contradictory truth (, third
//! motivating example). `dexdo market-data depth` adds a third, raw indexer view. All three print
//! the same keys with the same meaning, so a difference reads as "different scope" or "indexer lag"
//! rather than "one of these is lying".

/// Rows folded from the order book's own chain events (authoritative).
pub(crate) const ROWS_CHAIN_EVENTS: &str = "chain:order-book-events";
/// Rows read through the legacy contract getters (the fallback when the event fold is unavailable).
pub(crate) const ROWS_CHAIN_GETTERS: &str = "chain:getters";
/// Aggregated bid/ask levels returned by the indexer depth endpoint.
pub(crate) const ROWS_INDEXER_DEPTH: &str = "indexer:depth-levels";
/// Only the querying note's own resting orders (`orders list`).
pub(crate) const SCOPE_OWNER_RESTING: &str = "owner-resting-orders";
/// Only asks a buy could actually match (`market`).
pub(crate) const SCOPE_EXECUTABLE_ASKS: &str = "executable-asks";
/// Raw indexer levels; neither expiry nor TokenContract liveness is applied (`market-data depth`).
pub(crate) const SCOPE_RAW_INDEXER_LEVELS_UNGATED: &str = "raw-indexer-levels-ungated";

/// Wall-clock seconds at which the snapshot was read. A pre-epoch clock is an error.
pub(crate) fn now_unix_at(now: std::time::SystemTime) -> Result<u64, dexdo_core::ChainError> {
    now.duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|error| {
            dexdo_core::ChainError::Chain(format!(
                "cannot derive a finite render snapshot timestamp from the system clock: {error}"
            ))
        })
}

pub(crate) fn now_unix() -> Result<u64, dexdo_core::ChainError> {
    now_unix_at(std::time::SystemTime::now())
}

/// The provenance suffix, in ONE vocabulary so the views can be compared key for key.

/// * `source` -- where the freshness marker came from: `indexer` (lags the chain by design) or
/// `chain`.
/// * `last_update_id` -- that marker, `-` when the source does not publish one.
/// * `as_of` -- Unix seconds at which this snapshot was read.
/// * `rows` -- where the displayed rows came from ([`ROWS_CHAIN_EVENTS`] / [`ROWS_CHAIN_GETTERS`] /
/// [`ROWS_INDEXER_DEPTH`]).
/// * `scope` -- which subset of the book is displayed ([`SCOPE_OWNER_RESTING`] /
/// [`SCOPE_EXECUTABLE_ASKS`] / [`SCOPE_RAW_INDEXER_LEVELS_UNGATED`]); a differing scope is the
/// other reason two views disagree.
pub(crate) fn render(
    source: &str,
    last_update_id: &str,
    as_of: u64,
    rows: &str,
    scope: &str,
) -> String {
    format!("source={source} lastUpdateId={last_update_id} as_of={as_of} rows={rows} scope={scope}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_views_render_the_same_keys_in_the_same_order() {
        let market = render(
            "indexer",
            "indexer-77",
            1_754_006_400,
            ROWS_CHAIN_EVENTS,
            SCOPE_EXECUTABLE_ASKS,
        );
        let orders = render(
            "chain",
            "fold-13",
            1_754_006_400,
            ROWS_CHAIN_GETTERS,
            SCOPE_OWNER_RESTING,
        );
        assert_eq!(
            market,
            "source=indexer lastUpdateId=indexer-77 as_of=1754006400 \
             rows=chain:order-book-events scope=executable-asks"
        );
        assert_eq!(
            orders,
            "source=chain lastUpdateId=fold-13 as_of=1754006400 rows=chain:getters \
             scope=owner-resting-orders"
        );
        // Key-for-key comparable: a reader can diff the two lines field by field.
        let keys = |line: &str| {
            line.split_whitespace()
                .filter_map(|pair| pair.split('=').next().map(str::to_string))
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&market), keys(&orders));
    }

    #[test]
    fn as_of_is_a_real_unix_timestamp() {
        // Sanity: not zero, and after this code was written.
        assert!(
            now_unix().expect("system clock") > 1_700_000_000,
            "clock is before 2023"
        );
    }
}
