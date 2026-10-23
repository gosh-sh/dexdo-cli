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
| `upstream_retryable` | The provider throttled the request or exhausted transient retries before any output. The deal remains open. | `retry_with_backoff`: retry this request on the same deal. |
| `invalid_buy_ticks` | The requested purchase is below the stream minimum. | `change_request`: correct the purchase parameters. |
| `deal_state_unavailable` | A temporary chain-read failure prevented confirmation of the deal's open/accounting state. The local session stays available. | `retry_with_backoff`: retry on the same deal; each request must obtain fresh authoritative state before contacting the seller. |
| `deal_unavailable` | Deal initialization or an authoritative serving-state read failed without one of the specific causes above. | `operator_action`: inspect the diagnostic before deciding whether to retry. |

Busy and seller capacity refusals use HTTP 429. Buyer admission refusals, including
the temporary trial ceiling, and initialization refusals use HTTP 503.
Invalid purchase ticks retain HTTP 400 on the OpenAI
endpoints and HTTP 503 on Messages. An authoritative serving check that finds
a funded but unopened deal uses 502. Machine causes are identical across all
three endpoints; existing HTTP status policies are preserved.

`deal_state_unavailable` also uses HTTP 502, including for streaming requests refused
before the answer starts. It does not submit a chain write or authorize a new BUY.
After reads recover, the next request rechecks state and existing accounting gates
on the same session. Contradictory, missing, terminal or unsafe state keeps its
existing fail-closed behavior; this retry does not reopen a separately closed session.

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

Transient provider failures carry `dexdo-error-code: upstream_retryable` gRPC
metadata while retaining their original gRPC status and diagnostic. Before any
output, buyers return HTTP 502 with this code and preserve the deal. If the SSE
response has already opened, the same code and recovery action appear in an error
event; the request finishes as an error. Both peers must update. Untyped transport
failures and failures after accepted output retain their configured recovery policy.

Unknown codes must be handled conservatively, without automatic new purchases.
HTTP 410 reports a closed serving session. These pre-answer codes do not replace
its lifecycle diagnostics or authorize reopening that session.
