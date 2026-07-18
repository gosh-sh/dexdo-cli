## v0.0.14

- fix: stamp workspace version to release tag
- fix: persist on-demand ambiguous-submit recovery identity
- fix( corrective): injectable note-deploy retry boundary + real production-loop regressions
- mitigate: harden dexdo note-deploy Hermez SRS preflight
- Directive: release under LIVE shellnet 4.0.27 (supersedes 362/4.0.21)
- refactor(cli): move buyer command core to cli/buyer.rs (C15)
- refactor(cli): move run_monitor to cli/monitor.rs (C14)
- refactor(cli): move run_seller to cli/seller.rs (C13)
- refactor(cli): move run_note_withdraw handler to cli/note_cmd.rs (C12)
- refactor(cli): move markets list handlers to cli/markets.rs (C11)
- refactor(cli): move provision/market-deploy/destroy admin handlers to cli/admin.rs (C10)
- fix: restore read-only test coverage + correct C7/C8/C9/C-FIX/lint evidence (foreground recovery, supersedes PR465)
- chore(cli): fix pre-existing shellnet-binary clippy lints + enforce the gate
- fix(cli): repair dexdo shellnet binary build (C8/C9 regression) + add binary-shellnet ci gate
- refactor(cli): move note balance/deploy handlers to cli/note_cmd.rs (C9)
- refactor(cli): move note recover/stream-lock handlers to cli/note_cmd.rs (C8)
- refactor(cli): move deal-close handlers to cli/close.rs (C7)
- refactor(cli): move run_orders order-book view handler to cli/orders.rs (C6)
- refactor(cli): move market-data/quote view handlers to cli/market_views.rs (C5)
- refactor(cli): move reporting/view handlers to cli/reports.rs (C4)
- refactor(c3): move seller policy helpers to cli/seller_policy.rs (move-only)
- refactor(c2): move recovery handlers to cli/recover.rs (move-only)
- refactor(c1): move oracle handlers to cli/oracle.rs (move-only) [review -- merge awaits owner]
- refactor(a1): drop 5 dead Real* shellnet backend accessors (A1.d)
- Directive amendment: merge authority for refactor track (unblock Coordinator)

## v0.0.13

- mitigate: harden dexdo note-deploy Hermez SRS preflight
- Directive: release under LIVE shellnet 4.0.27 (supersedes 362/4.0.21)
- refactor(cli): move buyer command core to cli/buyer.rs (C15)
- refactor(cli): move run_monitor to cli/monitor.rs (C14)
- refactor(cli): move run_seller to cli/seller.rs (C13)
- refactor(cli): move run_note_withdraw handler to cli/note_cmd.rs (C12)
- refactor(cli): move markets list handlers to cli/markets.rs (C11)
- refactor(cli): move provision/market-deploy/destroy admin handlers to cli/admin.rs (C10)
- fix: restore read-only test coverage + correct C7/C8/C9/C-FIX/lint evidence (foreground recovery, supersedes PR465)
- chore(cli): fix pre-existing shellnet-binary clippy lints + enforce the gate
- fix(cli): repair dexdo shellnet binary build (C8/C9 regression) + add binary-shellnet ci gate
- refactor(cli): move note balance/deploy handlers to cli/note_cmd.rs (C9)
- refactor(cli): move note recover/stream-lock handlers to cli/note_cmd.rs (C8)
- refactor(cli): move deal-close handlers to cli/close.rs (C7)
- refactor(cli): move run_orders order-book view handler to cli/orders.rs (C6)
- refactor(cli): move market-data/quote view handlers to cli/market_views.rs (C5)
- refactor(cli): move reporting/view handlers to cli/reports.rs (C4)
- refactor(c3): move seller policy helpers to cli/seller_policy.rs (move-only)
- refactor(c2): move recovery handlers to cli/recover.rs (move-only)
- refactor(c1): move oracle handlers to cli/oracle.rs (move-only) [review -- merge awaits owner]
- refactor(a1): drop 5 dead Real* shellnet backend accessors (A1.d)
- Directive amendment: merge authority for refactor track (unblock Coordinator)
- refactor(a1): drop dead accessor StreamVerifier::claimed_model (A1.c)
- refactor(a1): drop dead pub struct TokenBudget (A1.b)
- chore(PR-0): canonicalize the one un-fmt'd assert (unblock ci-local fmt gate)

## v0.0.12

