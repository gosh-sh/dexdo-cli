# Buyer API refusal codes

Chat Completions, Responses and Messages expose stable `error.code` and
`error.recovery_action` for the pre-answer refusals below. `error.type` remains
`invalid_request_error`; Messages also keeps its top-level `type: "error"`.
`error.message` is a human diagnostic, not a machine discriminator. A streaming
request refused before its answer starts receives the same JSON error body.

| Code | Meaning | Recovery action |
| --- | --- | --- |
| `order_unavailable` | The selected ask changed or is no longer executable before payment. | `retry`: repeat selection. |
| `deal_not_opened` | The seller did not open the matched deal or provide its handover. | `read_only_reconciliation`: inspect the existing purchase before rematching. |
| `purchase_reconciliation_required` | A purchase has an outstanding or ambiguous submission. | `read_only_reconciliation`: reconcile the existing submission; never issue a new BUY on this response. |
| `deal_budget_exhausted` | The fixed purchase has no usable capacity. | `new_purchase`: obtain explicit purchase authorization; repeating the same request cannot restore capacity. |
| `renewal_pending` | An exhausted ordinary route belongs to a service authorized for fresh continuation, in either on-demand or proactive mode. | `retry_with_backoff`: wait for the configured service to publish its next route. |
| `subscription_budget_exhausted` | This subscription's current weekly quota is spent. | `wait_for_next_cycle`: wait for the next authoritative weekly allowance. |
| `subscription_state_unavailable` | Weekly reconciliation cannot establish usable quota. | `read_only_reconciliation`: inspect authoritative state; do not assume exhaustion or authorize a purchase. |
| `subscription_terminal` | The subscription is stopped, disputed, or its full term ended. | `operator_action`: no further quota will open on this term; inspect dispute/terminal facts before requesting a new purchase. |
| `verification_budget_insufficient` | The remainder cannot cover the identity verification still owed and an answer. | `operator_action`: inspect the verification debt and the serving mode before replenishing capacity. |
| `verification_grant_consumed` | Completed identity verification used the request's whole grant. No answer request was sent. | `retry`: the deal is now verified; a later request can still be refused if the shared budget is spent. |
| `deal_busy` | The seller's per-deal concurrent request limit is occupied. | `retry_with_backoff`: retry after an active request finishes. |
| `deal_capacity_unavailable` | Paid remainder is temporarily behind the pre-acceptance trial ceiling, or the seller refused capacity without a specific busy indication. | `retry_after_reconciliation`: wait for authoritative probe acceptance or seller capacity reconciliation on this deal. |
| `upstream_retryable` | The provider failed temporarily before any output: its stream or body ended before the answer, or the connection dropped. The deal remains open. | `retry_with_backoff`: retry this request on the same deal. |
| `unsupported_api_path` | The seller's provider does not serve this path: an OpenAI-compatible seller serves Chat Completions and Responses, an Anthropic seller serves Messages. No money moved and the deal remains open. | `change_request`: send the request on a path the seller serves. |
| `seller_update_required` | The seller runs a version that does not forward requests as sent. No money moved and the deal remains open. | `wait_for_seller_update`: retry after the seller updates. |
| `invalid_buy_ticks` | The requested purchase is below the stream minimum. | `change_request`: correct the purchase parameters. |
| `deal_unavailable` | Deal initialization or serving failed without one of the specific causes above. | `operator_action`: inspect the diagnostic before deciding whether to retry. |

Busy and seller capacity refusals use HTTP 429. `unsupported_api_path` uses HTTP 400 and
`seller_update_required` HTTP 502. Buyer admission refusals, including
the temporary trial ceiling, and initialization refusals use HTTP 503.
Invalid purchase ticks retain HTTP 400 on the OpenAI
endpoints and HTTP 503 on Messages. Machine causes are identical across all
three endpoints; existing HTTP status policies are preserved.

A request does not read the deal from the chain before it reaches the seller. The
buyer's background observer reads immediately, then pauses for 60 seconds after
each read; read time and retries add to the interval. Once a read finds the deal
ended or not open, requests are refused with HTTP 410 `deal_session_closed`. A
request that reaches a seller no longer serving the deal fails at the seller and
follows the configured dead-gateway policy.

Spending the trial tick does not imply that the purchase or current week is spent.
When usable paid capacity remains beyond that ceiling, acceptance can admit another
request on the same deal and week. If the whole weekly quota is one tick and it is
spent, `subscription_budget_exhausted` still requires waiting for the next cycle.
Renewal guidance reflects existing purchase authorization, not lazy initialization
or route-replacement support alone.

Retries must be bounded and respect caller cancellation/deadlines. For
`retry_with_backoff`, use the integration's backoff policy and avoid immediate
retry loops; no requests-per-minute limit or fixed wait duration is implied.
Neither refusal rendering nor its recovery guidance submits a BUY. The existing
configured lifecycle/recovery policy continues to govern monetary operations.

The seller carries the typed busy cause as additive `dexdo-error-code: deal_busy`
gRPC metadata. Both peers must update for that distinction. A seller without this
metadata yields `deal_capacity_unavailable`; buyers never infer busy from prose.

A provider's own error answer -- for example HTTP 429 with `retry-after`, or a 5xx -- reaches the
caller with the provider's status and body. Every provider header arrives prefixed
`llm_provider-`; `retry-after`, `retry-after-ms`, `x-ratelimit-*` and `anthropic-ratelimit-*` also
arrive under their own names, so a client retries by them as it would against the provider. The
answer carries no dexdo code, moves no money and leaves the deal open; the seller does not repeat
the request. The exception is HTTP 401 and 403, a refused seller key: the caller receives HTTP 502
without the provider's body. A provider redirect (3xx) is likewise HTTP 502, without `Location`, and
so is any provider error whose text contains the seller's key: such an answer never leaves the seller.

Temporary provider failures that produced no answer carry `dexdo-error-code: upstream_retryable`
gRPC metadata while retaining their original gRPC status and diagnostic. Before any output, buyers
return HTTP 502 with this code and preserve the deal. If the SSE response has already opened, the
same code and recovery action appear in an error event; the request finishes as an error. Both
peers must update. Untyped transport failures and failures after accepted output retain their
configured recovery policy.

Unknown codes must be handled conservatively, without automatic new purchases.
HTTP 410 reports a closed serving session. These pre-answer codes do not replace
its lifecycle diagnostics or authorize reopening that session.
