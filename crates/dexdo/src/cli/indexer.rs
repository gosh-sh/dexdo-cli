use anyhow::{bail, Context, Result};
use dexdo_core::params::INDEXER_ERROR_BODY_MAX_BYTES;
use reqwest::Url;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::time::Duration;

pub(crate) const INDEXER_URL_ENV: &str = "DEXDO_INDEXER_URL";

#[derive(Clone, Debug)]
pub(crate) struct IndexerClient {
    base_url: String,
    http: reqwest::Client,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct MarketsQuery<'a> {
    pub(crate) inference_order_book_address: Option<&'a str>,
    pub(crate) producer: Option<&'a str>,
    pub(crate) status: Option<&'a str>,
    pub(crate) cursor: Option<&'a str>,
    pub(crate) limit: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DepthQuery<'a> {
    pub(crate) inference_order_book_address: &'a str,
    pub(crate) limit: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InferenceMarketsResponse {
    pub(crate) server_time: i64,
    pub(crate) next_cursor: Option<String>,
    pub(crate) has_more: bool,
    pub(crate) markets: Vec<InferenceMarket>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InferenceMarket {
    pub(crate) inference_order_book_address: String,
    pub(crate) model_ref_name: String,
    pub(crate) contract_version: String,
    pub(crate) status: String,
    pub(crate) quote_asset: String,
    pub(crate) maker_commission: String,
    pub(crate) taker_commission: String,
    pub(crate) price_precision: i32,
    pub(crate) quantity_precision: i32,
    pub(crate) tick_size: String,
    pub(crate) step_size: String,
    pub(crate) min_notional: String,
    pub(crate) reference_price: Option<String>,
    pub(crate) created_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InferenceDepthResponse {
    pub(crate) inference_order_book_address: String,
    pub(crate) last_update_id: String,
    pub(crate) bids: Vec<[String; 2]>,
    pub(crate) asks: Vec<[String; 2]>,
}

impl IndexerClient {
    pub(crate) fn new(base_url: String, timeout: Duration) -> Result<Self> {
        let base_url = normalize_base_url(&base_url)?;
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("build Dodex indexer HTTP client")?;
        Ok(Self { base_url, http })
    }

    pub(crate) fn base_url(&self) -> &str {
        &self.base_url
    }

    pub(crate) async fn markets(
        &self,
        query: MarketsQuery<'_>,
    ) -> Result<InferenceMarketsResponse> {
        validate_cursor(query.cursor)?;
        validate_optional_address(
            "inferenceOrderBookAddress",
            query.inference_order_book_address,
        )?;
        let mut url = self.endpoint("/api/v1/inference/markets")?;
        {
            let mut qp = url.query_pairs_mut();
            if let Some(address) = query.inference_order_book_address {
                qp.append_pair("inferenceOrderBookAddress", address);
            }
            if let Some(producer) = non_empty_trimmed(query.producer, "producer")? {
                qp.append_pair("producer", producer);
            }
            if let Some(status) = non_empty_trimmed(query.status, "status")? {
                qp.append_pair("status", status);
            }
            if let Some(cursor) = query.cursor {
                qp.append_pair("cursor", cursor);
            }
            if let Some(limit) = query.limit {
                qp.append_pair("limit", &limit.to_string());
            }
        }
        let response = self.get_json::<InferenceMarketsResponse>(url).await?;
        validate_markets_response(&response)?;
        Ok(response)
    }

    pub(crate) async fn depth(&self, query: DepthQuery<'_>) -> Result<InferenceDepthResponse> {
        validate_address(
            "inferenceOrderBookAddress",
            query.inference_order_book_address,
        )?;
        let mut url = self.endpoint("/api/v1/inference/depth")?;
        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair(
                "inferenceOrderBookAddress",
                query.inference_order_book_address,
            );
            if let Some(limit) = query.limit {
                qp.append_pair("limit", &limit.to_string());
            }
        }
        let response = self.get_json::<InferenceDepthResponse>(url).await?;
        validate_depth_response(&response)?;
        Ok(response)
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        Url::parse(&format!("{}{}", self.base_url, path))
            .with_context(|| format!("invalid Dodex indexer endpoint {}{}", self.base_url, path))
    }

    async fn get_json<T: DeserializeOwned>(&self, url: Url) -> Result<T> {
        let response = self
            .http
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("Dodex indexer GET {url} failed"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("Dodex indexer GET {url}: read response body"))?;
        if !status.is_success() {
            bail!(
                "Dodex indexer GET {url}: HTTP {status}: {}",
                compact_body(&body)
            );
        }
        serde_json::from_str(&body)
            .with_context(|| format!("Dodex indexer GET {url}: parse JSON response"))
    }
}

/// What a deployed-contracts manifest says about the indexer: the chain it is for, and an indexer of
/// its own if it names one.

/// Read from the raw JSON rather than through `dexdo_core::Deployed`, which used to be compiled only
/// behind the cargo feature removed. `market-data` signs nothing and ships in the default
/// build, so its manifest read has to as well.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManifestIndexer {
    network: String,
    indexer: Option<String>,
}

impl ManifestIndexer {
    pub(crate) fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read the manifest {}", path.display()))?;
        Self::from_json(&text).with_context(|| format!("manifest {}", path.display()))
    }

