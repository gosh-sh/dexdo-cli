use super::{render_depth_output, render_depth_table, InferenceDepthResponse};

const ENDPOINT: &str = "http://indexer.example.test:8080";
const ADDRESS: &str = "0:4a04daaf8aff55a23c8dd5edabf7c81eeb300c7b5d70ad0c6fa955c25eab0b76";
const AS_OF: u64 = 1_754_006_400;

fn depth() -> InferenceDepthResponse {
    InferenceDepthResponse {
        inference_order_book_address: ADDRESS.to_string(),
        last_update_id: "1782897900:7".to_string(),
        bids: vec![["1000".to_string(), "4".to_string()]],
        asks: vec![
            ["1100".to_string(), "2".to_string()],
            ["1200".to_string(), "1".to_string()],
        ],
    }
}

#[test]
fn depth_provenance_names_raw_ungated_indexer_scope() {
    let rendered = render_depth_output(&depth(), ENDPOINT, AS_OF);
    let provenance = rendered.lines().next().expect("provenance line");

    assert_eq!(
        provenance,
        "depth source=indexer lastUpdateId=1782897900:7 as_of=1754006400 \
         rows=indexer:depth-levels \
         scope=raw-indexer-levels-ungated"
    );
}

#[test]
fn depth_provenance_preserves_level_rows_and_order() {
    let response = depth();
    let before = render_depth_table(&response, ENDPOINT);
    let after = render_depth_output(&response, ENDPOINT, AS_OF);
    let (_, table) = after
        .split_once('\n')
        .expect("provenance before depth table");

    assert_eq!(table, before);
    assert_eq!(
        table.lines().skip(1).collect::<Vec<_>>(),
        vec![
            "bid price_per_tick=1000 ticks=4",
            "ask price_per_tick=1100 ticks=2",
            "ask price_per_tick=1200 ticks=1",
        ]
    );
}
