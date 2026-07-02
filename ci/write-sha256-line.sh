#!/bin/sh
set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: write-sha256-line.sh <sha256> <asset>" >&2
  exit 2
fi

hash="$1"
asset="${2##*/}"

case "$hash" in
  ""|*[!0123456789abcdefABCDEF]*)
    echo "write-sha256-line: invalid SHA256 hash" >&2
    exit 2
    ;;
esac

if [ "${#hash}" -ne 64 ]; then
  echo "write-sha256-line: SHA256 hash must be 64 hex characters" >&2
  exit 2
fi

printf '%s  %s\n' "$hash" "$asset"