    pub(crate) fn from_json(text: &str) -> Result<Self> {
        let json: serde_json::Value =
            serde_json::from_str(text).context("parse deployed-contracts manifest JSON")?;
        let network = json
            .get("network")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "deployed-contracts manifest has no `network` field, so there is nothing to say \
                     which chain its addresses are on"
                )
            })?;
        Ok(Self {
            network: network.to_string(),
            indexer: json
                .get("indexer")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        })
    }
}

/// The market-data indexer this run reads, in descending order of authority: `--indexer-url`, the
/// `DEXDO_INDEXER_URL` environment fallback, the manifest's own `indexer` field, and only then the
/// indexer of the network the manifest declares.

/// **A network label is not an indexer address, and an indexer is not network-agnostic.**
/// One compile-time default answered for every manifest, so pointing the CLI at mainnet listed the
/// one chain's acceptance book as another's, under a header that names only the indexer host. An
/// indexer serves one chain; when the declared network has none this tree knows, that is a refusal,
/// because a market address taken from the wrong chain's list does not exist on the chain the
/// operator is about to spend on.

/// With no manifest the operator named no network, so the documented default still answers.
pub(crate) fn resolve_base_url(
    explicit: Option<&str>,
    manifest: Option<&ManifestIndexer>,
) -> Result<String> {
    resolve_base_url_with_env(
        explicit,
        manifest,
        std::env::var(INDEXER_URL_ENV).ok().as_deref(),
    )
}

/// The same order with the environment supplied rather than read, so the decision can be pinned
/// without a test depending on the environment the suite happens to run in.
pub(crate) fn resolve_base_url_with_env(
    explicit: Option<&str>,
    manifest: Option<&ManifestIndexer>,
    env: Option<&str>,
) -> Result<String> {
    if let Some(value) = explicit {
        return normalize_base_url(value);
    }
    if let Some(value) = env {
        return normalize_base_url(value);
    }
    let Some(manifest) = manifest else {
        anyhow::bail!(
            "no manifest, so no network, so no indexer. An indexer answers for ONE chain and this \
             run cannot say which chain it is on: point {} at a manifest, pass `--indexer-url \
             <url>`, or set `{INDEXER_URL_ENV}`.",
            dexdo_core::params::MANIFEST_PATH_VAR
        );
    };
    if let Some(indexer) = manifest.indexer.as_deref() {
        return normalize_base_url(indexer);
    }
    anyhow::bail!(
        "the manifest declares network `{}` and carries no `indexer`, so nothing says who to ask. \
         An indexer answers for ONE chain, and there is no table of known indexers to fall back \
         on: answering from another chain's would list books that do not exist on `{}`. Pass \
         `--indexer-url <url>`, set `{INDEXER_URL_ENV}`, or add an `indexer` field to the manifest.",
        manifest.network,
        manifest.network
    )
}

pub(crate) fn timeout_from_ms(timeout_ms: u64) -> Result<Duration> {
    if timeout_ms == 0 {
        bail!("--timeout-ms must be > 0");
    }
    Ok(Duration::from_millis(timeout_ms))
}

#[cfg(test)]
pub(crate) fn parse_markets_json(bytes: &[u8]) -> Result<InferenceMarketsResponse> {
    let response: InferenceMarketsResponse =
        serde_json::from_slice(bytes).context("parse /api/v1/inference/markets JSON")?;
    validate_markets_response(&response)?;
    Ok(response)
}

#[cfg(test)]
pub(crate) fn parse_depth_json(bytes: &[u8]) -> Result<InferenceDepthResponse> {
    let response: InferenceDepthResponse =
        serde_json::from_slice(bytes).context("parse /api/v1/inference/depth JSON")?;
    validate_depth_response(&response)?;
    Ok(response)
}

