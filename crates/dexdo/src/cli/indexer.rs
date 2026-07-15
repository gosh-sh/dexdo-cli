use anyhow::{bail, Context, Result};
use reqwest::Url;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::time::Duration;

pub(crate) const DEFAULT_INDEXER_URL: &str = "http://dodex-dev.ackinacki.org:8080";
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
    pub(crate) model: InferenceModel,
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
pub(crate) struct InferenceModel {
    pub(crate) producer: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) version: Option<String>,
    #[serde(rename = "ref")]
    pub(crate) ref_: String,
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

pub(crate) fn resolve_base_url(explicit: Option<&str>) -> Result<String> {
    let raw = match explicit {
        Some(value) => value.to_string(),
        None => std::env::var(INDEXER_URL_ENV).unwrap_or_else(|_| DEFAULT_INDEXER_URL.to_string()),
    };
    normalize_base_url(&raw)
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
        response.inference_order_book_address,
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
        "market address={} model_ref={} producer={} name={} version={} status={} quote_asset={} maker_commission={} taker_commission={} price_precision={} quantity_precision={} tick_size={} step_size={} min_notional={} reference_price={} created_at={}",
        market.inference_order_book_address,
        market.model.ref_,
        opt(&market.model.producer),
        opt(&market.model.name),
        opt(&market.model.version),
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

fn opt(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("-")
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
        if market.model.ref_.trim().is_empty() {
            bail!(
                "market {}: model.ref must not be empty",
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
    let Some(hex) = value.strip_prefix("0:") else {
        bail!("{name} `{value}` is not a valid shellnet address: expected 0:<64 hex>");
    };
    if hex.len() != 64 || !hex.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        bail!("{name} `{value}` is not a valid shellnet address: expected 0:<64 hex>");
    }
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
    const MAX_BODY: usize = 2048;
    let compact = body.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= MAX_BODY {
        compact
    } else {
        let mut end = MAX_BODY;
        while !compact.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &compact[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::sleep;

    const ADDRESS: &str = "0:4a04daaf8aff55a23c8dd5edabf7c81eeb300c7b5d70ad0c6fa955c25eab0b76";

    fn markets_fixture() -> String {
        format!(
            r#"{{
  "serverTime": 1782897900000,
  "nextCursor": "MTc4Mjg4NDY0MTAwMDAwMDo0",
  "hasMore": true,
  "markets": [
    {{
      "inferenceOrderBookAddress": "{ADDRESS}",
      "model": {{"producer": null, "name": null, "version": null, "ref": "qwen--qwen3--32b"}},
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

    #[test]
    fn parses_markets_fixture_and_renders_table() {
        let response = parse_markets_json(markets_fixture().as_bytes()).unwrap();
        assert_eq!(response.markets.len(), 1);
        assert_eq!(response.markets[0].model.ref_, "qwen--qwen3--32b");
        let rendered = render_markets_table(&response, DEFAULT_INDEXER_URL);
        assert_eq!(
            rendered,
            concat!(
                "market_data source=indexer endpoint=http://dodex-dev.ackinacki.org:8080 server_time=1782897900000 count=1 has_more=true next_cursor=MTc4Mjg4NDY0MTAwMDAwMDo0\n",
                "market address=0:4a04daaf8aff55a23c8dd5edabf7c81eeb300c7b5d70ad0c6fa955c25eab0b76 model_ref=qwen--qwen3--32b producer=- name=- version=- status=TRADING quote_asset=SHELL maker_commission=-0.02 taker_commission=0.025 price_precision=9 quantity_precision=0 tick_size=0.000000001 step_size=1 min_notional=0.000000001 reference_price=- created_at=1782897852\n"
            )
        );
    }

    #[test]
    fn parses_depth_fixture_and_renders_table() {
        let response = parse_depth_json(depth_fixture().as_bytes()).unwrap();
        let rendered = render_depth_table(&response, DEFAULT_INDEXER_URL);
        assert_eq!(
            rendered,
            concat!(
                "depth source=indexer endpoint=http://dodex-dev.ackinacki.org:8080 address=0:4a04daaf8aff55a23c8dd5edabf7c81eeb300c7b5d70ad0c6fa955c25eab0b76 last_update_id=1782897900:7 bid_levels=1 ask_levels=2\n",
                "bid price_per_tick=1000 ticks=4\n",
                "ask price_per_tick=1100 ticks=2\n",
                "ask price_per_tick=1200 ticks=1\n"
            )
        );
    }

    #[test]
    fn json_output_shape_is_stable() {
        let response = parse_markets_json(markets_fixture().as_bytes()).unwrap();
        let json = serde_json::to_string_pretty(&response.markets[0]).unwrap();
        assert!(json.contains(r#""inferenceOrderBookAddress": "0:4a04"#));
        assert!(json.contains(r#""ref": "qwen--qwen3--32b""#));
        assert!(json.contains(r#""quoteAsset": "SHELL""#));
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
            render_markets_table(&response, DEFAULT_INDEXER_URL),
            "market_data source=indexer endpoint=http://dodex-dev.ackinacki.org:8080 server_time=1 count=0 has_more=false next_cursor=-\nmarkets none=true\n"
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
        let body = format!("{}\u{00e9}{}", "a".repeat(2047), "b".repeat(16));
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
