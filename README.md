# dexdo

`dexdo` is a command-line client for the private inference market on Acki Nacki.
Sellers offer large-language-model inference for sale; buyers purchase it by the
tick. Deals settle on chain with escrow, private notes, and end-to-end encrypted
streaming, so neither side has to trust the other to be paid or served fairly.

## Install

### One-line install

**Linux / macOS**

```sh
curl -fsSL https://get.dex.do/install.sh | sh
```

**Windows (PowerShell)**

```powershell
irm https://get.dex.do/install.ps1 | iex
```

The installer detects your operating system and CPU architecture, downloads the
matching release archive, verifies its checksum, and installs `dexdo` into
`~/.local/bin` (Linux/macOS) or `%LOCALAPPDATA%\dexdo\bin` (Windows). Override
the binary directory with `DEXDO_BIN_DIR`. It also installs the archived mainnet
manifest as `<home>/.dexdo/manifest.json`; reinstalling replaces that release
artifact with a warning.

### PATH setup

The installer then puts that directory on your PATH so `dexdo` works in new
terminals. On Linux/macOS it appends one line, marked
`# added by dexdo installer`, to the config of the shell in `$SHELL`:

| Shell | File | Line |
|-------|------|------|
| zsh | `~/.zshrc` | `export PATH="$HOME/.local/bin:$PATH"` |
| bash (Linux) | `~/.bashrc` | `export PATH="$HOME/.local/bin:$PATH"` |
| bash (macOS) | `~/.bash_profile` | `export PATH="$HOME/.local/bin:$PATH"` |
| fish | `~/.config/fish/config.fish` | `fish_add_path "$HOME/.local/bin"` |

It prints the file it changed and the line it added, never writes outside
`$HOME`, never needs `sudo`, and re-running it does not duplicate the entry. Any
other shell is left untouched with a copy-paste instruction instead. A running
shell keeps its old PATH, so `source` the file or open a new terminal.

To skip PATH setup entirely and just get the instruction:

```sh
curl -fsSL https://get.dex.do/install.sh | DEXDO_NO_MODIFY_PATH=1 sh
# or, with an argument:
curl -fsSL https://get.dex.do/install.sh | sh -s -- --no-modify-path
```

### Manual download

