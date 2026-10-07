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

## Guides: 

* [DEX.DO glossary](docs/glossary.md),
* [Registered AI models](docs/registered-models.md),
* [Wallets and fund flows](docs/wallets-and-funds.md),
* [Withdrawing PrivateNote funds to a wallet](docs/withdraw-private-note-to-wallet.md)

## License

Released under the MIT License -- see [LICENSE](LICENSE).
