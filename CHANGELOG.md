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

