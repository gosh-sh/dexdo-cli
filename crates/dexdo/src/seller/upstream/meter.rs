//! Provider-native billing read from a forwarded response.

//! Money is authorized only by the provider's own terminal input + output record. Each format
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
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or_else(|| {
            Status::data_loss(format!("provider usage {field} is not a token count"))
        }),
    }
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
    if let Some(details) = object
        .get("prompt_tokens_details")
        .filter(|value| !value.is_null())
    {
        if !details.is_object() {
            return Err(Status::data_loss(
                "provider prompt_tokens_details is not an object",
            ));
        }
        usage.cache_read_tokens = optional_count(details, "cached_tokens")?.unwrap_or(0);
        usage.cache_write_tokens = optional_count(details, "cache_write_tokens")?.unwrap_or(0);
    }
    usage.validate().map_err(Status::data_loss)?;
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
            for (field, count, mirror) in [
                (
                    "cached_tokens",
                    &mut standard.cache_read_tokens,
                    groq.cache_read_tokens,
                ),
                (
                    "cache_write_tokens",
                    &mut standard.cache_write_tokens,
                    groq.cache_write_tokens,
                ),
            ] {
                let known = value["usage"]["prompt_tokens_details"][field]
                    .as_u64()
                    .is_some();
                let mirror_known = value["x_groq"]["usage"]["prompt_tokens_details"][field]
                    .as_u64()
                    .is_some();
                if known && mirror_known && *count != mirror {
                    return Err(Status::data_loss(format!(
                        "provider {field} usage totals disagree"
                    )));
                }
                if !known {
                    *count = mirror;
                }
            }
            standard.validate().map_err(Status::data_loss)?;
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
    if let Some(details) = object
        .get("input_tokens_details")
        .filter(|value| !value.is_null())
    {
        if !details.is_object() {
            return Err(Status::data_loss(
                "provider input_tokens_details is not an object",
            ));
        }
        usage.cache_read_tokens = optional_count(details, "cached_tokens")?.unwrap_or(0);
    }
    usage.validate().map_err(Status::data_loss)?;
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
    Ok(MessagesCounts {
        input: optional_count(usage, "input_tokens")?,
        output: optional_count(usage, "output_tokens")?,
        cache_read: optional_count(usage, "cache_read_input_tokens")?,
        cache_write: optional_count(usage, "cache_creation_input_tokens")?,
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
