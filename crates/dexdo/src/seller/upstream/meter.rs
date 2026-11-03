//! Provider-native billing read from a forwarded response.

//! Money is authorized only by the non-cached part of the provider's terminal input + output record. Each format
//! states it in its own place; every format follows the same rules: one record per request (an
//! identical restatement is the same record), no output after it, output requires a positive
//! output count, and a response without output must not bill.

use crate::api_format::{ApiFormat, ResponseFacts};
use dexdo_proto::BillingUsage;
use serde_json::Value;
use tonic::Status;

/// Billing state of one forwarded response.
pub(crate) enum UsageMeter {
    /// Chat Completions and Responses state one aggregate record.
    Aggregate {
        format: ApiFormat,
        usage: Option<BillingUsage>,
    },
    /// Anthropic Messages states input first and a cumulative output count afterwards.
    Messages(MessagesMeter),
}

impl UsageMeter {
    pub(crate) fn new(format: ApiFormat) -> Self {
        match format {
            ApiFormat::Messages => Self::Messages(MessagesMeter::default()),
            format => Self::Aggregate {
                format,
                usage: None,
            },
        }
    }

    /// Observe one streamed event before it is forwarded.
    #[allow(clippy::result_large_err)]
    pub(crate) fn observe(&mut self, value: &Value, facts: &ResponseFacts) -> Result<(), Status> {
        match self {
            Self::Aggregate { format, usage } => {
                let reported = match format {
                    ApiFormat::Responses if facts.done => {
                        responses_usage(value.get("response").and_then(|r| r.get("usage")))?
                    }
                    ApiFormat::Responses => None,
                    _ => chat_frame_usage(value)?,
                };
                if let Some(reported) = reported {
                    // UPS-29: a content-carrying frame is not a terminal record.
                    if facts.has_output() {
                        return Err(Status::data_loss(
                            "provider usage is attached to an output delta, not a terminal record",
                        ));
                    }
                    // UPS-24: an identical restatement is the same record; two totals are refused.
                    if usage.is_some_and(|recorded| recorded != reported) {
                        return Err(Status::data_loss(
                            "provider stream reported contradictory terminal usage totals",
                        ));
                    }
                    *usage = Some(reported);
                } else if facts.has_output() && usage.is_some() {
                    // UPS-30: output after the bill cannot be ignored while the bill is kept.
                    return Err(Status::data_loss(
                        "provider output continued after the terminal usage record",
                    ));
                }
                Ok(())
            }
            Self::Messages(meter) => meter.observe(value, facts),
        }
    }

    /// The record to bill once the response reached its final event.
    #[allow(clippy::result_large_err)]
    pub(crate) fn finish(self, saw_output: bool) -> Result<Option<BillingUsage>, Status> {
        match self {
            Self::Aggregate { usage, .. } => aggregate_verdict(usage, saw_output),
            Self::Messages(meter) => meter.finish(saw_output),
        }
    }

    /// The record of a whole non-streamed response.
    #[allow(clippy::result_large_err)]
    pub(crate) fn body(
        format: ApiFormat,
        value: &Value,
        facts: &ResponseFacts,
    ) -> Result<Option<BillingUsage>, Status> {
        let saw_output = facts.has_output();
        match format {
            ApiFormat::ChatCompletions => aggregate_verdict(chat_frame_usage(value)?, saw_output),
            ApiFormat::Responses => {
                aggregate_verdict(responses_usage(value.get("usage"))?, saw_output)
            }
            ApiFormat::Messages => {
                let usage = value.get("usage");
                let counts = messages_counts(usage)?;
                let input = counts
                    .input
                    .ok_or_else(|| Status::data_loss("Anthropic usage is missing input_tokens"))?;
                let output = counts
                    .output
                    .ok_or_else(|| Status::data_loss("Anthropic usage is missing output_tokens"))?;
                messages_verdict(
                    input,
                    output,
                    counts.cache_read.unwrap_or(0),
                    counts.cache_write.unwrap_or(0),
                    saw_output,
                )
            }
        }
    }
}

#[allow(clippy::result_large_err)]
fn aggregate_verdict(
    usage: Option<BillingUsage>,
    saw_output: bool,
) -> Result<Option<BillingUsage>, Status> {
    if !saw_output {
        let usage = usage.ok_or_else(|| {
            Status::data_loss("provider response ended without terminal billing usage")
        })?;
        // UPS-28: a positive number alone is not proof of delivered service.
        if usage.total_tokens != 0 {
            return Err(Status::data_loss(
                "provider usage reports billable tokens without delivered output",
            ));
        }
        return Ok(None);
    }
    // UPS-02/UPS-07/UPS-20: delivered output bills exactly the provider's positive terminal record.
    usage
        .filter(|usage| usage.output_tokens > 0)
        .map(Some)
        .ok_or_else(|| {
            Status::data_loss("provider output ended without positive terminal output usage")
        })
}