pub(crate) fn render_markets_table(response: &InferenceMarketsResponse, endpoint: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "market_data source=indexer endpoint={} server_time={} count={} has_more={} next_cursor={}\n",
        endpoint,
        response.server_time,
        response.markets.len(),
        response.has_more,
        response.next_cursor.as_deref().unwrap_or("-")
    ));
    if response.markets.is_empty() {
        out.push_str("markets none=true\n");
        return out;
    }
    for market in &response.markets {
        out.push_str(&render_market_line(market));
        out.push('\n');
    }
    out
}

pub(crate) fn render_market_table(market: &InferenceMarket) -> String {
    let mut out = render_market_line(market);
    out.push('\n');
    out
}

pub(crate) fn render_depth_table(response: &InferenceDepthResponse, endpoint: &str) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "depth source=indexer endpoint={} address={} last_update_id={} bid_levels={} ask_levels={}\n",
        endpoint,
        dexdo_core::address::display(&response.inference_order_book_address),
        if response.last_update_id.is_empty() {
            "-"
        } else {
            response.last_update_id.as_str()
        },
        response.bids.len(),
        response.asks.len()
    ));
    for bid in &response.bids {
        out.push_str(&format!("bid price_per_tick={} ticks={}\n", bid[0], bid[1]));
    }
    for ask in &response.asks {
        out.push_str(&format!("ask price_per_tick={} ticks={}\n", ask[0], ask[1]));
    }
    out
}

pub(crate) fn render_depth_output(
    response: &InferenceDepthResponse,
    endpoint: &str,
    as_of: u64,
) -> String {
    let last_update_id = if response.last_update_id.is_empty() {
        "-"
    } else {
        response.last_update_id.as_str()
    };
    format!(
        "depth {}\n{}",
        crate::cli::provenance::render(
            "indexer",
            last_update_id,
            as_of,
            crate::cli::provenance::ROWS_INDEXER_DEPTH,
            crate::cli::provenance::SCOPE_RAW_INDEXER_LEVELS_UNGATED,
        ),
        render_depth_table(response, endpoint)
    )
}

pub(crate) fn validate_cursor(cursor: Option<&str>) -> Result<()> {
    let Some(cursor) = cursor else {
        return Ok(());
    };
    if cursor.trim().is_empty() {
        bail!("--cursor must not be empty");
    }
    if cursor.chars().any(char::is_whitespace) {
        bail!("--cursor must be the opaque token exactly as returned by the indexer, without whitespace");
    }
    if cursor.chars().any(char::is_control) {
        bail!("--cursor must not contain control characters");
    }
    Ok(())
}

fn render_market_line(market: &InferenceMarket) -> String {
    format!(
        "market address={} model_ref={} contract_version={} status={} quote_asset={} maker_commission={} taker_commission={} price_precision={} quantity_precision={} tick_size={} step_size={} min_notional={} reference_price={} created_at={}",
        dexdo_core::address::display(&market.inference_order_book_address),
        market.model_ref_name,
        market.contract_version,
        market.status,
        market.quote_asset,
        market.maker_commission,
        market.taker_commission,
        market.price_precision,
        market.quantity_precision,
        market.tick_size,
        market.step_size,
        market.min_notional,
        market.reference_price.as_deref().unwrap_or("-"),
        market.created_at
    )
}

fn normalize_base_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("Dodex indexer base URL is empty");
    }
    let parsed =
        Url::parse(trimmed).with_context(|| format!("invalid Dodex indexer URL `{raw}`"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => bail!("Dodex indexer URL must use http or https, got `{scheme}`"),
    }
    if parsed.host_str().is_none() {
        bail!("Dodex indexer URL must include a host");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("Dodex indexer URL must not include query or fragment");
    }
    Ok(trimmed.to_string())
}

fn validate_markets_response(response: &InferenceMarketsResponse) -> Result<()> {
    for market in &response.markets {
        validate_address(
            "inferenceOrderBookAddress",
            &market.inference_order_book_address,
        )?;
        if market.model_ref_name.trim().is_empty() {
            bail!(
                "market {}: modelRefName must not be empty",
                market.inference_order_book_address
            );
        }
        if market.contract_version.trim().is_empty() {
            bail!(
                "market {}: contractVersion must not be empty",
                market.inference_order_book_address
            );
        }
        if market.status.trim().is_empty() {
            bail!(
                "market {}: status must not be empty",
                market.inference_order_book_address
            );
        }
        if market.quote_asset.trim().is_empty() {
            bail!(
                "market {}: quoteAsset must not be empty",
                market.inference_order_book_address
            );
        }
    }
    Ok(())
}

