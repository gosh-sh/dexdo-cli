#!/bin/bash
set -euo pipefail

USER_EMAIL="$1"
GITHUB_URL="$2"
SEVERITY="$3"
POINTS="$4"
VERIFIED_BY="${5:-admin}"

if [ -z "$USER_EMAIL" ] || [ -z "$GITHUB_URL" ] || [ -z "$SEVERITY" ] || [ -z "$POINTS" ]; then
  echo "Usage: $0 <user_email> <github_issue_url> <severity> <points> [verified_by]"
  echo "Example: $0 user@example.com https://github.com/org/repo/issues/123 medium 50"
  exit 1
fi

if ! [[ "$POINTS" =~ ^[0-9]+$ ]]; then
  echo "Error: Points must be a number"
  exit 1
fi

if ! [[ "$GITHUB_URL" =~ ^https://github\.com/.+/issues/[0-9]+$ ]]; then
  echo "Error: Invalid GitHub issue URL format"
  exit 1
fi

GITHUB_USERNAME=$(echo "$GITHUB_URL" | sed -E 's|https://github\.com/([^/]+)/.*|\1|')

psql "${DATABASE_URL}" <<SQL
BEGIN;

WITH user_lookup AS (
  SELECT id, email FROM users WHERE email = '${USER_EMAIL}'
),
github_link AS (
  INSERT INTO bug_bounty_github_links (
    user_id, 
    github_issue_url, 
    github_username, 
    severity, 
    points_awarded, 
    verified_by,
    notes
  )
  SELECT 
    user_lookup.id,
    '${GITHUB_URL}',
    '${GITHUB_USERNAME}',
    '${SEVERITY}',
    ${POINTS},
    '${VERIFIED_BY}',
    'Linked via manual verification script'
  FROM user_lookup
  WHERE user_lookup.id IS NOT NULL
  ON CONFLICT (github_issue_url) DO NOTHING
  RETURNING user_id, points_awarded
)
UPDATE users
SET 
  bug_bounty_points = COALESCE(bug_bounty_points, 0) + github_link.points_awarded,
  bug_bounty_reports = COALESCE(bug_bounty_reports, 0) + 1,
  updated_at = NOW()
FROM github_link
WHERE users.id = github_link.user_id;

COMMIT;
SQL

if [ $? -eq 0 ]; then
  echo "✓ Successfully linked GitHub issue to ${USER_EMAIL} (+${POINTS} points)"
  echo "  Issue: ${GITHUB_URL}"
  echo "  Severity: ${SEVERITY}"
  echo "  Verified by: ${VERIFIED_BY}"
else
  echo "✗ Failed to link GitHub issue"
  exit 1
fi