Download the archive for your platform from the
[latest release](https://github.com/gosh-sh/dexdo-cli/releases/latest), verify it
against `SHA256SUMS`, extract it, move `dexdo` onto your PATH, and copy the
archived `manifest/mainnet.manifest.json` to `<home>/.dexdo/manifest.json`.

| Platform | Archive |
|----------|---------|
| Linux x86_64 | `dexdo-<version>-x86_64-linux.tar.gz` |
| Linux ARM64 | `dexdo-<version>-aarch64-linux.tar.gz` |
| macOS (Apple Silicon) | `dexdo-<version>-aarch64-macos.tar.gz` |
| macOS (Intel) | `dexdo-<version>-x86_64-macos.tar.gz` |
| Windows x86_64 | `dexdo-<version>-x86_64-windows.zip` |

### Build from source

```sh
cargo build --release -p dexdo
```

The release binary is written to `target/release/dexdo`.

## Addresses

Blockchain addresses are shown and stored in the canonical Acki Nacki form:

```text
<dapp_id>::<account_id>
```

Both halves are 64 hex characters. Every address dexdo prints, and every address
it writes into `market.json` or a deal handle, uses that form.

Wherever dexdo takes an address (`--token-contract`, `--note-addr`,
`--multisig-address`, `--to`, a positional address, ...) it accepts either the
canonical form or the older `0:<account_id>` form, so an address copied from an
earlier run still works. Files written by an earlier version keep loading
unchanged and are rewritten canonically the next time dexdo saves them.

The single exception is `dexdo note withdraw --to`, which requires the
DApp-qualified form: the destination DApp is evidence in `TokensWithdrawn`, and
account-only input cannot supply it.

Withdrawal links the note to that wallet, deanonymizes it, and permanently
prevents the note from posting another sell offer. The interactive command asks
for an explicit `YES`; a non-interactive caller must pass
`--acknowledge-note-retirement` after accepting those consequences. Continuing
to sell requires a new note.

## Commands

| Command | What it does |
|---------|--------------|
| `doctor` | Read-only network version / pin and market-freshness checks. Alias: `health`. |
| `provision` | Bring up an order book, model root, and per-deal token contract for a market. |
| `note deploy` | Mint a wallet-funded private note and fold it into the local note pool. |
| `seller` | Seller client: gateway, authorization, and stream handover. |
| `buyer` | Buyer client: endpoint decryption, challenge signing, and stream reception. |
| `markets` | Discover active model order books and their depth. |
| `quote` | Compute an executable quote over current order-book depth. |
| `orders` | List, show, or cancel this note's resting inference orders. |
| `monitor` | Human-readable, read-only view of the loaded note's offers, deals, and exposure. |
| `reclaim` | Buyer reclaims escrow when a seller does not show. |
| `recover` | Buyer closes an orphaned open deal so it can be settled. |
| `dispute` | Buyer opens an on-chain dispute on an open deal. |
| `destroy` | Seller closes a stopped deal's token contract. |

Run `dexdo <command> --help` for the flags of any command.

## Configuration

`dexdo` reads the deployed contract pins from a manifest: the one the
installer put at `<home>/.dexdo/manifest.json`, or the one `DEXDO_MANIFEST`
names.

A **seller** also needs a model catalogue: `models.json` in the working
directory, describing the upstream it proxies. You write that file --
`models.example.json` beside this README is a filled-in shape to copy and
edit, and nothing loads it under that name. Put your provider key in the
environment variable its `api_key_env` names, never in the file itself.

A **buyer** needs one only to add per-model verification data, or to use a
short local nickname instead of the market's full name. `market`,
`executable-book`, `quote`, `orders` and `subscription` resolve a model name
against the on-chain ModelRegistry and need no catalogue at all.

The manifest names the network it pins, and the client takes the network from
it, endpoint and all: which file you point at is which network you work on.
With the variable unset the client reads `$HOME/.dexdo/manifest.json` on
Linux/macOS or `%USERPROFILE%\.dexdo\manifest.json` on Windows, which the
installer puts there -- so a fresh install already works, on mainnet, with
nothing for you to configure. This is the only default: the working directory,
binary directory and platform configuration directories are never searched.

Set `DEXDO_MANIFEST` to use a manifest kept anywhere else -- another network,
or a copy you maintain yourself. It wins wherever it is set, and a path it
names that does not exist is refused against that path rather than falling
back to the installed default.

```sh
export DEXDO_MANIFEST=~/dexdo/mainnet.manifest.json
```

### Output detail

Without `RUST_LOG` the client prints four kinds of line and nothing else: what it
is doing now, what it has finished, the result, and refusals. Records at `info`
are off for **every** command, `dexdo seller` included.

This matters most for the seller. Its readiness components are `info` records:

```text
component="gateway_task" status="pass"
component="advertised_gateway" status="pass"
component="upstream_authentication_and_model" status="pass"
```

A healthy seller prints none of them, so their absence is not a fault. Set
`RUST_LOG=info` to get them back.

Reading a seller with a program: do not depend on those records even with
`RUST_LOG`. Use the printed readiness line, which needs no `RUST_LOG` and carries
the whole verdict:

```text
seller_ready token_contract=<addr> gateway=<host:port> gateway_listen=<addr> order_id=<n> readiness=exact_tc_offer_accepted
```

### Seller metrics (OpenTelemetry)

`dexdo seller` can export metrics over OTLP/gRPC to an OpenTelemetry Collector.
Export is off by default and independent of `RUST_LOG`; no other command starts
it. Only metrics are exported, never logs or traces, and no labels carry
addresses, keys, prompts or per-deal identifiers.

| Variable | Default | Meaning |
|---|---|---|
| `OTEL_METRICS_EXPORTER` | `none` | `otlp` turns export on; `none`, `off` or empty keep it off. |
| `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | `http://localhost:4317` | Collector address: an `http` or `https` origin with no credentials, path or query. Falls back to `OTEL_EXPORTER_OTLP_ENDPOINT`. |
| `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL` | `grpc` | Only `grpc` is accepted. Falls back to `OTEL_EXPORTER_OTLP_PROTOCOL`. |
| `OTEL_EXPORTER_OTLP_METRICS_HEADERS` | none | `name=value` pairs separated by commas, values percent-encoded, for example `authorization=Bearer%20<token>`. Falls back to `OTEL_EXPORTER_OTLP_HEADERS`. |
| `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT` | `5000` | Export timeout, milliseconds. Falls back to `OTEL_EXPORTER_OTLP_TIMEOUT`. |
| `OTEL_METRIC_EXPORT_INTERVAL` | `15000` | Time between exports, milliseconds. |
| `OTEL_SERVICE_NAME` | `dexdo-seller` | `service.name` resource attribute. |
| `OTEL_RESOURCE_ATTRIBUTES` | none | Extra resource attributes as `key=value` pairs. Give every seller a unique, stable `service.instance.id`. `role`, `network` and `service.name` set here are ignored. |

```sh
export OTEL_METRICS_EXPORTER=otlp
export OTEL_EXPORTER_OTLP_METRICS_ENDPOINT=http://127.0.0.1:4317
export OTEL_RESOURCE_ATTRIBUTES=service.instance.id=seller-west
dexdo seller ...
```

The client adds `role=seller`, `network` (`mock` under `--mock-chain`) and
`service.version` to every series. An invalid configuration with export turned
on stops `dexdo seller` before it trades. A collector that is down or slow does
not: trading and inference continue, the seller prints one line when export
starts failing and one when it recovers, and shutdown waits for the collector at
most the export timeout. `https` uses the system certificate roots; custom CA
and client certificates are not supported.

Metric names below are OTLP instrument names. A Prometheus exporter in the
Collector usually rewrites them, for example `dexdo_seller_heartbeat_seconds`
and `dexdo_seller_upstream_requests_total`; check the exposed names before
writing alerts. Counters are cumulative. Every metric except
`heartbeat` and `telemetry.export_failures` carries a `model` label; the label
table below the metrics says what each label value means.

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

| Label | Values | Meaning |
|---|---|---|
| `model` | the market's model name | The model this seller sells, as configured in `models.json` for the market; `mock` for the mock backend. Never taken from a buyer request. |
| `component` | `gateway_task` | The seller's gateway task inside the process is still running. |
| | `advertised_gateway` | The seller reaches its own gateway at the address it advertised to buyers, the way a buyer would. |
| | `upstream_authentication_and_model` | A small real request to the model provider succeeds with the configured key, and the model that answers is the one being sold. |
| `outcome` on `health.*` | `pass`, `fail`, `timeout` | `fail`: the check got an answer and it was wrong (an error, a refused connection, a different model). `timeout`: no answer within 20 s. |
| `state` on `health.status` | `pass`, `fail`, `unknown` | `unknown`: no check of a watched deal in the last 120 s. |
| `outcome` on `offer.events` | `matched` | A buyer bought the ask. |
| | `expired` | The ask reached its deadline unsold. |
| | `relisted` | The seller posted a fresh ask after one expired. |
| | `recovering` | Posting an ask failed and the seller is retrying. |
| | `isolated` | A deal watcher stopped after an error; that deal is no longer supervised by this process. |
| `purpose` | `inference` | A buyer's request. |
| | `probe` | The seller's own readiness check. |
| `backend` | `openai`, `anthropic`, `mock` | OpenAI-compatible provider, native Anthropic API, or the simulated backend, which never reaches a real provider. |
| `outcome` on `upstream.*` | `success` | The stream ended with the provider's usage record. |
| | `error` | The provider or the stream failed; see `error_class`. |
| | `cancelled` | The call stopped before either, usually because the buyer disconnected. |
| `error_class` | `none` | No error (`outcome` is not `error`). |
| | `unavailable` | The provider could not be reached, the connection broke, or it answered with an HTTP error status. Provider rate limits (`429`) and rejected keys (`401`, `403`) land here too. |
| | `timeout` | The provider produced no output before the seller's supervision deadline. |
| | `stream_protocol` | The provider's stream was malformed or ended without its usage record. |
| | `configuration` | A seller-side setup problem: missing key, unsupported request parameter, output cap. |
| | `rate_limit` | A single stream frame from the provider exceeded the seller's buffer cap. |
| | `authentication` | Reserved; the adapters do not produce it today. |
| | `other` | Any other failure. |
| `status_class` | `started` | On `upstream.attempts`: an HTTP attempt began. |
| | `2xx`, `4xx`, `5xx`, `other` | On `upstream.responses`: the class of the provider's HTTP status; `status_code` carries the exact code. |

A failed or stale chain read is reported as unknown, never as zero: while
`offer.unknown` is nonzero for a model, `offer.executable` and
`offer.remaining_ticks` report `NaN` for it. A matched ask is an end state: once
the last watched deal of a model is sold, its offer series show `0` and its
health series keep their last result instead of turning `unknown`. A watcher
that stops does not mean the offer was cancelled.

## Guides: 

* [DEX.DO glossary](docs/glossary.md),
* [Registered AI models](docs/registered-models.md),
* [Wallets and fund flows](docs/wallets-and-funds.md),
* [Withdrawing PrivateNote funds to a wallet](docs/withdraw-private-note-to-wallet.md),
* [Disable reasoning for text generation](docs/reasoning-control.md)

## License

Released under the MIT License -- see [LICENSE](LICENSE).