- release(v0.0.12): order-management scan skips filled/consumed orders instead of aborting (#433) -- a filled order lingering in the book (getOrder amount=0) no longer aborts `dexdo orders list` or the buyer's book scan before it reaches live orders behind it
- proven live (4.0.27/dapp-4): on a book with filled/zeroed slots and a live ask, the scan skips the dead slots and lists the live order

## v0.0.11

- release(v0.0.11): fix note deploy on the relaunched shellnet -- Hermez KZG voucher prover (gosh-ackinacki v0.4.1) so deployPrivateNote verifies against the node post-rotation DEX verifier (was ERR_INVALID_ZKPROOF/exit 137)
- release(v0.0.11): re-pin to 4.0.27 / dapp-4; RootPN v1 hash; tvm-sdk v3.0.4 (getDetails v2 out-actions)
- proven live: note deploy exit 0, full trade e2e (offer rests, deal funds, real qwen stream, settle)

## v0.0.9

- directive(v0.0.9): item 12 -- withdraw fail-closed on prev-gen note (public fund loss)
- directive(v0.0.9): narrow scope to client-only, contract-independent fixes
- directive(v0.0.9): item 11 -- phantom order root = destroy orphans order (/)
- directive(v0.0.9): item 9 contract fixed in 4.0.21, client work remains
- directive(v0.0.9): item 10 -- withdraw stuck stream-lock (public )
- directive(v0.0.9): item 9 root -- undeployed TC, possible money hole
- directive(v0.0.9): amend item 9 -- qwen book deadlocked by best-priced unusable ask
- directive(v0.0.9): item 9 -- buyer selector vs contract FIFO (public )
- directive(v0.0.9): item 8 -- stale recovery file (public )
- directive(v0.0.9): amend item 3 after new by-fact from
- directive: v0.0.9 post-release fixes
- directive: agent wallets (verify hash at pinned tag + wallet onboard/topup)
- chore: bump version to 0.0.8
- fix: accept matched seller offer outcomes
- directive: seller TC Uninit / offer never rests
- fix: recover note deploy after interrupted pool write
- fix: TC pool recovery and CLI nitpicks
- Buyer executable book and auto-match
- directive: note deploy money-safety (persist key before spend)
- directives: remaining + buyer view-book/auto-match
- Guard note deploy key mismatches
- precheck withdrawn seller notes
- Bound direct shellnet read commands
- directive: keypair check + pool-key mismatch + 101 guidance
- directives: seller postSellOffer root + read timeout

## v0.0.8

- chore: bump version to 0.0.8
- fix: accept matched seller offer outcomes
- directive: seller TC Uninit / offer never rests
- fix: recover note deploy after interrupted pool write
- fix: TC pool recovery and CLI nitpicks
- Buyer executable book and auto-match
- directive: note deploy money-safety (persist key before spend)
- directives: remaining + buyer view-book/auto-match
- Guard note deploy key mismatches
- precheck withdrawn seller notes
- Bound direct shellnet read commands
- directive: keypair check + pool-key mismatch + 101 guidance
- directives: seller postSellOffer root + read timeout
- agents(8): e2e = happy + every negative case, each to the very end
- agents(8): ban crafted/isolated live proofs; lead must inspect proof setup ( lesson)
- fix: reject one-tick on-demand buys before escrow
- directive: v0.0.7 on-demand endpoint_binding INTERNAL
- release: bump version to 0.0.7
- agents(8): e2e test must drive the full flow to completion (the lesson)
- fix: preflight on-demand buyer content policy
- directive: on-demand buyer INVALID_ARGUMENT after handover
- release: bump version to 0.0.6
- fix: carry unverified model fallback onto current dev
- directive: carry PR307 unverified-model fallback onto post-
- fix: point seller abandoned Probe recovery to advance

## v0.0.7

- release: bump version to 0.0.7
- agents(8): e2e test must drive the full flow to completion (the lesson)
- fix: preflight on-demand buyer content policy
- directive: on-demand buyer INVALID_ARGUMENT after handover
- release: bump version to 0.0.6
- fix: carry unverified model fallback onto current dev
- directive: carry PR307 unverified-model fallback onto post-
- fix: point seller abandoned Probe recovery to advance
- directive: seller guidance on buyer-abandoned deal
- fix: allow submit-safe partial quote
- fix: embed ModelRegistry ABI
- agents(10): merge only on owner's direct permission (LGTM is not a merge invitation)
- directives: quote partial-take + embed ModelRegistry ABI
- fix: bind buyer API before on-demand chain work
- directive: buyer bind-first (fix silent hang)
- release: bump version to 0.0.5
- fix: align shellnet quote with buyer submit path
- directive: buyer place_buy fix (for executor)
- agents(8): fix = regression test + live shellnet proof (owner rule)
- docs: fix dexdo-install doctor/policy wording
- release: publish dexdo-install skill (allow-list + normalize)
- skills: dexdo-install (install the CLI + get an agent working)
- fix: dexdo --version reports the release version (0.0.4)
- test: cover quote stale liquidity regression
- agents(10): /executor never merge; lead-only merge after Section 9 (money-path)

## v0.0.6

- release: bump version to 0.0.6
- fix: carry unverified model fallback onto current dev
- directive: carry PR307 unverified-model fallback onto post-
- fix: point seller abandoned Probe recovery to advance
- directive: seller guidance on buyer-abandoned deal
- fix: allow submit-safe partial quote
- fix: embed ModelRegistry ABI
- agents(10): merge only on owner's direct permission (LGTM is not a merge invitation)
- directives: quote partial-take + embed ModelRegistry ABI
- fix: bind buyer API before on-demand chain work
- directive: buyer bind-first (fix silent hang)
- release: bump version to 0.0.5
- fix: align shellnet quote with buyer submit path
- directive: buyer place_buy fix (for executor)
- agents(8): fix = regression test + live shellnet proof (owner rule)
- docs: fix dexdo-install doctor/policy wording
- release: publish dexdo-install skill (allow-list + normalize)
- skills: dexdo-install (install the CLI + get an agent working)
- fix: dexdo --version reports the release version (0.0.4)
- test: cover quote stale liquidity regression
- agents(10): /executor never merge; lead-only merge after Section 9 (money-path)
- fix: skip stale buyer preflight rows
- Spec/RFC: Attested Seller Relay (dexdo-in-TEE)
- release: ASCII gate on release notes (close the notes gap)
- fix: build portable linux musl releases

## v0.0.5

- release: bump version to 0.0.5
- fix: align shellnet quote with buyer submit path
- directive: buyer place_buy fix (for executor)
- agents(8): fix = regression test + live shellnet proof (owner rule)
- docs: fix dexdo-install doctor/policy wording
- release: publish dexdo-install skill (allow-list + normalize)
- skills: dexdo-install (install the CLI + get an agent working)
- fix: dexdo --version reports the release version (0.0.4)
- test: cover quote stale liquidity regression
- agents(10): /executor never merge; lead-only merge after Section 9 (money-path)
- fix: skip stale buyer preflight rows
- Spec/RFC: Attested Seller Relay (dexdo-in-TEE)
- release: ASCII gate on release notes (close the notes gap)
- fix: build portable linux musl releases
- feat: data-driven per-model verification + Path A fail-closed
- fix: generalize model registry aliases
- docs: directive for Linux release portability
- docs(agents): explicit push targets + private vs public push
- ci: smoke-test release binaries + install
- Wire buyer failure policy dispatch
- Handle post-reject ERR_NOT_OPEN terminally
- skills: english public seller+buyer skills
- fix registry-backed qwen content identity
- Add buyer continuity mode flag
- buyer continuity -- no auto-reclaim/rebuy on idle (demand-gated + kill-switch)

## v0.0.4

- fix: dexdo --version reports the release version (0.0.4)
- test: cover quote stale liquidity regression
- agents(10): /executor never merge; lead-only merge after Section 9 (money-path)
- fix: skip stale buyer preflight rows
- Spec/RFC: Attested Seller Relay (dexdo-in-TEE)
- release: ASCII gate on release notes (close the notes gap)
- fix: build portable linux musl releases
- feat: data-driven per-model verification + Path A fail-closed
- fix: generalize model registry aliases
- docs: directive for Linux release portability
- docs(agents): explicit push targets + private vs public push
- ci: smoke-test release binaries + install
- Wire buyer failure policy dispatch
- Handle post-reject ERR_NOT_OPEN terminally
- skills: english public seller+buyer skills
- fix registry-backed qwen content identity
- Add buyer continuity mode flag
- buyer continuity -- no auto-reclaim/rebuy on idle (demand-gated + kill-switch)
- Fix qwen content identity matching
- Add seller gateway advertise address
- Shellnet update
- Coalesce equivalent duplicate TC asks for quote
- Fix monitor closed state classification
- Fix demand-driven buyer continuity
- docs(skills): self-deploy notes, order-book view + price/volume prompt, watcher, status-authoritative Phase 8

## v0.0.3

- fix: build portable linux musl releases
- feat: data-driven per-model verification + Path A fail-closed
- fix: generalize model registry aliases
- docs: directive for Linux release portability
- docs(agents): explicit push targets + private vs public push
- ci: smoke-test release binaries + install
- Wire buyer failure policy dispatch
- Handle post-reject ERR_NOT_OPEN terminally
- skills: english public seller+buyer skills
- fix registry-backed qwen content identity
- Add buyer continuity mode flag
- buyer continuity -- no auto-reclaim/rebuy on idle (demand-gated + kill-switch)
- Fix qwen content identity matching
- Add seller gateway advertise address
- Shellnet update
- Coalesce equivalent duplicate TC asks for quote
- Fix monitor closed state classification
- Fix demand-driven buyer continuity
- docs(skills): self-deploy notes, order-book view + price/volume prompt,  watcher, status-authoritative Phase 8
- docs(skills): self-deploy notes, milestone logging, buyer lifecycle + idle-close invariant
- Keep buyer API sessions open after upstream request errors
- Fix concurrent consumer API challenge responses
- fix active book e2e expectations
- tighten  failure policy validation
- Release pipeline: public dexdo-cli, cleanliness scrub gate, native binaries, publish

## v0.0.2

- docs(agents): explicit push targets + private vs public push
- ci: smoke-test release binaries + install
- Wire buyer failure policy dispatch
- Handle post-reject ERR_NOT_OPEN terminally
- skills: english public seller+buyer skills
- fix registry-backed qwen content identity
- Add buyer continuity mode flag
- buyer continuity -- no auto-reclaim/rebuy on idle (demand-gated + kill-switch)
- Fix qwen content identity matching
- Add seller gateway advertise address
- Shellnet update
- Coalesce equivalent duplicate TC asks for quote
- Fix monitor closed state classification
- Fix demand-driven buyer continuity
- docs(skills): self-deploy notes, order-book view + price/volume prompt,  watcher, status-authoritative Phase 8
- docs(skills): self-deploy notes, milestone logging, buyer lifecycle + idle-close invariant
- Keep buyer API sessions open after upstream request errors
- Fix concurrent consumer API challenge responses
- fix active book e2e expectations
- tighten  failure policy validation
- Release pipeline: public dexdo-cli, cleanliness scrub gate, native binaries, publish
- Draft: implement runtime machine JSON contract 
- Draft: client registry validation policy 
- fix seller probe reserve for issue 228
- Revert duplicate  directive (canonical 214-runtime-failure-policy-design.md already exists)

## v0.0.1

- fix registry-backed qwen content identity
- Add buyer continuity mode flag
- buyer continuity -- no auto-reclaim/rebuy on idle (demand-gated + kill-switch)
- Fix qwen content identity matching
- Add seller gateway advertise address
- Shellnet update
- Coalesce equivalent duplicate TC asks for quote
- Fix monitor closed state classification
- Fix demand-driven buyer continuity
- docs(skills): self-deploy notes, order-book view + price/volume prompt,  watcher, status-authoritative Phase 8
- docs(skills): self-deploy notes, milestone logging, buyer lifecycle + idle-close invariant
- Keep buyer API sessions open after upstream request errors
- Fix concurrent consumer API challenge responses
- fix active book e2e expectations
- tighten  failure policy validation
- Release pipeline: public dexdo-cli, cleanliness scrub gate, native binaries, publish
- Draft: implement runtime machine JSON contract 
- Draft: client registry validation policy 
- fix seller probe reserve for issue 228
- Revert duplicate  directive (canonical 214-runtime-failure-policy-design.md already exists)
- Canonical Model Registry : add contract + shellnet deploy record
- diagnose TokenContract.open probe funding abort
- Draft: policy fail-closed checkpoint
- buyer/seller failure-policy -- persistent policy.json + fail-closed once (wire-only)
- Add runtime failure policy directive

## v0.0.1

- fix registry-backed qwen content identity
- Add buyer continuity mode flag
- buyer continuity -- no auto-reclaim/rebuy on idle (demand-gated + kill-switch)
- Fix qwen content identity matching
- Add seller gateway advertise address
- Shellnet update
- Coalesce equivalent duplicate TC asks for quote
- Fix monitor closed state classification
- Fix demand-driven buyer continuity
- docs(skills): self-deploy notes, order-book view + price/volume prompt,  watcher, status-authoritative Phase 8
- docs(skills): self-deploy notes, milestone logging, buyer lifecycle + idle-close invariant
- Keep buyer API sessions open after upstream request errors
- Fix concurrent consumer API challenge responses
- fix active book e2e expectations
- tighten  failure policy validation
- Release pipeline: public dexdo-cli, cleanliness scrub gate, native binaries, publish
- Draft: implement runtime machine JSON contract 
- Draft: client registry validation policy 
- fix seller probe reserve for issue 228
- Revert duplicate  directive (canonical 214-runtime-failure-policy-design.md already exists)
- Canonical Model Registry : add contract + shellnet deploy record
- diagnose TokenContract.open probe funding abort
- Draft: policy fail-closed checkpoint
- buyer/seller failure-policy -- persistent policy.json + fail-closed once (wire-only)
- Add runtime failure policy directive