#[allow(clippy::result_large_err)]
fn token(object: &serde_json::Map<String, Value>, field: &str) -> Result<u64, Status> {
    object.get(field).and_then(Value::as_u64).ok_or_else(|| {
        Status::data_loss(format!(
            "provider usage.{field} is missing or is not a token count"
        ))
    })
}

#[allow(clippy::result_large_err)]
fn optional_count(container: &Value, field: &str) -> Result<Option<u64>, Status> {
    match container.get(field) {
        None => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            Status::data_loss(format!("provider usage {field} is not a token count"))
        }),
    }
}

#[allow(clippy::result_large_err)]
fn matching_count(first: Option<u64>, second: Option<u64>) -> Result<Option<u64>, Status> {
    if first.zip(second).is_some_and(|(a, b)| a != b) {
        return Err(Status::data_loss("provider cache counters disagree"));
    }
    Ok(first.or(second))
}

/// Supported OpenAI-compatible cache counters are disjoint subsets of total input.
/// Aliases report the same subset and must agree; they must never be added together.
/// Unknown fields are preserved by the relay and have no billing semantics.
#[allow(clippy::result_large_err)]
fn input_cache_counts(
    container: &Value,
    details: &str,
) -> Result<(Option<u64>, Option<u64>), Status> {
    let mut read = optional_count(container, "prompt_cache_hit_tokens")?;
    let mut write = None;
    if let Some(details) = container.get(details).filter(|v| !v.is_null()) {
        if !details.is_object() {
            return Err(Status::data_loss(
                "provider input token details is not an object",
            ));
        }
        read = matching_count(
            read,
            matching_count(
                optional_count(details, "cached_tokens")?,
                optional_count(details, "cache_read_tokens")?,
            )?,
        )?;
        write = matching_count(
            optional_count(details, "cache_write_tokens")?,
            optional_count(details, "cache_creation_tokens")?,
        )?;
    }
    Ok((read, write))
}

#[allow(clippy::result_large_err)]
fn check_cache_miss(container: &Value, usage: &BillingUsage) -> Result<(), Status> {
    if optional_count(container, "prompt_cache_miss_tokens")?
        .is_some_and(|miss| miss != usage.input_tokens - usage.cache_read_tokens)
    {
        return Err(Status::data_loss(
            "provider cache hit/miss counts disagree with input",
        ));
    }
    Ok(())
}

/// One OpenAI-compatible `usage`-shaped record (E2E-UPS-18/22: untrusted counts are refused).
#[allow(clippy::result_large_err)]
fn chat_usage(container: Option<&Value>) -> Result<Option<BillingUsage>, Status> {
    let Some(container) = container.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let object = container
        .as_object()
        .ok_or_else(|| Status::data_loss("provider usage is not an object"))?;
    if !["prompt_tokens", "completion_tokens", "total_tokens"]
        .iter()
        .any(|field| object.contains_key(*field))
    {
        let (read, write) = input_cache_counts(container, "prompt_tokens_details")?;
        if read.is_some() || write.is_some() || container.get("prompt_cache_miss_tokens").is_some()
        {
            return Err(Status::data_loss(
                "provider cache usage is missing input/output totals",
            ));
        }
        return Ok(None);
    }
    let declared_total = object
        .get("total_tokens")
        .map(|_| token(object, "total_tokens"))
        .transpose()?;
    let mut usage = BillingUsage::new(
        token(object, "prompt_tokens")?,
        token(object, "completion_tokens")?,
        declared_total,
    )
    .map_err(Status::data_loss)?;
    let (read, write) = input_cache_counts(container, "prompt_tokens_details")?;
    usage.cache_read_tokens = read.unwrap_or(0);
    usage.cache_write_tokens = write.unwrap_or(0);
    usage.validate().map_err(Status::data_loss)?;
    check_cache_miss(container, &usage)?;
    Ok(Some(usage))
}