fn validate_depth_response(response: &InferenceDepthResponse) -> Result<()> {
    validate_address(
        "inferenceOrderBookAddress",
        &response.inference_order_book_address,
    )?;
    Ok(())
}

fn validate_optional_address(name: &str, value: Option<&str>) -> Result<()> {
    if let Some(value) = value {
        validate_address(name, value)?;
    }
    Ok(())
}

fn validate_address(name: &str, value: &str) -> Result<()> {
    dexdo_core::address::CanonicalAddress::parse(value).map_err(|error| {
        anyhow::anyhow!(
            "{name} `{value}` is not a valid {} address: {error}",
            dexdo_core::params::current_network()
        )
    })?;
    Ok(())
}

fn non_empty_trimmed<'a>(value: Option<&'a str>, name: &str) -> Result<Option<&'a str>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("--{name} must not be empty");
    }
    Ok(Some(trimmed))
}

fn compact_body(body: &str) -> String {
    let compact = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= INDEXER_ERROR_BODY_MAX_BYTES {
        compact
    } else {
        let mut end = INDEXER_ERROR_BODY_MAX_BYTES;
        while !compact.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &compact[..end])
    }
}

#[cfg(test)]
#[path = "indexer_depth_provenance_1043.rs"]
mod indexer_depth_provenance_1043;

#[cfg(test)]
mod tests {
    /// A stand-in indexer address for rendering tests. Deliberately not a real network's: what is
    /// under test is the TABLE, and a live host in a fixture is how a network name gets back into
    /// the sources.
    const TEST_INDEXER_URL: &str = "http://indexer.example:8080";

    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::sleep;

    const ADDRESS: &str = "0:4a04daaf8aff55a23c8dd5edabf7c81eeb300c7b5d70ad0c6fa955c25eab0b76";

