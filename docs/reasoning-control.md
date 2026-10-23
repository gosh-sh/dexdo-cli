# Control reasoning for text generation

The buyer's local API accepts explicit reasoning effort:

- Chat Completions: `"reasoning_effort": "none"`, `"minimal"`, `"low"`, `"medium"`,
  `"high"`, `"xhigh"`, or `"max"`.
- Responses: `"reasoning": {"effort": "none"}` with the same seven values.

Omitting the setting retains provider defaults and works with older sellers. Reasoning
budgets and provider-native controls are unsupported at the buyer API. Use the
documented API field, not `enable_thinking` or `chat_template_kwargs` at the buyer API.
This does not add a reasoning output field or change other model parameters.
The setting applies to the consumer's generation request; existing model-identity
probes retain their verification settings and accounting.

## Seller configuration

Set `capabilities.reasoning_disable` in the exact model's catalogue entry after
validating the served checkpoint, endpoint and server version. Five fixed profiles
translate the same buyer request:

| Profile | Outbound setting | Adapter |
| --- | --- | --- |
| `chat_template` | `chat_template_kwargs: {"enable_thinking": false}` | OpenAI-compatible |
| `chat_template_thinking` | `chat_template_kwargs: {"thinking": false}` | OpenAI-compatible |
| `reasoning_effort` | `reasoning_effort: "none"` | OpenAI-compatible |
| `openrouter` | `reasoning: {"enabled": false, "exclude": false}` and `provider: {"require_parameters": true}` | OpenAI-compatible |
| `thinking_disabled` | `thinking: {"type": "disabled"}` | OpenAI-compatible or native Anthropic |

An absent `reasoning_disable` or `"unsupported"` declares no support. The mock
adapter advertises no support; native Anthropic advertises it only for
`thinking_disabled`. Selecting a profile does not discover or validate the remote
model automatically. Unsupported models, mandatory-thinking models, unknown server
versions and incompatible adapter/profile combinations must not advertise support.

