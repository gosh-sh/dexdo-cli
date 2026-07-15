#!/usr/bin/env bash
# #137 Gate 2 — single Acki Nacki SDK invariant.
#
# `dexdo note deploy` (Option 3, #138) shells out to the SDK's `onboard_user_shellnet` binary at the PROCESS
# boundary on purpose: dexdo's Cargo graph must keep exactly ONE Acki Nacki SDK (`gosh.ackinacki`, tvm-sdk v3.0.3).
# The rejected #138 approach (a `dodex-sdk` git-dep) would drag a SECOND SDK (`ackinacki-kit`, tvm-sdk v3.0.2).
# This gate fails closed if a future change reintroduces that second SDK / a second tvm-sdk source.
set -euo pipefail
cd "$(dirname "$0")/.."

tree="$(cargo tree -p dexdo --features shellnet 2>/dev/null)"

bad=0
for forbidden in ackinacki-kit dodex-sdk; do
  if grep -qi "$forbidden" <<<"$tree"; then
    echo "FAIL (#137 Gate 2): '$forbidden' is in 'cargo tree -p dexdo --features shellnet' — a SECOND Acki Nacki SDK."
    bad=1
  fi
done

# Exactly one tvm-sdk git source (gosh.ackinacki's pin). A second distinct source = dual SDK.
sources="$(grep -oE 'tvm-sdk\.git[^ )]*' <<<"$tree" | sort -u || true)"
nsrc="$(printf '%s\n' "$sources" | grep -c . || true)"
if [ "${nsrc:-0}" -gt 1 ]; then
  echo "FAIL (#137 Gate 2): ${nsrc} distinct tvm-sdk sources (dual SDK):"
  printf '%s\n' "$sources" | sed 's/^/  /'
  bad=1
fi

[ "$bad" -eq 0 ] || exit 1
echo "OK (#137 Gate 2): single Acki Nacki SDK (gosh.ackinacki); no ackinacki-kit/dodex-sdk; one tvm-sdk source."