/// The standard `usage` and the Groq mirror `x_groq.usage` are both read; a disagreement is refused
/// instead of resolved (E2E-UPS-19).
#[allow(clippy::result_large_err)]
pub(crate) fn chat_frame_usage(value: &Value) -> Result<Option<BillingUsage>, Status> {
    let standard = chat_usage(value.get("usage"))?;
    let groq = chat_usage(value.get("x_groq").and_then(|groq| groq.get("usage")))?;
    match (standard, groq) {
        (Some(mut standard), Some(groq)) => {
            if (
                standard.input_tokens,
                standard.output_tokens,
                standard.total_tokens,
            ) != (groq.input_tokens, groq.output_tokens, groq.total_tokens)
            {
                return Err(Status::data_loss("provider usage totals disagree"));
            }
            let (read, write) = input_cache_counts(&value["usage"], "prompt_tokens_details")?;
            let (mirror_read, mirror_write) =
                input_cache_counts(&value["x_groq"]["usage"], "prompt_tokens_details")?;
            standard.cache_read_tokens = matching_count(read, mirror_read)?.unwrap_or(0);
            standard.cache_write_tokens = matching_count(write, mirror_write)?.unwrap_or(0);
            standard.validate().map_err(Status::data_loss)?;
            check_cache_miss(&value["usage"], &standard)?;
            check_cache_miss(&value["x_groq"]["usage"], &standard)?;
            Ok(Some(standard))
        }
        (Some(usage), None) | (None, Some(usage)) => Ok(Some(usage)),
        (None, None) => Ok(None),
    }
}

/// An OpenAI Responses `usage` record.
#[allow(clippy::result_large_err)]
fn responses_usage(container: Option<&Value>) -> Result<Option<BillingUsage>, Status> {
    let Some(container) = container.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let object = container
        .as_object()
        .ok_or_else(|| Status::data_loss("provider usage is not an object"))?;
    let declared_total = object
        .get("total_tokens")
        .map(|_| token(object, "total_tokens"))
        .transpose()?;
    let mut usage = BillingUsage::new(
        token(object, "input_tokens")?,
        token(object, "output_tokens")?,
        declared_total,
    )
    .map_err(Status::data_loss)?;
    let (read, write) = input_cache_counts(container, "input_tokens_details")?;
    usage.cache_read_tokens = read.unwrap_or(0);
    usage.cache_write_tokens = write.unwrap_or(0);
    usage.validate().map_err(Status::data_loss)?;
    check_cache_miss(container, &usage)?;
    Ok(Some(usage))
}

#[derive(Default)]
struct MessagesCounts {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
}

#[allow(clippy::result_large_err)]
fn messages_counts(usage: Option<&Value>) -> Result<MessagesCounts, Status> {
    let Some(usage) = usage.filter(|value| !value.is_null()) else {
        return Ok(MessagesCounts::default());
    };
    if !usage.is_object() {
        return Err(Status::data_loss("Anthropic usage is not an object"));
    }
    // Anthropic optional cumulative input/cache fields use null for "not restated".
    // Required initial input and final output are still enforced by their callers.
    let optional = |field| {
        if usage.get(field).is_some_and(Value::is_null) {
            Ok(None)
        } else {
            optional_count(usage, field)
        }
    };
    let creation = optional("cache_creation_input_tokens")?;
    let cache_write =
        if let Some(details) = usage.get("cache_creation").filter(|v| !v.is_null()) {
            let object = details
                .as_object()
                .ok_or_else(|| Status::data_loss("Anthropic cache_creation is not an object"))?;
            let mut total = None;
            for (field, value) in object {
                if !field.starts_with("ephemeral_") || !field.ends_with("_input_tokens") {
                    continue;
                }
                let count = value.as_u64().ok_or_else(|| {
                    Status::data_loss("Anthropic cache creation is not a token count")
                })?;
                total =
                    Some(total.unwrap_or(0_u64).checked_add(count).ok_or_else(|| {
                        Status::data_loss("Anthropic cache creation overflows u64")
                    })?);
            }
            matching_count(creation, total)?
        } else {
            creation
        };
    Ok(MessagesCounts {
        input: optional("input_tokens")?,
        output: optional_count(usage, "output_tokens")?,
        cache_read: optional("cache_read_input_tokens")?,
        cache_write,
    })
}

/// Anthropic reports uncached input separately; the record keeps total input with cache subsets,
/// like OpenAI `prompt_tokens`.
#[allow(clippy::result_large_err)]
fn messages_verdict(
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    saw_output: bool,
) -> Result<Option<BillingUsage>, Status> {
    if !saw_output {
        if output != 0 {
            return Err(Status::data_loss(
                "Anthropic usage reports output tokens without delivered output",
            ));
        }
        return Ok(None);
    }
    if output == 0 {
        return Err(Status::data_loss(
            "Anthropic output ended without positive output_tokens",
        ));
    }
    let input = input
        .checked_add(cache_read)
        .and_then(|count| count.checked_add(cache_write))
        .ok_or_else(|| Status::data_loss("Anthropic input usage overflows u64"))?;
    let mut usage = BillingUsage::new(input, output, None).map_err(Status::data_loss)?;
    usage.cache_read_tokens = cache_read;
    usage.cache_write_tokens = cache_write;
    usage.validate().map_err(Status::data_loss)?;
    Ok(Some(usage))
}

