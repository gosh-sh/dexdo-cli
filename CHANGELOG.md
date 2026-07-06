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

