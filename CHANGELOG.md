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

