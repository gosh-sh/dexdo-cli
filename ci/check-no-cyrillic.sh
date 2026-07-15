#!/usr/bin/env bash
# Release gate: source code must be English-only — fail on ANY Cyrillic character.
# Keeps non-English text (comments, identifiers, string literals) out of release artifacts.
#
# Scope = source code and shipped docs. Excluded: coordinator working docs under
# directives/ and executor sidecar reports (*.report.md), which are not code.
#
# Detection is done in Python (deterministic Unicode handling, portable across CI
# images); the file list comes from git so only tracked, in-scope files are scanned.
set -uo pipefail
cd "$(dirname "$0")/.."

mapfile -t FILES < <(git ls-files -- \
  '*.rs' '*.sh' '*.toml' '*.yml' '*.yaml' '*.proto' 'README.md' 'PLATFORMS.md' \
  ':(exclude)directives/' \
  ':(exclude)*.report.md')

if [ "${#FILES[@]}" -eq 0 ]; then
  echo "OK: no in-scope files to scan."
  exit 0
fi

python3 - "${FILES[@]}" <<'PY'
import re, sys

# Cyrillic (U+0400..U+04FF) + Cyrillic Supplement (U+0500..U+052F).
# Escaped (not literal) so this gate script itself stays pure-ASCII and does not self-flag.
CYRILLIC = re.compile('[' + chr(0x0400) + '-' + chr(0x052F) + ']')
hits = 0
for path in sys.argv[1:]:
    try:
        with open(path, encoding='utf-8', errors='replace') as fh:
            for n, line in enumerate(fh, 1):
                if CYRILLIC.search(line):
                    print(f"{path}:{n}:{line.rstrip()}")
                    hits += 1
    except FileNotFoundError:
        pass

if hits:
    sys.stderr.write(
        f"\nERROR: {hits} line(s) contain Cyrillic. Source code must be "
        "English-only (comments, identifiers, string literals). Translate and re-run.\n"
    )
    sys.exit(1)
print(f"OK: no Cyrillic in source code ({len(sys.argv) - 1} files scanned).")
PY
