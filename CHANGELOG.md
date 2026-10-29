## v0.11.0

### Breaking Changes

- `dexdo buyer --continuity-mode` now defaults to `off`: when its deal ends, the
  buyer places no new BUY and exits. The previous default `proactive` bought a
  replacement deal by itself, even with no request waiting. To keep a buyer that
  renews, pass `--continuity-mode proactive` (buy ahead) or
  `--continuity-mode on-demand` (buy on the next request). In `--json` mode the
  self-ended run reports `stopping` with `reason` `policy`; `continuity_mode` in
  `starting` can now be `off`.

### New / Improvements

- `dexdo buyer --wait-for-seller` now works with `--market` and `--token-contract`,
  and applies to the first buy and to every replacement. With it the BUY carries
  `AON` and waits in the model book; without it the BUY carries `AON|FOK`, fills
  from one seller at once or is refused, and the buyer exits. A waiting BUY names
  the model book, not one deal: with `--market` or `--token-contract` the first
  seller whose ask crosses it fills it, and that deal is served. The flag is still
  refused with `--resume` and `--mock-chain`.
- `--continuity-mode on-demand` and `proactive` now renew the same way for a buy by
  model, `--market` and `--token-contract`: the replacement is bought from the model
  book at the run's `--ticks` and `--max-price-per-tick`. Without `--wait-for-seller`
  a replacement that finds no seller ends the buyer instead of retrying. A clean
  close that destroys the old deal also triggers proactive replacement.

### Fixes

- Private-note deployment and recovery rebuild expired historical proofs on the
  next layer using the original paid voucher. Interrupted recovery keeps its
  selected layer, keys and payment evidence, and reports unavailable historical
  data without asking for another voucher payment.
- Stopping `dexdo buyer` (Ctrl-C or SIGTERM) now cancels its own BUY that is still
  waiting in the book, the first one or a replacement, before the process exits, and
  waits for the book to confirm it; the escrow returns to the note. The next start
  is no longer refused with "belongs to a different logical invocation". A stop
  before the endpoint is up exits with an error that names the cancelled order; in
  `--json` mode the cancel is reported on stderr as
  `buyer_order_cancelled order_id=<n> reason=stop`, with no new event.
  A delayed placement is reconciled before cancelling; an unconfirmed placement or
  cancellation exits with an error and preserves recovery state without another BUY.
  Pending purchases are stopped and reconciled before the API drains in-flight
  requests. If a fill wins the stop race, its checked attribution is saved for
  `--resume`; local expiry alone never substitutes for a confirmed escrow refund.
- Sellers no longer hang while waiting for usage after the buyer stops a deal.
  They recognize the settlement receipt after the deal contract is removed and
  follow their configured `after_deal_done` policy.
- `dexdo provision` retries temporary chain-read failures, including a gateway 502
  while loading the model registry, and temporary GraphQL errors in HTTP 200.
  Normal CLI errors name the failed step and any offer retirement, deployment or
  reserve request already attempted; an unconfirmed cost remains explicitly
  unconfirmed. Money submissions are not repeated by read retries.

## v0.10.0

### Breaking Changes

- `/v1/messages` on the buyer is no longer translated to OpenAI. The buyer forwards
  every request to the seller as sent -- path, body and headers, less the client's
  credentials -- and returns the provider's answer as produced. Messages needs a seller with an Anthropic provider;
  Chat Completions and Responses need an OpenAI-compatible seller. A request on a path
  the seller does not serve is refused with HTTP 400 `unsupported_api_path` before any
  money moves. A Messages client sends `anthropic-version` itself, as the Anthropic
  API requires; the seller no longer adds one. `/v1/messages` is always available and `--anthropic-compat` is removed.
- Every request field reaches the provider unchanged, including `reasoning_effort`,
  `reasoning`, `tools`, tool results and fields dexdo does not know. The buyer no
  longer checks reasoning settings before payment, and `capabilities.reasoning_disable`
  and `capabilities.reasoning_effort` in `models.json` are ignored. `model` must name
  the deal's model; a request without it is refused. Tools the provider runs itself,
  such as web search, are sold by the tokens the provider reports, their results
  included; the provider's per-call fee is not billed.
- Provider answers, errors included, reach the client with the provider's status and
  body. Provider response headers arrive prefixed `llm_provider-`; `retry-after`,
  `retry-after-ms`, `x-ratelimit-*` and `anthropic-ratelimit-*` also keep their own
  names. The buyer sets `Content-Type` itself. A provider 401 or 403 remains HTTP 502
  without the provider's body; a provider redirect, or a provider error whose text
  contains the seller's key, is HTTP 502 without it. Sellers no longer
  repeat a request after a provider error; the client decides whether to retry.
- The `session_id` and `conversation_id` headers each reach the provider as sent;
  `conversation_id` no longer stands in for a missing `session_id`. A streamed Chat
  Completions answer always carries the provider's usage chunk before `[DONE]`; the
  seller removes `stream_options.continuous_usage_stats` from the request.
- Buyer machine events move to `dexdo.buyer.event.v3`. `starting` and `endpoint_ready`
  no longer carry `anthropic_compat`, and `request_delivery` carries the field set the
  runtime contract lists for v3.
- Buyer and seller must both update. A buyer refuses requests to an older seller with
  HTTP 502 `seller_update_required` and keeps the deal open. A seller refuses an older
  buyer's requests: an older buyer's local API keeps its deal open, while an older
  one-shot `dexdo buyer` run settles its deal as a dead gateway.
- The buyer checks substitution by the `model` each provider answer names, against
  the deal model's known spellings; for a deal model named without its producer, any
  producer prefix in the answer is ignored. An answer naming another model ends the
  stream and closes the deal as a substitution; an answer naming no model passes.

### New / Improvements

- `"stream": false` requests reach the provider as non-streamed requests and return
  its single JSON answer; the seller waits up to three minutes for it.

### Fixes

- Sellers let deals spend their initially funded SHELL reserve without automatically
  refilling it from the seller note. An exhausted TokenContract reserve is reported
  with the failed operation and available receipt evidence, including seller bond
  funding and offer placement through the note. A failed or cancelled receipt lookup
  preserves the known submission or rejection and leaves its cause unproven.
  Recovery state is kept and settlement remains unconfirmed until the chain proves
  a terminal outcome.
- The buyer no longer slows down as its request history grows. Checking unresolved
  usage skips completed history, and reading usage reports no longer holds up
  request accounting.
- Large inference purchases no longer fail with no executable liquidity because a
  TokenContract native VMSHELL balance was compared with its SHELL ECC[2] reserve.
  Reserve checks account for the operation and already-paid setup charges; buyer
  stop, dispute and unopened-deal recovery do not require the original lifetime reserve.