    const MAINNET_MARKETS_FIXTURE: &[u8] = br#"{
  "serverTime": 1788794575,
  "nextCursor": "MTc4ODc5MDEyOTAwMDAwMDoxNzg2",
  "hasMore": true,
  "markets": [{
    "inferenceOrderBookAddress": "0:33c30af6f32f86623dc9711eedc1779e35e7e7c8f7dfe38db55d20f3a51eb8af",
    "modelRefName": "gpt-5.6-sol",
    "contractVersion": "4.0.36",
    "status": "TRADING",
    "quoteAsset": "SHELL",
    "makerCommission": "-0.02",
    "takerCommission": "0.025",
    "pricePrecision": 9,
    "quantityPrecision": 0,
    "tickSize": "1",
    "stepSize": "1",
    "minNotional": "1",
    "referencePrice": null,
    "createdAt": 1788790129
  }]
}"#;

    const SHELLNET_MARKETS_FIXTURE: &[u8] = br#"{
  "serverTime": 1788794583,
  "nextCursor": "MTc4ODc5MTQ0MjAwMDAwMDo3Nzcw",
  "hasMore": true,
  "markets": [{
    "inferenceOrderBookAddress": "0:ab7c42a4eba813969aba212899c875aa8c3cff6ad57d59af80d7790d1e95881b",
    "modelRefName": "Qwen3.6-27B-oracle-noliquidity-1788791441",
    "contractVersion": "4.0.36",
    "status": "TRADING",
    "quoteAsset": "SHELL",
    "makerCommission": "-0.02",
    "takerCommission": "0.025",
    "pricePrecision": 9,
    "quantityPrecision": 0,
    "tickSize": "1",
    "stepSize": "1",
    "minNotional": "1",
    "referencePrice": null,
    "createdAt": 1788791442
  }]
}"#;

    fn markets_fixture() -> String {
        format!(
            r#"{{
  "serverTime": 1782897900000,
  "nextCursor": "MTc4Mjg4NDY0MTAwMDAwMDo0",
  "hasMore": true,
  "markets": [
    {{
      "inferenceOrderBookAddress": "{ADDRESS}",
      "modelRefName": "qwen--qwen3--32b",
      "contractVersion": "4.0.36",
      "status": "TRADING",
      "quoteAsset": "SHELL",
      "makerCommission": "-0.02",
      "takerCommission": "0.025",
      "pricePrecision": 9,
      "quantityPrecision": 0,
      "tickSize": "0.000000001",
      "stepSize": "1",
      "minNotional": "0.000000001",
      "referencePrice": null,
      "createdAt": 1782897852
    }}
  ]
}}"#
        )
    }

    fn depth_fixture() -> String {
        format!(
            r#"{{
  "inferenceOrderBookAddress": "{ADDRESS}",
  "lastUpdateId": "1782897900:7",
  "bids": [["1000", "4"]],
  "asks": [["1100", "2"], ["1200", "1"]]
}}"#
        )
    }

    /// The same account in the canonical `<dapp_id>::<account_id>` form the human tables render
    /// . An `InferenceOrderBook` is a system contract of the shared dexdo DApp, so its DApp
    /// half is `DEXDO_DAPP_ID`. The raw `--output json` DTO keeps the legacy machine form, which
    /// `json_output_shape_is_stable` below still pins.
    fn canonical_address() -> String {
        format!(
            "{}::{}",
            dexdo_core::DEXDO_DAPP_ID,
            ADDRESS.strip_prefix("0:").unwrap()
        )
    }

    #[test]
    fn parses_current_mainnet_and_shellnet_market_shapes() {
        for (network, fixture, expected_model_ref) in [
            ("mainnet", MAINNET_MARKETS_FIXTURE, "gpt-5.6-sol"),
            (
                "shellnet",
                SHELLNET_MARKETS_FIXTURE,
                "Qwen3.6-27B-oracle-noliquidity-1788791441",
            ),
        ] {
            let response = parse_markets_json(fixture)
                .unwrap_or_else(|error| panic!("{network} fixture must parse: {error:#}"));
            let market = serde_json::to_value(&response.markets[0]).unwrap();
            assert_eq!(market["modelRefName"], expected_model_ref);
            assert_eq!(market["contractVersion"], "4.0.36");
            assert!(market.get("model").is_none());
        }
    }

    #[test]
    fn parses_markets_fixture_and_renders_table() {
        let response = parse_markets_json(markets_fixture().as_bytes()).unwrap();
        assert_eq!(response.markets.len(), 1);
        assert_eq!(response.markets[0].model_ref_name, "qwen--qwen3--32b");
        assert_eq!(response.markets[0].contract_version, "4.0.36");
        let rendered = render_markets_table(&response, TEST_INDEXER_URL);
        assert_eq!(
            rendered,
            format!(
                concat!(
                    "market_data source=indexer endpoint=http://indexer.example:8080 server_time=1782897900000 count=1 has_more=true next_cursor=MTc4Mjg4NDY0MTAwMDAwMDo0\n",
                    "market address={address} model_ref=qwen--qwen3--32b contract_version=4.0.36 status=TRADING quote_asset=SHELL maker_commission=-0.02 taker_commission=0.025 price_precision=9 quantity_precision=0 tick_size=0.000000001 step_size=1 min_notional=0.000000001 reference_price=- created_at=1782897852\n"
                ),
                address = canonical_address()
            )
        );
    }

    #[test]
    fn parses_depth_fixture_and_renders_table() {
        let response = parse_depth_json(depth_fixture().as_bytes()).unwrap();
        let rendered = render_depth_table(&response, TEST_INDEXER_URL);
        assert_eq!(
            rendered,
            format!(
                concat!(
                    "depth source=indexer endpoint=http://indexer.example:8080 address={address} last_update_id=1782897900:7 bid_levels=1 ask_levels=2\n",
                    "bid price_per_tick=1000 ticks=4\n",
                    "ask price_per_tick=1100 ticks=2\n",
                    "ask price_per_tick=1200 ticks=1\n"
                ),
                address = canonical_address()
            )
        );
    }

    #[test]
    fn json_output_shape_is_stable() {
        let response = parse_markets_json(markets_fixture().as_bytes()).unwrap();
        let json = serde_json::to_value(&response.markets[0]).unwrap();
        assert_eq!(json["inferenceOrderBookAddress"], ADDRESS);
        assert_eq!(json["modelRefName"], "qwen--qwen3--32b");
        assert_eq!(json["contractVersion"], "4.0.36");
        assert_eq!(json["quoteAsset"], "SHELL");
        assert!(json.get("model").is_none());
    }

    #[test]
    fn rejects_missing_or_empty_market_identity_fields() {
        let missing_model_ref =
            markets_fixture().replace("      \"modelRefName\": \"qwen--qwen3--32b\",\n", "");
        let error = parse_markets_json(missing_model_ref.as_bytes()).unwrap_err();
        assert!(format!("{error:#}").contains("modelRefName"));

        let missing_contract_version =
            markets_fixture().replace("      \"contractVersion\": \"4.0.36\",\n", "");
        let error = parse_markets_json(missing_contract_version.as_bytes()).unwrap_err();
        assert!(format!("{error:#}").contains("contractVersion"));

        let empty_model_ref = markets_fixture().replace(
            "\"modelRefName\": \"qwen--qwen3--32b\"",
            "\"modelRefName\": \" \"",
        );
        let error = parse_markets_json(empty_model_ref.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("modelRefName"));

        let empty_contract_version = markets_fixture().replace(
            "\"contractVersion\": \"4.0.36\"",
            "\"contractVersion\": \" \"",
        );
        let error = parse_markets_json(empty_contract_version.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("contractVersion"));
    }

    #[test]
    fn rejects_malformed_json_missing_required_fields_and_bad_addresses() {
        assert!(parse_markets_json(br#"{"markets":["#).is_err());
        let missing_quote_asset =
            markets_fixture().replace("      \"quoteAsset\": \"SHELL\",\n", "");
        assert!(parse_markets_json(missing_quote_asset.as_bytes()).is_err());
        let bad_address = markets_fixture().replace(ADDRESS, "not-address");
        let err = parse_markets_json(bad_address.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("inferenceOrderBookAddress"));
        let bad_depth_address = depth_fixture().replace(ADDRESS, "not-address");
        assert!(parse_depth_json(bad_depth_address.as_bytes()).is_err());
    }

    #[test]
    fn empty_market_list_is_explicit() {
        let response = parse_markets_json(
            br#"{"serverTime":1,"nextCursor":null,"hasMore":false,"markets":[]}"#,
        )
        .unwrap();
        assert_eq!(
            render_markets_table(&response, TEST_INDEXER_URL),
            "market_data source=indexer endpoint=http://indexer.example:8080 server_time=1 count=0 has_more=false next_cursor=-\nmarkets none=true\n"
        );
    }

    #[test]
    fn rejects_bad_cursor_and_bad_timeout() {
        assert!(validate_cursor(Some("")).is_err());
        assert!(validate_cursor(Some("abc def")).is_err());
        assert!(validate_cursor(Some("MTc4Mjg4NDY0MTAwMDAwMDo0")).is_ok());
        assert!(timeout_from_ms(0).is_err());
    }

    #[tokio::test]
    async fn http_error_status_is_fail_loud() {
        let base = serve_once(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 19\r\n\r\n{\"code\":-1,\"msg\":\"x\"}",
            Duration::ZERO,
        )
        .await;
        let client = IndexerClient::new(base, Duration::from_secs(2)).unwrap();
        let err = client.markets(MarketsQuery::default()).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("HTTP 500"), "{msg}");
        assert!(msg.contains("\"code\":-1"), "{msg}");
    }

    #[tokio::test]
    async fn unicode_http_error_body_truncates_without_panicking() {
        let body = format!(
            "{}\u{00e9}{}",
            "a".repeat(INDEXER_ERROR_BODY_MAX_BYTES - 1),
            "b".repeat(16)
        );
        let response = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let base = serve_once_owned(response, Duration::ZERO).await;
        let client = IndexerClient::new(base, Duration::from_secs(2)).unwrap();
        let error = client
            .markets(MarketsQuery::default())
            .await
            .expect_err("remote Unicode error body must return the HTTP failure");
        let message = error.to_string();
        assert!(message.contains("HTTP 500"), "{message}");
        assert!(message.ends_with("..."), "{message}");
    }

    #[tokio::test]
    async fn timeout_is_fail_loud() {
        let base = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
            Duration::from_millis(250),
        )
        .await;
        let client = IndexerClient::new(base, Duration::from_millis(25)).unwrap();
        let err = client.markets(MarketsQuery::default()).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Dodex indexer GET"), "{msg}");
        assert!(
            msg.contains("operation timed out")
                || msg.contains("deadline has elapsed")
                || msg.contains("timed out"),
            "{msg}"
        );
    }

    async fn serve_once(response: &'static str, delay: Duration) -> String {
        serve_once_owned(response.to_string(), delay).await
    }

    async fn serve_once_owned(response: String, delay: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0_u8; 1024];
            let _ = stream.read(&mut buf).await;
            if !delay.is_zero() {
                sleep(delay).await;
            }
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{addr}")
    }
}
