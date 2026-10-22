#!/bin/bash
set -euo pipefail

CSV_FILE="$1"

if [ -z "$CSV_FILE" ] || [ ! -f "$CSV_FILE" ]; then
  echo "Usage: $0 <csv_file>"
  echo ""
  echo "CSV format (with header):"
  echo "user_email,github_url,severity,points"
  echo ""
  echo "Example:"
  echo "user@example.com,https://github.com/org/repo/issues/123,medium,50"
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LINK_SCRIPT="${SCRIPT_DIR}/link-github-bug-report.sh"

if [ ! -x "$LINK_SCRIPT" ]; then
  echo "Error: link-github-bug-report.sh not found or not executable"
  exit 1
fi

TOTAL_LINES=$(tail -n +2 "$CSV_FILE" | wc -l | tr -d ' ')
CURRENT=0
SUCCESS=0
FAILED=0

echo "Starting bulk import of ${TOTAL_LINES} GitHub bug reports..."
echo ""

tail -n +2 "$CSV_FILE" | while IFS=, read -r email url severity points; do
  CURRENT=$((CURRENT + 1))
  
  email=$(echo "$email" | xargs)
  url=$(echo "$url" | xargs)
  severity=$(echo "$severity" | xargs)
  points=$(echo "$points" | xargs)
  
  echo "[${CURRENT}/${TOTAL_LINES}] Processing: ${email}"
  
  if "$LINK_SCRIPT" "$email" "$url" "$severity" "$points" "bulk_import" 2>&1; then
    SUCCESS=$((SUCCESS + 1))
  else
    FAILED=$((FAILED + 1))
    echo "  ✗ Failed to process this entry"
  fi
  
  echo ""
  sleep 0.5
done

echo "═══════════════════════════════════════"
echo "Bulk import complete"
echo "Total processed: ${TOTAL_LINES}"
echo "Successful: ${SUCCESS}"
echo "Failed: ${FAILED}"
echo "═══════════════════════════════════════"