- Requests to the buyer's local endpoint no longer wait for a chain read of the deal
  before they reach the seller. The read took about 1.5 s per request on shellnet, and
  on mainnet it held a buyer to one request every two seconds across all its clients.
  A request is no longer refused with `deal_state_unavailable` while the chain cannot
  be read.
- The buyer's local API checks the current deal in the background once a minute
  instead of once a second, reducing chain reads and contention for the network
  request limit. Probe acceptance, deal completion, and continuity decisions can
  take up to a minute to be observed. Transient chain-read failures still retry.
- A seller no longer bills the probe tick on top of the tokens delivered after it.
  The probe tick is now the minimum charge: tokens delivered before and after the
  probe count toward it, and beyond one tick the seller claims exactly the buyer's
  total usage. Before, a buyer paid for up to almost one tick of tokens never
  received whenever the probe was accepted before one tick was used. Sellers must
  update; a deal whose probe was accepted before the update finishes under the old
  rule. The buyer's pre-purchase line no longer
  prints `streaming_ticks_after_probe` and `streaming_tokens_after_probe`, and its
  note now reads `at least one tick is charged even if you use less`.

## v0.9.0

### Breaking Changes

- `/v1/usage/facts` now returns bounded pages: `limit` defaults to 100 and accepts
  1 through 1000. Follow `next_cursor` with the same time filters until it is null.
  Financial changes between pages return HTTP 400; discard the partial traversal
  and restart. Page exhaustion is independent of financial `complete`.
  Existing schema-3 and schema-4 usage databases migrate automatically to schema 5,
  preserving history and retaining deal prices; earlier binaries cannot reopen it.
- `/v1/usage` now uses `quota_limited` and reports the current purchased deal's
  `quota.limit`, `quota.used` and `quota.remaining` in SHELL, with `remaining` and
  `unit` also at the response root. Quota usage excludes previous deals and BUY fees;
  historical expense summaries keep them. Usage follows accepted billable tokens past
  the first tick. After STOP or closure, remaining quota is
  zero while usage retains consumed work, including observed external
  closure. Purchase metadata survives a failed SQLite write; an unavailable initial state
  or unavailable terms return `503 usage_unavailable` instead of a previous deal's quota.
  A confirmed funded but unopened candidate keeps the previous quota (or zero before the
  first purchase) readable without restarting; inference still refuses the unopened candidate.
  Keep the buyer's data directory across restarts to retain its quota and history.
- `/v1/usage` replaces `actual_cost` with `cost` and `overhead_cost`, both in SHELL.
  `cost` grows after every accepted model billing response at the deal's execution
  price, including verification work, without waiting for payment or settlement.
  `overhead_cost` includes separately proven fees, burns and unused paid capacity;
  unclassified financial amounts remain internal. Their sum is not settled cash spending.
  `/v1/usage/facts` keeps confirmed financial expenses and can lag live consumption;
  use `/v1/usage` for current cost and usable remaining quota.
  Token counters are integers, with zero for unreported
  values; `input_tokens` excludes separately reported cache tokens. Update integrations
  to read both expense fields and count each token category once.
- `/v1/usage` now returns only the documented Gateway fields; the `buyer` extension
  is removed. Use `/v1/usage/facts` for exact expenses and financial completeness.
- Chat Completions now rejects unsupported reasoning controls and effort values
  instead of silently ignoring them. Use the documented reasoning-disable option
  with a compatible seller, or omit reasoning settings to retain provider defaults.

### New / Improvements

- Buyers can request `minimal`, `low`, `medium`, `high`, `xhigh`, or `max` reasoning
  effort in Chat Completions and Responses when the seller declares the level in
  `capabilities.reasoning_effort` in `models.json`. Sellers choose either the direct
  `reasoning_effort` field or nested `reasoning.effort` profile. Both peers must
  update. Unsupported levels are rejected before inference; explicit `none` keeps
  its existing disable control, and requests without the setting keep provider defaults.
- `/v1/usage/facts` now includes required `cost` as an exact SHELL string for the
  identifiable consumption within each expense's `amount`. Deduplicate by network
  and source ID before summing confirmed consumption; `amount - cost` can include
  unclassified expenses and is not the live summary's `overhead_cost`. When merging
  repeated exports, preserve a confirmed nonzero cost over a zero observation and
  reject unequal fixed nonzero splits. Paid-probe facts include `paid_probe` with
  accepted tokens and tick size: validate its exact cost formula and keep the largest
  valid bound for the same payment, without adding it again. Refresh cached classifications
  from a new traversal after upgrading. Missing `cost` must not fall back to `amount`.
- The buyer retains local request usage in SQLite and serves `/v1/usage` with
  Gateway daily/model summaries and confirmed SHELL expenses. The separate
  `/v1/usage/facts` exports exact expenses with stable source IDs, network, chain
  time, model and completeness so consumers can deduplicate shared-note expenses
  across buyer instances. BUY fees recover their original model from its order book
  even without a filled deal. Known open-deal expenses remain visible during incomplete
  reconciliation; confirmed accruals retain their identities and are subtracted from the
  terminal fact to avoid double counting. Inclusive dates and precise ranges are supported.