/// Anthropic usage is cumulative and untrusted until `message_stop`; it is held locally so a later
/// decrease or malformed event cannot leave a partially advanced bill. Every count may only grow.
#[derive(Default)]
pub(crate) struct MessagesMeter {
    started: bool,
    input: Option<u64>,
    output: Option<u64>,
    post_output: Option<u64>,
    cache_read: u64,
    cache_write: u64,
}

impl MessagesMeter {
    #[allow(clippy::result_large_err)]
    fn observe(&mut self, value: &Value, facts: &ResponseFacts) -> Result<(), Status> {
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "message_start" => {
                if self.started {
                    return Err(Status::data_loss(
                        "Anthropic message_start usage repeated within one stream",
                    ));
                }
                let counts = messages_counts(
                    value
                        .get("message")
                        .and_then(|message| message.get("usage")),
                )?;
                self.input = Some(counts.input.ok_or_else(|| {
                    Status::data_loss("Anthropic initial usage is missing input_tokens")
                })?);
                self.started = true;
                self.cache_read = counts.cache_read.unwrap_or(0);
                self.cache_write = counts.cache_write.unwrap_or(0);
                self.advance_output(counts.output.unwrap_or(0))?;
            }
            "message_delta" => {
                if !self.started {
                    return Err(Status::data_loss(
                        "Anthropic message_delta usage arrived before message_start usage",
                    ));
                }
                let counts = messages_counts(value.get("usage"))?;
                // A provider-run tool adds its results to the input, so the cumulative input and
                // cache counts may grow; the last stated value is the bill.
                if counts
                    .cache_read
                    .is_some_and(|count| count < self.cache_read)
                    || counts
                        .cache_write
                        .is_some_and(|count| count < self.cache_write)
                {
                    return Err(Status::data_loss(
                        "Anthropic cumulative cache usage decreased",
                    ));
                }
                if counts
                    .input
                    .is_some_and(|input| self.input.is_some_and(|initial| input < initial))
                {
                    return Err(Status::data_loss(
                        "Anthropic cumulative input_tokens decreased",
                    ));
                }
                self.input = counts.input.or(self.input);
                self.cache_read = counts.cache_read.unwrap_or(self.cache_read);
                self.cache_write = counts.cache_write.unwrap_or(self.cache_write);
                let output = counts.output.ok_or_else(|| {
                    Status::data_loss("Anthropic cumulative usage is missing output_tokens")
                })?;
                self.advance_output(output)?;
                self.post_output = Some(output);
            }
            _ if facts.has_output() => {
                if !self.started {
                    return Err(Status::data_loss(
                        "Anthropic content arrived before message_start usage",
                    ));
                }
                self.post_output = None;
            }
            _ => {}
        }
        Ok(())
    }

    #[allow(clippy::result_large_err)]
    fn advance_output(&mut self, output: u64) -> Result<(), Status> {
        if self.output.is_some_and(|previous| output < previous) {
            return Err(Status::data_loss(
                "Anthropic cumulative output_tokens decreased",
            ));
        }
        self.output = Some(output);
        Ok(())
    }

    #[allow(clippy::result_large_err)]
    fn finish(self, saw_output: bool) -> Result<Option<BillingUsage>, Status> {
        let input = self.input.ok_or_else(|| {
            Status::data_loss("Anthropic output ended without initial input_tokens")
        })?;
        let output = self.post_output.ok_or_else(|| {
            Status::data_loss("Anthropic output ended without final cumulative output_tokens")
        })?;
        messages_verdict(input, output, self.cache_read, self.cache_write, saw_output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn cache_fixture(format: ApiFormat, input: u64, read: u64, write: u64, output: u64) -> Value {
        let usage = match format {
            ApiFormat::ChatCompletions => serde_json::json!({
                "prompt_tokens":input+read+write,"completion_tokens":output,
                "prompt_tokens_details":{"cached_tokens":read,"cache_write_tokens":write},
                "completion_tokens_details":{"reasoning_tokens":output},
            }),
            ApiFormat::Responses => serde_json::json!({
                "input_tokens":input+read+write,"output_tokens":output,
                "input_tokens_details":{"cached_tokens":read,"cache_write_tokens":write},
                "output_tokens_details":{"reasoning_tokens":output},
            }),
            ApiFormat::Messages => serde_json::json!({
                "input_tokens":input,"output_tokens":output,
                "cache_read_input_tokens":read,"cache_creation_input_tokens":write,
                "cache_creation":{"ephemeral_5m_input_tokens":write/2,"ephemeral_1h_input_tokens":write-write/2},
                "server_tool_use":{"web_search_requests":1},
            }),
        };
        serde_json::json!({"usage":usage})
    }

    fn cached_body_and_stream(format: ApiFormat, fixture: Value) -> (BillingUsage, BillingUsage) {
        let body = UsageMeter::body(format, &fixture, &facts(true))
            .unwrap()
            .unwrap();
        let mut meter = UsageMeter::new(format);
        match format {
            ApiFormat::Messages => {
                meter
                    .observe(
                        &serde_json::json!({"type":"message_start","message":fixture}),
                        &facts(false),
                    )
                    .unwrap();
                meter
                    .observe(
                        &serde_json::json!({"type":"content_block_delta"}),
                        &facts(true),
                    )
                    .unwrap();
                meter
                    .observe(
                        &serde_json::json!({"type":"message_delta","usage":fixture["usage"]}),
                        &facts(false),
                    )
                    .unwrap();
            }
            ApiFormat::Responses => {
                let terminal = ResponseFacts {
                    done: true,
                    ..facts(false)
                };
                meter
                    .observe(&serde_json::json!({"response":fixture}), &terminal)
                    .unwrap();
            }
            ApiFormat::ChatCompletions => meter.observe(&fixture, &facts(false)).unwrap(),
        }
        (body, meter.finish(true).unwrap().unwrap())
    }

    #[test]
    fn cache_2234_unknown_cache_fields_do_not_change_known_billing() {
        for (format, mirror, details) in [
            (ApiFormat::ChatCompletions, false, "input_tokens_details"),
            (ApiFormat::Responses, false, "prompt_tokens_details"),
            (ApiFormat::Messages, false, "output_tokens_details"),
            (ApiFormat::ChatCompletions, true, "input_tokens_details"),
        ] {
            for (input, read, write) in [(1000, 0, 0), (300, 500, 200)] {
                let mut fixture = cache_fixture(format, input, read, write, 100);
                if read == 0 && write == 0 {
                    fixture["usage"]
                        .as_object_mut()
                        .unwrap()
                        .retain(|field, _| {
                            !field.contains("cache")
                                && field != "prompt_tokens_details"
                                && field != "input_tokens_details"
                        });
                }
                if mirror {
                    fixture["x_groq"] = serde_json::json!({"usage":fixture["usage"]});
                }
                let usage = if mirror {
                    &mut fixture["x_groq"]["usage"]
                } else {
                    &mut fixture["usage"]
                };
                usage[details] = serde_json::json!({"reasoning_tokens":70,"tool_tokens":20});
                let (body, stream) = cached_body_and_stream(format, fixture.clone());
                assert_eq!(body.billable_tokens(), input + 100);
                assert_eq!(stream, body);
                for unsupported in [
                    serde_json::json!({"cached_tokens":0,"cache_write_tokens":0}),
                    serde_json::json!({"cached_tokens":500}),
                    serde_json::json!({"provider":{"cached_tokens":500}}),
                    serde_json::json!([{"cached_tokens":500}]),
                    serde_json::json!({"cache_future": [null, -1, "unknown", {"cached_tokens":u64::MAX}]}),
                ] {
                    let mut extra = fixture.clone();
                    if mirror {
                        extra["x_groq"]["usage"][details] = unsupported;
                    } else {
                        extra["usage"][details] = unsupported;
                    }
                    let (extra_body, extra_stream) = cached_body_and_stream(format, extra);
                    assert_eq!(extra_body, body);
                    assert_eq!(extra_stream, body);
                }
            }
        }
    }

    #[test]
    fn cache_2234_anthropic_unknown_creation_breakdown_is_ignored() {
        for creation in [None, Some(200)] {
            for breakdown in [
                serde_json::json!({"future_cache_tokens":900}),
                serde_json::json!({"ephemeral_5m_input_tokens":100,"ephemeral_1h_input_tokens":100,"future_cache_tokens":{"cached_tokens":-1}}),
            ] {
                let mut fixture =
                    cache_fixture(ApiFormat::Messages, 300, 500, creation.unwrap_or(0), 100);
                if creation.is_none() {
                    fixture["usage"]
                        .as_object_mut()
                        .unwrap()
                        .remove("cache_creation_input_tokens");
                }
                let expected_write = if breakdown.get("ephemeral_5m_input_tokens").is_some() {
                    200
                } else {
                    creation.unwrap_or(0)
                };
                fixture["usage"]["cache_creation"] = breakdown;
                let (body, stream) = cached_body_and_stream(ApiFormat::Messages, fixture);
                assert_eq!(body, stream);
                assert_eq!(body.cache_write_tokens, expected_write);
                assert_eq!(body.billable_tokens(), 400);
            }
        }
    }

    #[test]
    fn cache_2223_review_anthropic_nullable_counters_preserve_initial_usage() {
        let stream = |initial: Value, final_usage: Value| -> Result<_, Box<Status>> {
            let mut meter = UsageMeter::new(ApiFormat::Messages);
            meter.observe(
                &serde_json::json!({"type":"message_start","message":{"usage":initial}}),
                &facts(false),
            )?;
            meter.observe(
                &serde_json::json!({"type":"content_block_delta"}),
                &facts(true),
            )?;
            meter.observe(
                &serde_json::json!({"type":"message_delta","usage":final_usage}),
                &facts(false),
            )?;
            meter.finish(true).map_err(Box::new)
        };
        let initial = serde_json::json!({"input_tokens":300,"output_tokens":0,
            "cache_read_input_tokens":500,"cache_creation_input_tokens":200});
        let omitted = serde_json::json!({"output_tokens":100});
        let nullable = serde_json::json!({"input_tokens":null,"cache_read_input_tokens":null,
            "cache_creation_input_tokens":null,"output_tokens":100});
        let expected = stream(initial.clone(), omitted.clone()).unwrap().unwrap();
        assert_eq!(expected.billable_tokens(), 400);
        assert_eq!(stream(initial.clone(), nullable).unwrap(), Some(expected));
        let body = serde_json::json!({"usage":{"input_tokens":300,"output_tokens":100,
            "cache_read_input_tokens":null,"cache_creation_input_tokens":null,"cache_creation":null,
            "output_tokens_details":{"reasoning_tokens":70},"server_tool_use":{"web_search_requests":1}}});
        assert_eq!(
            UsageMeter::body(ApiFormat::Messages, &body, &facts(true))
                .unwrap()
                .unwrap()
                .billable_tokens(),
            400
        );
        let mut missing_input = initial.clone();
        missing_input["input_tokens"] = Value::Null;
        assert!(stream(missing_input, omitted.clone()).is_err());
        assert!(stream(initial.clone(), serde_json::json!({"output_tokens":null})).is_err());
        for field in [
            "input_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ] {
            for bad in [
                serde_json::json!(-1),
                serde_json::json!(1.5),
                serde_json::json!("5"),
            ] {
                let mut final_usage = omitted.clone();
                final_usage[field] = bad.clone();
                assert!(stream(initial.clone(), final_usage).is_err());
                let mut malformed = body.clone();
                malformed["usage"][field] = bad;
                assert!(UsageMeter::body(ApiFormat::Messages, &malformed, &facts(true)).is_err());
            }
        }
        for field in ["input_tokens", "output_tokens"] {
            let mut required = body.clone();
            required["usage"][field] = Value::Null;
            assert!(UsageMeter::body(ApiFormat::Messages, &required, &facts(true)).is_err());
        }
    }

    #[test]
    fn cache_2223_all_formats_and_response_modes_exclude_read_and_creation() {
        for format in [
            ApiFormat::ChatCompletions,
            ApiFormat::Responses,
            ApiFormat::Messages,
        ] {
            for (input, read, write, output) in [
                (300, 0, 0, 100),
                (300, 500, 0, 100),
                (300, 0, 200, 100),
                (300, 500, 200, 100),
                (0, 500, 200, 100),
            ] {
                let (body, stream) = cached_body_and_stream(
                    format,
                    cache_fixture(format, input, read, write, output),
                );
                assert_eq!(body, stream);
                assert_eq!(body.billable_tokens(), input + output);
                assert_eq!(body.total_tokens, input + read + write + output);
            }
        }
    }

    #[test]
    fn cache_2223_aliases_and_creation_breakdowns_are_not_added_twice() {
        let chat = serde_json::json!({"usage":{
            "prompt_tokens":1000,"completion_tokens":100,
            "prompt_cache_hit_tokens":500,"prompt_cache_miss_tokens":500,
            "prompt_tokens_details":{"cached_tokens":500,"cache_read_tokens":500,
                "cache_write_tokens":200,"cache_creation_tokens":200},
            "completion_tokens_details":{"reasoning_tokens":70},
            "attribution":{"cached_tokens":500,"cache_write_tokens":200},
        }});
        assert_eq!(
            UsageMeter::body(ApiFormat::ChatCompletions, &chat, &facts(true))
                .unwrap()
                .unwrap()
                .billable_tokens(),
            400
        );
        let mut conflicting = chat.clone();
        conflicting["usage"]["prompt_tokens_details"]["cache_read_tokens"] = 499.into();
        assert!(UsageMeter::body(ApiFormat::ChatCompletions, &conflicting, &facts(true)).is_err());
        let mut mirror = chat.clone();
        mirror["x_groq"] = serde_json::json!({"usage":chat["usage"]});
        mirror["x_groq"]["usage"]["prompt_tokens_details"]["cache_write_tokens"] = 201.into();
        assert!(UsageMeter::body(ApiFormat::ChatCompletions, &mirror, &facts(true)).is_err());
        let partial = serde_json::json!({"usage":{"prompt_tokens_details":{"cached_tokens":500}},"x_groq":{"usage":{"prompt_tokens":1000,"completion_tokens":100}}});
        assert!(UsageMeter::body(ApiFormat::ChatCompletions, &partial, &facts(true)).is_err());
        let mut creation = cache_fixture(ApiFormat::Messages, 300, 500, 200, 100);
        creation["usage"]["cache_creation"]["ephemeral_1h_input_tokens"] = 101.into();
        assert!(UsageMeter::body(ApiFormat::Messages, &creation, &facts(true)).is_err());
    }

    #[test]
    fn cache_2223_malformed_impossible_and_overflowing_known_counts_bill_nothing() {
        for bad in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("5"),
            Value::Null,
            serde_json::json!(1001),
        ] {
            for format in [ApiFormat::ChatCompletions, ApiFormat::Responses] {
                let mut fixture = cache_fixture(format, 300, 500, 200, 100);
                let details = if format == ApiFormat::Responses {
                    "input_tokens_details"
                } else {
                    "prompt_tokens_details"
                };
                fixture["usage"][details]["cached_tokens"] = bad.clone();
                assert!(UsageMeter::body(format, &fixture, &facts(true)).is_err());
            }
        }
        for (format, usage) in [
            (
                ApiFormat::Messages,
                serde_json::json!({"input_tokens":u64::MAX,"output_tokens":1,"cache_read_input_tokens":1}),
            ),
            (
                ApiFormat::Messages,
                serde_json::json!({"input_tokens":1,"output_tokens":1,"cache_creation":{"ephemeral_5m_input_tokens":u64::MAX,"ephemeral_1h_input_tokens":1}}),
            ),
        ] {
            assert!(
                UsageMeter::body(format, &serde_json::json!({"usage":usage}), &facts(true))
                    .is_err()
            );
        }
    }

    proptest! {
        #[test]
        fn cache_2234_adding_unknown_cache_preserves_input_tools_reasoning_and_billable_work(
            input in 0_u64..1_000_000, output in 1_u64..1_000_000,
            read in 0_u64..1_000_000, write in 0_u64..1_000_000,
        ) {
            for format in [ApiFormat::ChatCompletions,ApiFormat::Responses,ApiFormat::Messages] {
                let (baseline, _) = cached_body_and_stream(format,cache_fixture(format,input,0,0,output));
                let (body, stream) = cached_body_and_stream(format,cache_fixture(format,input,read,write,output));
                prop_assert_eq!(body,stream);
                prop_assert_eq!(body.billable_tokens(),baseline.billable_tokens());
                prop_assert_eq!(body.billable_tokens(),input+output);
                let mut extra = cache_fixture(format,input,read,write,output);
                extra["usage"]["attribution"] = serde_json::json!({"cached_tokens":read,"items":[{"cache_write_tokens":write}]});
                if format != ApiFormat::Messages {
                    let details = if format == ApiFormat::Responses { "input_tokens_details" } else { "prompt_tokens_details" };
                    extra["usage"][details]["future_cache"] = serde_json::json!({"cached_tokens":write});
                }
                let (extra_body, extra_stream) = cached_body_and_stream(format,extra);
                prop_assert_eq!(extra_body,body);
                prop_assert_eq!(extra_stream,body);
                let mut forged = body;
                forged.charged_tokens = Some(input+output+1);
                prop_assert!(forged.validate().is_err());
            }
        }
    }

    #[derive(Clone, Debug)]
    enum Event {
        Output,
        Usage(u64, u64),
    }

    fn facts(output: bool) -> ResponseFacts {
        ResponseFacts {
            text: if output { "x".into() } else { String::new() },
            ..ResponseFacts::default()
        }
    }

    fn usage_value(input: u64, output: u64) -> Value {
        serde_json::json!({"choices": [], "usage": {"prompt_tokens": input, "completion_tokens": output}})
    }

    proptest! {
        /// Money moves only on one consistent provider record that follows delivered output; any
        /// contradiction, output after the bill, or a bill without output authorizes nothing.
        #[test]
        fn billing_follows_exactly_one_record_after_output(
            events in proptest::collection::vec(
                prop_oneof![
                    Just(Event::Output),
                    (0u64..4, 0u64..4).prop_map(|(i, o)| Event::Usage(i, o)),
                ],
                0..8,
            )
        ) {
            let mut meter = UsageMeter::new(ApiFormat::ChatCompletions);
            let mut saw_output = false;
            let mut recorded: Option<(u64, u64)> = None;
            let mut refused = false;
            for event in &events {
                let result = match event {
                    Event::Output => {
                        if recorded.is_some() {
                            refused = true;
                        }
                        saw_output = true;
                        meter.observe(&serde_json::json!({}), &facts(true))
                    }
                    Event::Usage(input, output) => {
                        if recorded.is_some_and(|known| known != (*input, *output)) {
                            refused = true;
                        }
                        recorded = Some((*input, *output));
                        meter.observe(&usage_value(*input, *output), &facts(false))
                    }
                };
                if result.is_err() {
                    break;
                }
            }
            let observed_refusal = refused;
            let verdict = if observed_refusal {
                None
            } else {
                Some(meter.finish(saw_output))
            };
            match (verdict, recorded) {
                (None, _) => {}
                (Some(Ok(Some(usage))), Some((input, output))) => {
                    prop_assert!(saw_output && output > 0);
                    prop_assert_eq!((usage.input_tokens, usage.output_tokens), (input, output));
                }
                (Some(Ok(None)), Some((input, output))) => {
                    prop_assert!(!saw_output && input + output == 0);
                }
                (Some(Err(_)), recorded) => {
                    let total_positive = recorded.is_some_and(|(i, o)| i + o > 0);
                    let output_positive = recorded.is_some_and(|(_, o)| o > 0);
                    prop_assert!(
                        recorded.is_none()
                            || (saw_output && !output_positive)
                            || (!saw_output && total_positive)
                    );
                }
                (Some(Ok(_)), None) => prop_assert!(false, "billed without a record"),
            }
        }
    }

    #[test]
    fn a_contradiction_is_refused_while_it_is_observed() {
        let mut meter = UsageMeter::new(ApiFormat::ChatCompletions);
        meter.observe(&serde_json::json!({}), &facts(true)).unwrap();
        meter.observe(&usage_value(1, 1), &facts(false)).unwrap();
        meter.observe(&usage_value(1, 1), &facts(false)).unwrap();
        assert!(meter.observe(&usage_value(1, 2), &facts(false)).is_err());
        assert!(meter.observe(&serde_json::json!({}), &facts(true)).is_err());
    }

    #[test]
    fn whole_bodies_bill_like_streams() {
        let chat = serde_json::json!({"usage": {"prompt_tokens": 2, "completion_tokens": 3, "total_tokens": 5}});
        assert_eq!(
            UsageMeter::body(ApiFormat::ChatCompletions, &chat, &facts(true))
                .unwrap()
                .unwrap()
                .total_tokens,
            5
        );
        assert!(UsageMeter::body(ApiFormat::ChatCompletions, &chat, &facts(false)).is_err());
        let declared_wrong = serde_json::json!({"usage": {"input_tokens": 2, "output_tokens": 3, "total_tokens": 6}});
        assert!(UsageMeter::body(ApiFormat::Responses, &declared_wrong, &facts(true)).is_err());
        let messages = serde_json::json!({"usage": {"input_tokens": 1, "cache_creation_input_tokens": 2, "output_tokens": 3}});
        let usage = UsageMeter::body(ApiFormat::Messages, &messages, &facts(true))
            .unwrap()
            .unwrap();
        assert_eq!((usage.input_tokens, usage.cache_write_tokens), (3, 2));
        let silent = serde_json::json!({"usage": {"input_tokens": 1, "output_tokens": 0}});
        assert!(
            UsageMeter::body(ApiFormat::Messages, &silent, &facts(false))
                .unwrap()
                .is_none()
        );
    }
}