[SGLang](https://docs.sglang.io/docs/advanced_features/separate_reasoning) and
[vLLM](https://docs.vllm.ai/en/latest/features/reasoning_outputs/) use model-specific
template keys. The `reasoning_effort` spelling is documented by
[OpenAI](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create),
[Groq](https://console.groq.com/docs/reasoning),
[llama.cpp](https://github.com/ggml-org/llama.cpp/blob/master/tools/server/README.md),
[Ollama's OpenAI-compatible API](https://docs.ollama.com/api/openai-compatibility),
and [Gemini's compatibility layer](https://ai.google.dev/gemini-api/docs/openai?hl=en),
with model/version restrictions. [DeepSeek](https://api-docs.deepseek.com/api/create-chat-completion/)
also documents `thinking.type=disabled`.
[Anthropic](https://platform.claude.com/docs/en/build-with-claude/thinking-troubleshooting)
accepts that native switch only on permitted models; `between_tools` is a different
mode and is not substituted for full disable.

The [OpenRouter control](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens)
also requires a model with optional reasoning. Its
[routing guard](https://openrouter.ai/docs/guides/routing/provider-selection)
prevents providers that do not support request parameters from receiving the
explicit disable request. Ordinary requests keep their previous routing.
Hiding reasoning with `exclude`, `include_reasoning`, or a display setting does not
prove that reasoning generation is off.

## Example: a seller with SGLang and a compatible Qwen model

1. Check that this exact Qwen checkpoint supports the `enable_thinking=false`
   template switch on your SGLang deployment. A thinking-only checkpoint must not
   declare this profile.
2. Find the model entry in the catalogue the seller actually loads. `--models`
   selects an explicit path; otherwise it is `models.json` in the working directory,
   or `<data-dir>/models.json` when a data directory is selected.
3. Add `"reasoning_disable": "chat_template"` inside that entry's existing
   `capabilities`. For example, this fragment is part of the model entry:

```json
{
  "capabilities": {
    "max_output_tokens": 16384,
    "sample_algorithm": "NONE",
    "reasoning_disable": "chat_template"
  }
}
```

Keep the existing output cap and sampling profile for your endpoint. The example's
cap is illustrative; this declaration never changes request limits or model identity.
4. Restart the seller using its existing launch command and the edited catalogue.
   Use `--models /path/to/models.json` to select it explicitly. There is no separate
   reasoning-enable flag; the seller advertises support automatically from this
   model profile.
5. The buyer can then send a Chat Completions request such as:

```json
{
  "messages": [{"role": "user", "content": "Give a short answer: 2 + 2?"}],
  "reasoning_effort": "none"
}
```

For this request, the seller sends its configured model to SGLang with
`"chat_template_kwargs": {"enable_thinking": false}`.
The declaration applies to the exact endpoint/model, not every Qwen checkpoint.

The [official Qwen3.8-27B-FP8 model card](https://huggingface.co/Qwen/Qwen3.8-27B-FP8/blob/177354e00220d4fd6826574006e4f49a88df1796/README.md#instruct-or-non-thinking-mode)
documents the switch. [SGLang's Qwen3.8-27B guide](https://docs.sglang.io/cookbook/autoregressive/Qwen/Qwen3.8-27B)
confirms per-request thinking control. This is source evidence, not validation of
any particular deployed seller. Its checkpoint, server version, parser, effective
setting, answer and native usage must be checked before declaring support.

## Seller effort levels

The existing `capabilities.reasoning_disable` continues to handle `none`.

The effort levels are `minimal`, `low`, `medium`, `high`, `xhigh`, and `max`, matching
the [OpenAI reasoning effort values](https://developers.openai.com/api/docs/guides/reasoning#reasoning-effort)
apart from the separate disable control. Support is model-dependent; a seller must
advertise only its endpoint's verified subset.

Declare supported effort levels separately for the exact endpoint, model and server version:

```json
{
  "capabilities": {
    "reasoning_disable": "chat_template",
    "reasoning_effort": {
      "control": "reasoning_effort",
      "supported": ["minimal", "low", "medium", "high", "xhigh", "max"]
    }
  }
}
```

The fixed effort controls are:

| Control | Outbound setting for client `high` |
| --- | --- |
| `reasoning_effort` | `{"reasoning_effort": "high"}` |
| `reasoning_effort_nested` | `{"reasoning": {"effort": "high"}}` |

Only list levels actually supported by the served endpoint. A subset such as
`["medium", "high"]` is valid. Empty lists, duplicates, unknown levels and unknown
controls are invalid configuration. `none` belongs to `reasoning_disable`, not to
this list. An absent effort block declares no effort support. The effort profiles
apply only to the OpenAI-compatible seller adapter; native Anthropic and mock
adapters declare no effort support.

Both capabilities may be configured together. For the example above, client `none`
uses only `chat_template_kwargs.enable_thinking=false`; client `high` uses only
`reasoning_effort="high"`. Omission sends neither control. An on/off switch cannot
stand in for an effort level. No level is silently changed or converted into a
reasoning-token budget. These declarations require operator validation; selecting
a profile does not prove that an endpoint implements it.

Both peers must update to use effort levels. Older sellers advertise no supported
levels, so an explicit effort is rejected before billable identity probes or
inference. Sellers independently check the canonical request before admission;
unknown levels and simultaneous effort/disable controls are rejected.

## Compatibility and payment

New sellers advertise this capability in the existing challenge response. Older
buyers ignore the additional field. New buyers treat a missing declaration from
an older seller as unsupported only when the consumer explicitly requests that control.
The preliminary capability query issues no authorization nonce on new sellers;
ordinary stream authorization keeps its existing challenge-response flow.

An unsupported seller/model combination returns HTTP 400 with
`error.code: "unsupported_parameter"`, before admission, billable identity probes,
or provider inference. The deal remains usable. The CLI does not silently retry
without the setting, stop the deal, choose a different seller, or buy a replacement.

Capability negotiation occurs after a deal and its handover exist. It does not
make an already-purchased incompatible deal refundable for free. The buyer chooses
whether to continue without the setting or close under the existing settlement
rules and make a separate purchase.

Reasoning-only output retains the existing error and billing behavior. A clean
provider completion with no assistant text fails locally (HTTP 502 for a
non-streaming request; `empty_output` / `response.failed` for streaming), while its
validated terminal input and output usage can remain billable. Disabling reasoning
does not guarantee a particular answer length or change the output/billing caps.