- Sellers can opt in to OTLP/gRPC metrics for availability, upstream requests,
  offer recovery and deal lifecycle. Metrics are independent of log verbosity;
  disabled telemetry and collector failures do not interrupt inference. Set
  `OTEL_METRICS_EXPORTER=otlp` and point `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` at
  the collector (default `http://localhost:4317`); the README lists every
  variable and what every label value means. New metrics:
  | Metric | Type | Value | Collected at |
  |---|---|---|---|
  | `dexdo.seller.heartbeat` | observable gauge, f64, `s` | Unix time of the collection; no `model` label. | On every export (every `OTEL_METRIC_EXPORT_INTERVAL`, 15 s by default). If it stops advancing, the seller is down, hung, or cannot reach the collector. |
  | `dexdo.seller.health.checks` | counter, u64 | Completed readiness checks. Labels `model`, `component` (`gateway_task`, `advertised_gateway`, `upstream_authentication_and_model`), `outcome` (`pass`, `fail`, `timeout`). | On every readiness check: every 20 s for each watched deal, once per component. |
  | `dexdo.seller.health.duration` | histogram, f64, `s` | Time one readiness check took. Same labels as `health.checks`. Not recorded for `gateway_task`, which is a status flag read with no duration. | On every `advertised_gateway` and `upstream_authentication_and_model` check, every 20 s per watched deal. |
  | `dexdo.seller.health.last_check` | observable gauge, f64, `s` | Unix time of the oldest latest check across the watched deals of a `model` and `component`. | From the latest readiness checks, on every export. |
  | `dexdo.seller.health.last_success` | observable gauge, f64, `s` | Unix time of the oldest latest success across those deals; `0` while any of them has never passed. | From the latest readiness checks, on every export. |
  | `dexdo.seller.health.status` | observable gauge, u64 | `1` on the current `state` (`pass`, `fail`, `unknown`), `0` on the other two. `unknown` when a check of a watched deal is older than 120 s; `fail` when any latest outcome is not `pass`. Once the last watched deal of a model has ended, its last result stays. | From the latest readiness checks, on every export. |
  | `dexdo.seller.offer.executable` | observable gauge, f64, `1` | Seller asks seen live on the order book (deadline still ahead) within the last 120 s. `NaN` while any ask of the `model` is unknown. | From the seller's reads of its own asks on the order book, on every export. |
  | `dexdo.seller.offer.remaining_ticks` | observable gauge, f64, `1` | Ticks left in those live asks. `NaN` under the same condition. | From the seller's reads of its own asks on the order book, on every export. |
  | `dexdo.seller.offer.unknown` | observable gauge, f64, `1` | Asks with no fresh read: the last read failed or is older than 120 s. A matched ask is final and never counts here. | From the seller's reads of its own asks on the order book, on every export. |
  | `dexdo.seller.offer.recovery_age` | observable gauge, f64, `s` | Seconds since the oldest unresolved offer recovery started; `0` when none. | From the moment an offer post fails until a live ask is seen again, on every export. |
  | `dexdo.seller.offer.isolated` | observable gauge, f64, `1` | Local deal watchers isolated after an error. | When a deal watcher stops on an error or its listener is removed. |
  | `dexdo.seller.offer.events` | counter, u64 | Offer lifecycle transitions. Label `outcome`: `matched`, `expired`, `relisted`, `recovering`, `isolated`. | On each transition of a seller ask. |
  | `dexdo.seller.upstream.requests` | counter, u64 | Logical upstream requests, counted once at the end. Labels `model`, `purpose` (`inference`, `probe`), `backend` (`mock`, `openai`, `anthropic`), `outcome` (`success`, `error`, `cancelled`), `error_class` (`none`, `timeout`, `authentication`, `rate_limit`, `stream_protocol`, `unavailable`, `configuration`, `other`). | When an upstream call ends: buyer inference and readiness probes alike. |
  | `dexdo.seller.upstream.duration` | histogram, f64, `s` | Duration of one logical request. Same labels as `upstream.requests`. | When an upstream call ends. |
  | `dexdo.seller.upstream.attempts` | counter, u64 | HTTP attempts started, retries included. Labels `model`, `purpose`, `backend`, `status_class=started`. | On every HTTP attempt to the provider; the mock backend counts one per request. |
  | `dexdo.seller.upstream.responses` | counter, u64 | HTTP responses received. Labels `model`, `purpose`, `backend`, `status_class` (`2xx`, `4xx`, `5xx`, `other`), `status_code`. | On every HTTP response from the provider; the mock backend records none. |
  | `dexdo.seller.upstream.time_to_first_token` | histogram, f64, `s` | Time from request start to the first non-empty output chunk delivered to the buyer. Labels `model`, `purpose`, `backend`. | When the first output chunk reaches the buyer. |
  | `dexdo.seller.telemetry.export_failures` | observable counter, u64 | Failed exports to the collector, cumulative; visible after the next successful export. | On each failed export; visible after the next successful one. |
- Seller adapters now pass OpenAI-compatible cache-read/write counts and Anthropic cache-read
  and cache-creation counts through to the buyer's persistent usage summaries. Both peers
  must update to report cache usage; missing cache counts are returned as zero.
- Buyers can explicitly disable reasoning with `reasoning_effort: "none"` in Chat
  Completions or `reasoning: {"effort": "none"}` in Responses when the seller declares
  one of five verified controls: either chat-template switch, `reasoning_effort`,
  OpenRouter reasoning disable, or native thinking disable. This covers compatible
  OpenAI-shaped endpoints and supported native Anthropic models. OpenRouter disable
  requires parameter support during routing. Unsupported requests fail before inference
  without consuming deal tokens or closing the deal. Requests without the setting
  retain existing behavior, including with older sellers.
- A seller can serve concurrent inference requests on the same purchased deal. Set
  `dexdo seller --max-concurrent-requests-per-deal N` to choose a positive per-deal limit
  (default: 2; use 1 for sequential requests). Requests share the purchased budget. Once it
  is exhausted, new requests are refused and already admitted requests finish; buyers are
  never charged beyond the authorized capacity, and sellers cover excess provider work.
  Provider usage can therefore exceed the tokens paid from the deal. A request overrun
  also stops new admission before accepted work drains, even with paid capacity left.
  Complete shared-budget Messages answers report `end_turn`, including zero-charge
  answers; actual output truncation still reports `max_tokens`. Both participants
  must update to use shared-budget concurrency; older buyers retain exclusive reservations.
- Every `buyer.on.*` policy field now accepts `keep_open`. On that failure the buyer
  sends no STOP, reclaim, cleanup or dispute, keeps the local endpoint open and keeps
  running: the failed request gets an error and the next request reconnects to the same
  deal. A missing handover is awaited with no deadline, a malformed one is re-read
  until it is well-formed, and a verification failure refuses only the request that saw
  it, so the model-substitution check no longer protects the rest of the deal. Nothing
  reclaims the escrow on that failure; close the deal with `dexdo close` when you no
  longer want it. Accounting is unchanged. While it waits for a handover, a `keep_open`
  buyer checks the deal on chain once a minute; if the deal was closed meanwhile (for
  example cleaned up after the seller's open window), the buyer stops with an error
  naming the model, market and deal, and buys nothing new.

### Fixes

- Sellers now forward buyer API requests up to 32 MiB to the model provider
  instead of rejecting requests above 4 MiB at the gateway. Update the seller
  as well as the buyer; the provider's own request and context limits still apply.
- Buyer API requests now accept JSON bodies up to 32 MiB (33,554,432 bytes),
  including large prompts and conversation histories. Larger bodies return
  HTTP 413 with the size limit before opening a deal.
- Buyer `close` preserves its confirmed cleanup result if saving local usage fails,
  and reports how to retry local recovery instead of claiming nothing was sent.
- Standalone buyer `close` now closes the saved usage quota in SQLite and its
  recovery journal after confirmed closure, retaining consumed cost across
  restarts and repeated closes without affecting a replacement purchase.
- Buyer consumption cost grows and remaining quota decreases after each accepted
  billing response across all purchased ticks, including after restart. Later
  settlement does not count the same work again. Deal prices and usage history
  survive automatic database migration and replacement of the current purchase.
- Confirmed paid-probe consumption in `/v1/usage/facts` includes accepted work after probe
  acceptance within the already paid tick, including accepted work followed by an
  HTTP failure. Token claims beyond paid capacity do not create estimated expenses;
  confirmed spending and original financial identities remain unchanged.
- Sellers retry temporary provider failures without `Retry-After` using short,
  randomized delays with exponential backoff, avoiding synchronized 20-second
  pauses after HTTP 429 throttling. Provider-specified retry delays are still honored.
- Seller startup clears stale pending deal reservations after the network confirms
  the account exists or reports the specific missing-account response. Inconclusive
  reads are retried; if they still fail, startup stops and identifies the deal file
  and contract without changing pending reservations. Read retries honor server
  retry delays, and shutdown can interrupt this startup check.
- Network commands, including wallet onboarding, no longer exit silently with
  SIGPIPE when a peer closes its connection. Closing a pipe that receives CLI output
  still ends the command without a panic message.
- Sellers no longer refuse every later request on a deal after one shared-budget
  request is cut off mid-answer, even with paid capacity left. The interrupted
  request is still not billed; repeated interruptions can therefore produce unpaid
  provider work. Restarting the updated seller restores seller admission for deals
  already stuck this way. A buyer paused with `upstream_usage_unresolved` still
  requires reconciliation; this change does not automatically resume that buyer.
- Sellers keep serving existing deals when a new or residual offer fails its
  readiness checks. The rejected offer stays off the order book and retains
  its recovery state. Startup readiness also retries a temporary provider HTTP 429
  before refusing a new offer.
- Provider request errors no longer close a live buyer deal. Output interrupted
  without final usage pauses further requests with `503 upstream_usage_unresolved`
  and `read_only_reconciliation`, including paid verification and restart. The
  pause also survives accounting write failures and prevents automatic deal
  replacement. Reconcile the failed request and phase before resuming; incomplete
  accounting history needs repair, and restarting alone does not clear the pause.
  Requests whose usage history cannot be linked to the deal return
  `503 usage_unavailable` before paid work starts. Automatic verification and
  shutdown STOPs recheck the pause immediately before submission. Verification
  failure immediately blocks new paid requests while settlement is pending;
  waiting requests recheck before starting another paid phase. An accounting
  veto keeps the quota open and serving blocked for reconciliation.
  Update both peers. Recovery for an unreachable seller gateway is unchanged.
- Temporary provider throttling and exhausted retries before any output no longer
  close a buyer's deal. Retryable failures return `upstream_retryable` with
  `retry_with_backoff`; later requests can use the same deal. Sellers also keep
  their existing offer during provider HTTP 429 throttling. Update both peers.
- Cancelling one buyer API request no longer closes the whole local deal session.
  Later requests can use the same live deal; previously accepted usage is retained.
- Temporary chain-read failures no longer close an otherwise active buyer API
  session. The affected request returns HTTP 502 with `error.code` set to
  `deal_state_unavailable` and `error.recovery_action: "retry_with_backoff"`.
  Retry on the same deal after reads recover; no restart or new BUY is needed.
  Requests remain blocked until authoritative deal state can be confirmed.
- `note deploy` builds a voucher's proof from the exact payment event it proves. A block
  holding several note payments could previously yield a proof assembled from another
  payment, which RootPN rejects with error 137. Recovery of an already paid voucher whose
  saved event data is empty or unreadable now stops with an explicit error instead of
  building such a proof; keep the recovery file and do not fund a new voucher.
- Sellers keep supervising their existing offer when GraphQL temporarily cannot
  connect to the Block Manager. After reads recover, they resume the same offer,
  recover a buyer match, or renew an expired offer without posting duplicates.
- Provisioning retries temporary producer rate limits with bounded backoff and
  the original signed message. A buyer treats a manager-wrapped rate limit as an
  unknown money outcome and retains its recovery journal without resubmitting BUY.
- Sellers preserve an unconfirmed SELL submission across recovery attempts and
  restarts. Delayed placement is reconciled before another offer is sent; an empty
  market alone no longer permits a duplicate submission. Keep the seller's data
  directory when restarting so recovery can retain the original message identity.
  Renewing an expired offer also checks and replenishes the deal's existing gas
  reserve before posting, using the same reserve policy as other seller writes.
- Sellers recover paid purchases after restart even when their match events are
  on older history pages. Temporary history-server failures keep the paid deal
  available for recovery instead of abandoning it. Long history reads keep the
  seller pool responsive to active deals and shutdown.
- Purchase preparation reads history before signing, so a long history scan does
  not expire the first purchase message before it is sent.
- Vault-to-Hot funding retains confirmed request IDs and delivery receipts across
  restarts, so recovery can continue after the node prunes its history. Older
  journal evidence is reused only when it identifies the recorded request;
  ambiguous records remain protected against duplicate transfers and explain
  which confirmation is missing. Keep the original funding journal when upgrading.
- Closed buyer API sessions now return `error.code: "deal_session_closed"` with
  the deal address, request ID, first closure category, and known recovery state.
  Clients can distinguish pending or ambiguous recovery, confirmed terminal state,
  disputes, and planned preservation before choosing their next action. Reading
  this diagnostic does not submit a recovery transaction or create a new purchase.
- Buyer API refusals now include stable `error.code` and `error.recovery_action`
  fields across Chat Completions, Responses, and Messages. Integrations can distinguish
  unavailable orders, unopened deals, weekly or fixed-purchase exhaustion, verification
  budget refusals, pending purchases, and temporary per-deal concurrency limits without
  parsing English messages. Fixed purchases ask for explicit authorization of a new
  purchase; authorized on-demand and proactive services retain renewal guidance.
  A temporary trial-tick limit remains retryable when paid capacity awaits probe
  acceptance on the same deal. Update both peers to distinguish
  `deal_busy`; older sellers retain generic capacity-reconciliation guidance.
- The buyer's Chat Completions and Responses endpoints now forward the `session_id`
  header (or `conversation_id` when absent) and the `prompt_cache_key` body field to
  OpenAI-compatible providers. Reuse these values across steps of an agent session
  and use separate values for subagents to preserve prompt cache routing. Sellers
  log provider-reported `cached_tokens` when available.
- Sellers retry after temporary failures while renewing an expired SELL offer,
  including after safely cancelling a submitted offer whose acknowledgement was
  lost. Recovery checks existing orders before retrying to avoid duplicate offers.
- Sellers keep serving a paid deal when its SELL disappears before the purchase
  is visible to the match watcher. Delayed fill history is recovered before
  service and partial-fill remainder accounting, without reposting the paid deal.
- `note deploy` checks the proof's nominal and token type before submitting it.
  Error 137 now explains that either the inputs or ZK verification can have failed.
  Use `dexdo note recover --recovery <file>` to continue an already paid deploy,
  automatically rebuild a proof after a confirmed 137 rejection or expired history,
  and finish pool registration without another wallet payment. The separate
  `note deploy --reprove-paid-voucher` flag is removed. Keep the original recovery
  file; recovery still requires the original event's history to be available.

## v0.8.0

### Breaking Changes

- `dexdo markets` now lists the selected indexer's aggregate market rows and
  emits `dexdo.markets.v3`. Each JSON market contains exactly `frame_model`,
  `order_book`, `total_ask_ticks`, `best_ask`, and `tick_size`; the old v2
  chain-derived fields and the `--market`, `--models`, and `--note-addr` listing
  inputs are removed. The default request includes `liquidity=SELL`; pass
  `--all` to include markets without sell liquidity.

### Fixes

- `dexdo market`, `dexdo quote`, and buyer selection remain authoritative chain
  reads and do not depend on indexer availability. Expired asks are skipped like
  the order-book matcher skips them; an expired-only book reports ordinary no
  liquidity without stale-row details or ineffective retry/cleanup advice.
- `dexdo provision --replace-market <previous market.json>` and `server.sh reprovision --yes`
  retire previous unsold offers and their remaining capacity,
  while preserving recovery of already funded buyer deals. Stop the seller before replacing its
  market. Failed cancellation or a concurrent buyer match keeps the previous manifest available;
  restarting with the replacement market no longer reposts the old unsold offers.
  Relist logs distinguish a passed deadline from removal before the deadline or an unknown deadline.
- With `--json`, argument parse errors now name the missing or invalid options in
  `arguments` and explain the reason in `message`, including both options in a
  conflict. Use these fields to correct the command; `INVALID_ARGUMENT` and exit
  code 2 are unchanged. Rejected values are
  omitted from the diagnostic, including their components and rewritten forms.
- Sellers retain funded deals through Block Producer timeouts and follow bounded producer
  redirections. Match recovery saves the purchased and remaining capacity before opening,
  including matches discovered at offer expiry, so restarting resumes the same deal.
  PrivateNote and TokenContract requests use the system DApp id instead of an account-id fallback.
  Status and close guidance tell a seller with a funded unopened deal to resume its seller command.
- Sellers automatically restore a withdrawn offer when the upstream model becomes ready
  again after a failed readiness check, without requiring a manual restart.

## v0.7.0

### Fixes

- Sellers can renew an expired offer as soon as the order book and deal confirm its removal,
  even when the expiry request's response is delayed. Duplicate-message errors still require
  confirmation before another offer is posted; existing automatic recovery remains available.
- Sellers now print `seller_offer_relisted order_id=<new> previous_order_id=<old> ticks=<n>`
  after each confirmed expiry re-post, so redirected output records successor offers even without
  `RUST_LOG=info`.
- `dexdo monitor` now reads the current order books for the supplied `--market` files and shows
  the seller's live SELL offers instead of always reporting `offers in book: 0`. Repeated market
  files do not duplicate offers, and a failed book read reports an error instead of an empty book.
- Keep selling when a request finishes while the paid probe is being accepted. Delivery after the
  probe decision remains claimable, including after a restart. Seller failures now explicitly warn
  when a funded deal stops being served before settlement is confirmed.
- `dexdo seller` skips destroyed contract addresses when provisioning unsold capacity and
  automatically replaces an unused saved successor nonce that points to a destroyed contract.
  Restarting the seller no longer requires editing its cursor file to recover that capacity.
- `dexdo note deploy` now checks the exact receipt and waits up to two minutes for the expected
  note after an uncertain RootPN submission, preserving the message identity for restart recovery.
  `NOTE_DEPLOY_UNCONFIRMED` explains an unconfirmed result instead of reporting an internal error.
  Resume the original command with the same recovery file; previously paid vouchers are reused
  without another wallet payment, including recovery files created by older clients.
- Buyer submissions now preserve purchase recovery when the Block Manager reports a timeout
  forwarding to the Block Producer. The client checks the original purchase and retains its
  identity through handover instead of reporting an internal error or submitting another BUY.
  If confirmation is unavailable, keep the recovery files and re-run the same buyer command;
  `AMBIGUOUS_SUBMIT` forbids a fresh money submission while the previous result is unknown.

## v0.6.0

### Breaking Changes

- `dexdo seller` no longer accepts `--price-per-tick`; it always uses the terms stored in the
  deployed TokenContract. `dexdo buyer`, `dexdo provision`, and `dexdo executable-book` now require
  their price option explicitly instead of supplying an implicit price.

### Fixes

- Allow `dexdo buyer --resume` to reconnect to an already-priced deal without a new-buy ceiling.
  A price-less resume cannot place a fresh continuation BUY. Only a model-only resume with
  `--continuity-mode on-demand` can opt into later continuation by passing
  `--max-price-per-tick`, using exactly that ceiling; subscription, market, and explicit-contract
  resumes do not renew.
- `dexdo settlement-receipt --json` now explains the service charge, platform fee, forfeited bond,
  and total buyer cost for probe-only deals closed on contracts 4.0.36. Pass `--deals-dir` to also
  include a buyer client's recorded STOP reason when it matches the exact on-chain terminal message.
- Enforce `seller.max_open_deals` before provisioning spends money. Seller startup now warns when
  already deployed deals exceed the limit, continues serving every paid deal, and leaves residual
  provisioning queued until capacity is available.
- `dexdo note withdraw` now warns that withdrawal links the note to its destination wallet and
  permanently prevents future sell offers. Interactive use requires typing `YES`; non-interactive
  callers must pass `--acknowledge-note-retirement` after accepting the warning. Deploy a new note
  to continue selling.
- Return an explicit local error when text-only OpenAI or Anthropic endpoints receive reasoning
  without assistant text, while retaining the provider's clean terminal usage as billable and
  keeping the hidden reasoning out of visible output.
- Correlate local OpenAI and Anthropic responses with `request_delivery` through
  `x-dexdo-request-id`, and report the whole request's input, output, billable usage, HTTP status,
  and failure stage even when paid verification is followed by HTTP 502 or a client disconnect.
  Visible output excludes hidden reasoning; disconnect records preserve partial visible output and
  terminal usage observed before delayed EOF. Aggregated responses distinguish accepted buffered
  output from text that actually crossed the response-body boundary, and output-cap status follows
  provider usage rather than text visibility. Human output now shows the whole-request input/output
  split as well as its total.

## v0.5.0

### New / Improvements

- Added a guide for withdrawing all spendable PrivateNote funds to an explicitly selected wallet and collecting late ECC[2] refunds with `dexdo note sweep`.
- The buyer's local endpoint now accepts text-only OpenAI Responses requests at `POST /v1/responses`, with JSON and streaming replies charged to the same purchased deal. Tool calls are not supported and are rejected explicitly; `/v1/chat/completions` remains available. The buyer `request_delivery` event labels the new route `openai_responses`.

### Fixes

- The buyer's `/v1/chat/completions` endpoint now rejects requests with non-empty `tools` before contacting the seller or charging the deal; tool calls are not supported by the text-only buyer API.
- Added provider-native token usage to the local OpenAI-compatible API, including opt-in streaming usage, and made buyer request totals visible in human output and durable deal records so they remain readable after settlement destroys the TokenContract.
- Fixed `dexdo note withdraw` sending funds toward an account that does not exist or has no deployed code. The command now verifies the exact canonical account in its named DApp and refuses before submission when the destination cannot receive the withdrawal.
- `dexdo recover` now clears a note's stale deal record when its TokenContract no longer exists, waits for the note's 60-second touch interval, and confirms removal before reporting success. This may unblock `dexdo note withdraw` when no other locks remain; it does not restore escrow from the destroyed deal.

## v0.4.0

### Fixes

- Fixed `dexdo note deploy` so a new PrivateNote owner key is saved before wallet onboarding, chain checks, or wallet spending. PN pool files no longer reject or group notes by the multisig that funded them, and legacy funding provenance is removed on the next pool update.
- Fixed `dexdo note deploy` recovery after an already submitted deposit voucher: reruns now reconcile that voucher without requesting a Hot top-up or signing a second wallet transfer, and machine output reports `voucher_submitted_waiting_event_or_proof` while recovery is pending.
- Fixed reasoning-capable provider responses being rejected when provider billing usage exceeded the visible-output limit. The completed answer is now returned, charges cannot exceed the paid reservation, capacity exhaustion is reported to the buyer, and the seller closes the exhausted deal.
- Made `dexdo doctor` lead with a clear upgrade instruction when the installed manifest is older than the live chain, instead of leaving users to infer the remedy from contract hash failures.
- Fixed the seller process exiting when a completed deal's token contract had already been destroyed. A verified final settlement now retires only that deal while the seller keeps serving other offers; missing or conflicting settlement history still fails closed.
- Fixed slow or retried chain-time reads being counted as local clock skew, which could falsely block signed writes and make `dexdo doctor` recommend repairing an already-correct system clock.
- Prevented `--mock-model` from running with the real chain on production mainnet, so deterministic fake responses cannot be sold for mainnet SHELL. Mock-chain demos and the real-shellnet mock-model stage remain supported.
- Fixed buyer handover progress for matched deals that are funded but not yet opened: it now shows elapsed time, `funded=true opened=false`, and the reclaim countdown, prints the actionable `dexdo status '<TokenContract>'` command once, and emits bounded compact heartbeats in redirected output instead of appearing hung.
- Fixed settlement and recovery for deal addresses accidentally reused after a seller nonce was reset. Receipts from an earlier deployment no longer block or falsely complete the current deal, while duplicate settlement in the current deployment remains blocked. New nonce reuse is refused before deal deployment, seller offer posting, or buyer escrow. The required provisioning nonce now accepts `--nonce auto`, the recommended choice for a normal new market, while explicit numeric nonces remain available for reproducible operator workflows.

## v0.3.0

### New / Improvements

- The client now ships its user guides in `docs/`: a glossary of the market's terms, the registered-model page that shows how to export the on-chain name list and check one name against it, and a walkthrough of wallets, private notes and how funds move between them. They were not part of the published tree before.

### Fixes

- Fixed billing on provider-native streams, which counted only output tokens and so undercharged every request by the length of its prompt, leaving buyer and seller totals disagreeing about the same deal. Input and output usage are now accounted together against a separate billing grant, while the cap you set on model output stays output-only: an explicit output limit above the billing grant is refused rather than silently clamped. Sellers proxying Anthropic should expect a stricter stream: exactly one initial usage record is required before any content, a repeated or missing one ends the stream, and a malformed provider stream is no longer billable.
- Fixed a buy that could end after its money had already been posted. One connection reset while polling for the fill was turned into `ambiguous submit ... no resubmit is safe`, ending a purchase that was in fact still in flight. Those reads now go through the same retry policy as every other read on the money path; the surrounding wait bounds are unchanged, so an exhausted read still gives up as before.
- Fixed `dexdo doctor` running a label into its value when the label filled its column, which printed lines such as `generationmanifest 4.0.36, chain 4.0.36`. The separator is now part of the layout, so it is present at every label length, including the contract rows whose labels come from the manifest.
- Fixed OpenAI-compatible B7 sampling controls: readiness no longer sends reproducibility fields, and model profiles can set `capabilities.sample_algorithm` to `SEED`, `RANDOM_SEED`, `TOP_K`, or `NONE`. Omitted values default to `NONE` and degrade the reference comparison instead of rejecting honest sellers whose endpoint generates independently. The ineffective `DO_SAMPLE` value is no longer accepted; GLM profiles use `NONE`, and other non-`NONE` values require measured reproducibility on that exact endpoint.
- Fixed `dexdo market-data list` and `dexdo market-data show` output to use JSON fields `modelRefName` and `contractVersion` and table fields `model_ref` and `contract_version`; no flag or configuration changes are required.

## v0.2.1

### Breaking Changes

- A real-chain `dexdo seller` asks the on-chain ModelRegistry whether the name it is about to list exists, and refuses before it deploys an order book or posts an offer when the registry does not carry it. This runs on the default path: it no longer needs `--model-registry-validation`, which used to be the only way any catalog question was asked at all. Pass the new `--allow-unverified-model` to list a name the registry does not confirm, or to go on when the registry cannot be read; it is the same flag `provision` and `deploy-market` already carry. The flag does not cover a name the registry holds under a different spelling: that is refused either way, with the registered spelling named.
- The refusal on a secret file that users other than its owner can read now stands in front of every path that reads one. Newly covered: the note pool named by `--pool` or `DEXDO_PN_POOL`, which holds an owner key for every note in it; the `note deploy` recovery file; and the recovery phrase this client stores during wallet onboarding. The wallet secret file is now checked before it is classified rather than after, so a refusal no longer happens with the secret already in memory. On Unix-like systems any group or other permission bit refuses the command before anything is read; run `chmod 600` on the file the message names and repeat the command. Windows exposes no file mode and is unchanged.

### New / Improvements

- Real-chain commands work with nothing configured. With `DEXDO_MANIFEST` unset the client reads the sole per-user default, `$HOME/.dexdo/manifest.json` on Linux/macOS or `%USERPROFILE%\.dexdo\manifest.json` on Windows. Both installers copy the archived mainnet manifest there, verify it is a file, and replace it with a warning on every reinstall so updated pins cannot leave an old default behind. The variable still wins wherever it is set, and a path it names that does not exist is refused rather than falling back. The working directory, executable directory, XDG locations and directory scans are never consulted.
- `market`, `executable-book`, `quote`, `orders` and `subscription` resolve a model name against the on-chain ModelRegistry and no longer require a local `models.json`. Until now the catalog was the default authority and the chain was consulted only when `--model-registry-validation` switched it on, so a user with no catalog could not ask what a registered market was; `orders` had no registry path at all. Where a catalog exists it is still read, as a source of your own nicknames: a name it does not know is taken as the model itself, and where it maps a name elsewhere the registry decides. `markets` without `--market` still lists the books of the models in your own catalog -- the only one of these questions a local file can answer -- and its refusal now says that, names the file it wanted, and points at `markets address --model` for the single-model case that needs no file.
- `market` and `quote` say when nothing has been listed under a name instead of printing an empty table. The line goes to standard error, so `--json` output is unaffected, and it names the order-book address the ModelRegistry derives for that name so a misspelling is visible.
- `dexdo --version` names the build it came from: the package version, then the git short hash and commit date of the tree it was built from, or an explicit `(unknown)` for a build made outside a checkout. The line still starts with `dexdo <version>`, so installers and scripts that read the version off the front keep working. `-V` prints the short form.
- `dexdo doctor` now reports checks as they complete, separates skipped checks, ends with the overall verdict, and offers a stable `--json` health report for automation.
- The released archive carries `models.example.json` -- a filled-in shape with placeholders to copy and edit -- in place of the working `models.json` it used to ship, which named our own provider, model and key variable and stopped resolving when that provider retired the model. Nothing loads the example under that name; the catalog the client reads by default is still `models.json`, and it is yours to write.
- The seller's gateway TLS identity is kept in the operating system's secret store where the platform has one that holds a secret until something deletes it: Keychain on macOS, Credential Manager on Windows. Elsewhere, including a headless Linux server, it stays an owner-only file at exactly the path it has always used. An identity written by an earlier version is still found either way, so a restarted seller presents the certificate its buyers already pinned. `DEXDO_SECRET_STORE=system` or `=file` picks a branch deliberately. The permission check on that file now runs before the read, and accepts any owner-only mode instead of `0600` alone, so a stricter `0400` is no longer refused.

### Fixes

- Vault-to-Hot funding no longer makes every pending request block every later top-up. The client
  reuses a live request only when its native and currency amounts exactly match the current
  shortfall; a different shortfall creates another request. The owner-only funding journal now
  lists every live request with its UTC creation and expiry timestamps and removes unexecuted
  entries older than one hour when it creates a new request. A transfer that already left the Vault
  remains protected from an identical second submission until the Hot shows its credit, even after
  the original queue deadline; once credited, it no longer blocks a later request of the same size.
- Every command that reads the note pool finds the pool this client wrote. With no `--pool`, no `--data-dir` and no `DEXDO_PN_POOL` the readers stopped short of the platform data directory that `note deploy` writes to, so on a default install `dexdo note list` answered that the instance had deployed no notes while the pool sat on disk minutes after it was created, the picker that offers a note when `--note-addr` is omitted had nothing to offer, and pool-based recovery found no records. That location is now consulted last -- after the flag, the data directory and the variable, none of which change what they resolve to -- and only when the file is actually there, so an instance with no pool still says so, against the right path.
- `dexdo settlement-receipt` no longer reports every live deal as `inconsistent`. It read four deal fields the deployed contract has not declared since generation 4.0.31, so the decode failed on every real deal, the receipt recorded `current_getter_shape_invalid`, and both the `terminal` and `withdrawal` statuses came back `inconsistent` whatever the deal had actually done. The deal state now comes from the same strict decoder the rest of the client uses, pinned to the compiled ABI, and a shape refusal names the field that was missing instead of only saying that something was. In the receipt JSON the `current.state` object reports `probe_tick`, `tokens_final`, `tokens_pending`, `probe_time` and `last_claim_time` in place of the removed `prepaid`, `frozen`, `prepaid_time` and `last_advance`; the terminal-state check requires the two escrow earmarks to be zero and no longer demands that of the cumulative delivery counters, which a settled deal keeps by design.
- One transient server error no longer ends a command that has already spent. The exact-hash receipt read behind `note deploy` and the message reads behind the multisig delivery proof went straight to the endpoint, so a single `502` from the edge ended the run after the wallet spend had been submitted and left it to be reconciled by hand. Both now repeat like every other chain read in this client -- up to 5 attempts within 45 seconds, with backoff -- and a failure that is not transient still ends the command at once. These are queries: nothing is submitted, so nothing can be submitted twice.

## v0.2.0

### Breaking Changes

- CLI amounts and prices are now entered and printed in SHELL instead of raw ECC[2] units. Update scripts and configuration values using 1 SHELL = 1,000,000,000 raw units; prices remain whole SHELL.
- Real-chain selection now comes from the file named by `DEXDO_MANIFEST`. The `--contracts`, `--endpoint`, and `--network` selectors and the shellnet/test-giver Cargo features no longer select a deployment, so set `DEXDO_MANIFEST` for every real-chain command.
- The client now targets contract generation 4.0.36. A seller note deploys each deal and funds its ECC[2] reserve using `0.300 + 0.015 * max_ticks` SHELL. Notes from earlier generations cannot deploy new deals; mint a current note and provision new market handles before trading.
- Market identity is now the exact name held by the on-chain ModelRegistry. Update `frame_model` values in local model configuration to registered names; local aliases and provider slugs no longer choose an order book.
- Deployment manifests no longer contain `contract_hashes`. Contract generation comes from the manifest, while expected code hashes come from the client build; remove the old field from custom manifests.

### New / Improvements

- Interactive commands now ask only for decisions that are not already supplied, can select client-managed notes and keys, discover nearby `models.json` and market handles, and create policy files through a guided interview. The corresponding explicit inputs remain available for non-interactive use.
- `dexdo model-registry --output <path>` exports the on-chain model catalog as deterministic JSON. Use `--json` to write the same `dexdo.model_registry.v1` object to stdout.
- `dexdo markets address --model <name>` resolves a registered model and prints its canonical order-book address without requiring a live note. Use `--json` for the `dexdo.markets_address.v1` object.
- `dexdo note sweep [--note-addr <note>] [--note-key <key>] --to <address>` moves physical ECC[2] that reaches a note after its trading balance was withdrawn. Client-managed note details can come from the pool. The destination receives SHELL, not native vmshell.
- `dexdo oracle forfeit-stake ... --abandon-the-stake` releases a note from a stake that cannot pass `oracle cancel-stake`. The explicit flag is required because the stake is abandoned.
- `dexdo settlement-receipt <token-contract> --json --require-conserved` exits successfully only when the receipt proves conservation. Without `--require-conserved`, all receipt verdicts keep the previous zero exit status.
- Commands now accept the canonical `<dapp_id>::<account_id>` address form that dexdo itself prints.
- `dexdo note deploy` no longer creates a redundant second gas voucher. A new note uses the gas deposit it receives at creation, reducing wallet cost and deployment time.
- The chain client applies the manifest's `requests_per_second` ceiling across shared reads, and keeps outbound-message polling within the same limit.
- Public buyer and seller guides are now separated into human-run and agent-run workflows; install and seller-operations guides are published alongside them.

### Fixes

- Wallet onboarding reads its endpoint from `DEXDO_MANIFEST`. Gosh.ai onboarding is offered only when the manifest provides `goshai_onboarding_url`, and otherwise refuses before printing or storing invitation material.
- Wallet deployment and top-up payment codes name the target network. Deployment codes also request conversion with wallet flag 16 so transferred SHELL arrives as native vmshell.
- Seller startup skips stale local deal handles instead of refusing the entire deal list, and seller shutdown bounds its chain drain instead of waiting indefinitely.
- Model aliases can no longer redirect collateral to a different order book. Buyer admission accepts both registered identity spellings encountered during the 4.0.36 transition without weakening exact money-path comparison.
- Settlement receipts distinguish proven, unbalanced, incomplete, and unverified conservation instead of treating a one-sided reconstruction as proof.
- Chain and funding refusals retain the failing endpoint and actionable recovery text, and wallet read failures are no longer reported as insufficient balance.
- The public tree carries the mainnet manifest at `manifest/mainnet.manifest.json`; it no longer stages the shellnet manifest under `contracts/`.
- Withdrawn-note guidance points to executable recovery commands, including `note outstanding` and `note sweep`, rather than suggesting unavailable state or flags.

## v0.0.23

- fix(release): the release regression must carry every path the allow-list requires
- fix(release): two files that change the published binary were not being published
- fix(test): the last two platform rows -- a bound sized for the fast runner, and a stop Windows cannot make
- merge PR1435: an executed Vault transfer is credited by identity, not by a grown balance
- fix(windows): the shipped binary overflows the 1 MiB main-thread stack
- fix(test): isolate the data directory with the flag, not a Linux-only env var
- fix(test): the probe budget must outlast the platform refusal it waits for
- fix(ci): the Linux leg was killing the linker, not failing a test
- fix(ci): the campaign harness must run on the mac runner too
- fix(ci): the Windows leg, all three causes, measured on the runner
- merge PR1411: drop --contracts from the wallet commands, one endpoint seam, dd-shellnet
- chore(ci): throwaway windows loopback probe (delete after reading)
- fix(test): the gateway-check row holds its dead port the portable way too
- fix(test): the dead-address primitive must refuse on BSD too, not only on Linux
- fix(ci): the pinned-schema seam must not need the shellnet feature
- dev 92a2de8a. , :
- Promotes 717 commits of dev work. main held no unique non-merge commits -- its
- release: merge dev into main for v0.0.19
- Shellnet 4.0.29 client alignment, canonical multisig v2, recovered-buyer price safety, seller policy preflight, probe guidance, and release hardening.

## v0.0.22

- Fix wallet invitation output in terminals
- docs: report verification
- fix: switch to direct mainnet endpoint
- The live acceptance campaign passed 24/24 on 2ae2fff6 with set88. dev moved to
- test(live): bind settlement terminal by type
- test(campaign): run the two live onboarding proofs as campaign steps
- test: anchor lifecycle assertions at probe
- fix: preserve explicit onboarding paths
- fix: isolate Acki Nacki defaults per binding
- fix: keep PR1329 scoped to audit items 5 and 6
- fix: publish item 7's remediation where it is documented, and prove item 6 by what it produces
- fix: gosh-ai resumes, the menu has defaults, and the refusal keeps its contract
- 13f: PR1332 + PR1345 set85
- fix: chain liveness HTTP-
- integration 2026-08-14a: PR
- integration 2026-08-13d: five accepted PRs, live 22/22 on set84
- integration 2026-08-13c: four accepted PRs, live 22/22 on set82
- integration 2026-08-13b: the provider-answer fix, one address identity for the pool tools, and a verifier that refuses nothing
- integration 2026-08-13a: the wallet stack, the funding-wallet lock and the markdown scrub, proven live 22/22
- cut 12i: five fixes proven by a green live campaign (26/26 on set76)
- fix(ci): operator key material is structurally uncommittable
- fix(note wallet): stage one is measured deploy gas, not a nominal-sized burn
- test: wait for the gateway outcome, not for a wall-clock budget
- fix(note wallet): print the funding recipe note deploy actually requires
- test: the upstream stream shapes UPS-B3..UPS-B12, in mocks

## v0.0.21

- fix(test): let the live acceptance suite finish a full run
- fix: admission reserves the identity verification a fresh deal owes
- fix: preserve seller probe source chain
- fix: close seller liveness edge regressions
- fix(test): remove the shared-resource races that make CI non-deterministic (, )
- fix: use canonical advertise structured error
- fix: make installer PATH persistence rc-backed
- fix(ci): assert terminal 3/4/4 accounting
- fix(test): keep seller child output line-safe under libtest
- fix: refresh the subscription weekly allowance across boundaries
- fix(seller): reserve relay capacity before exposure
- fix: resolve an inherited :0 advertise to the bound gateway address
- fix: read canonical addresses in the release gate and live tests
- fix: let the real buyer book his own subscription week boundary
- fix: drive pool-only reclaim from recorded metadata alone
- fix: stop onboarding docs instructing a rejected --token-type
- merge: land structured CLI errors on top of /
- fix: urgent v0.0.21 pack -- installer PATH, canonical addresses, release version guard (, , )
- fix: validate public gateway advertise and tolerate tunneled self-probe (, )

## v0.0.20

- fix(tests): keep the models-fixture guard compiling without test-giver
- fix: do not count the probe seed as an unbacked delivery advance
- fix: declare model output caps in live fixtures ( follow-up)
- fix: clamp upstream max_tokens to the model output cap
- fix: emit seller_offer_outcome RESTED on every resting path
- fix: integrate complete v0.0.20 client for contracts 4.0.32
- contracts: integrate live shellnet 4.0.31 from PR713

## v0.0.19

- fix(release): omit internal agent docs from notes
- fix: require SHELL across dexdo market paths
- fix(seller): emit factual terminal and upstream events
- test: bootstrap SHELL-only no-giver release funding
- fix(buyer): show exact preflight and local handoff
- feat(seller): supervise partial-fill residual pool
- fix(note): re-prove the same paid voucher after exact 403
- docs: make lead own executor lifecycle
- docs: unblock exact bee wallet dependency for
- [v0.0.19][CI] Two-runner Windows seller Linux buyer shellnet E2E
- docs: replace onboarding with bee session v1
- fix: cancel unavailable seller offers
- fix: reject stale note deploy before wallet spend
- fix: resume interrupted Hermez SRS downloads
- fix: generate notes from current release baseline

## v0.0.18

- Shellnet 4.0.29 client alignment, canonical multisig v2, recovered-buyer price safety, seller policy preflight, probe guidance, and release hardening.

## v0.0.17

- Shellnet 4.0.29 client alignment, canonical multisig v2, recovered-buyer price safety, seller policy preflight, probe guidance, and release hardening.

