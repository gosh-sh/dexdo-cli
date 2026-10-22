# Linking GitHub Bug Reports to Bug Bounty Program

## Overview

This document describes the process for linking historical GitHub bug reports to user accounts in the Dex.Do Bug Bounty program.

## Background

During Season 1, several users reported bugs via GitHub issues with detailed reproduction steps. These reports were confirmed by the team but were not automatically linked to the Bug Bounty system since it was implemented later.

## Solution

We've implemented a manual verification process to credit historical bug reports:

### For Users

1. **Submit a Linking Request**
   - Email: bounty@dex.do
   - Subject: "Link GitHub Bug Reports - [Your Dex.Do Username]"
   - Include:
     - Your Dex.Do account email/username
     - List of GitHub issue URLs you reported
     - GitHub username used for reports

2. **Verification Process**
   - Team reviews GitHub issues to confirm:
     - Issue was reported by the claimed GitHub account
     - Issue contained detailed reproduction steps
     - Issue was confirmed/discussed by team
     - Issue was filed during Season 1

3. **Credit Assignment**
   - Verified reports are manually added to your Bug Bounty account
   - You'll receive an email confirmation with updated stats
   - Credits appear on your Bug Bounty dashboard within 48 hours

### Eligibility Criteria

- Report must have been filed as a GitHub issue
- Must include detailed reproduction steps
- Must have been acknowledged/confirmed by team
- Must have been filed during Season 1 period
- GitHub account must be verifiably linked to Dex.Do account

### Timeline

- Requests processed within 5 business days
- Bulk backfill for known reporters: Completed by end of month

## Technical Implementation

For internal team reference:

### Database Schema Addition

```sql
-- New table to track GitHub issue links
CREATE TABLE bug_bounty_github_links (
  id SERIAL PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id),
  github_issue_url TEXT NOT NULL,
  github_username TEXT NOT NULL,
  verified_at TIMESTAMP NOT NULL DEFAULT NOW(),
  verified_by TEXT NOT NULL,
  severity TEXT NOT NULL,
  points_awarded INTEGER NOT NULL,
  notes TEXT,
  UNIQUE(github_issue_url)
);

CREATE INDEX idx_github_links_user ON bug_bounty_github_links(user_id);
CREATE INDEX idx_github_links_verified ON bug_bounty_github_links(verified_at);
```

### Admin Script

```bash
#!/bin/bash
# scripts/link-github-bug-report.sh
# Usage: ./link-github-bug-report.sh <user_email> <github_issue_url> <severity> <points>

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

psql $DATABASE_URL <<SQL
WITH user_lookup AS (
  SELECT id FROM users WHERE email = '$USER_EMAIL'
),
github_link AS (
  INSERT INTO bug_bounty_github_links (user_id, github_issue_url, github_username, severity, points_awarded, verified_by)
  SELECT 
    user_lookup.id,
    '$GITHUB_URL',
    (regexp_match('$GITHUB_URL', 'github\.com/([^/]+)/([^/]+)/issues/(\d+)'))[1],
    '$SEVERITY',
    $POINTS,
    '$VERIFIED_BY'
  FROM user_lookup
  ON CONFLICT (github_issue_url) DO NOTHING
  RETURNING user_id, points_awarded
)
UPDATE users
SET bug_bounty_points = bug_bounty_points + github_link.points_awarded,
    bug_bounty_reports = bug_bounty_reports + 1
FROM github_link
WHERE users.id = github_link.user_id;
SQL

echo "✓ Linked GitHub issue to $USER_EMAIL (+$POINTS points)"
```

### Bulk Import Script

```bash
#!/bin/bash
# scripts/bulk-import-github-reports.sh
# Reads from CSV: user_email,github_url,severity,points

CSV_FILE="$1"

if [ -z "$CSV_FILE" ] || [ ! -f "$CSV_FILE" ]; then
  echo "Usage: $0 <csv_file>"
  echo "CSV format: user_email,github_url,severity,points"
  exit 1
fi

tail -n +2 "$CSV_FILE" | while IFS=, read -r email url severity points; do
  echo "Processing: $email - $url"
  ./scripts/link-github-bug-report.sh "$email" "$url" "$severity" "$points" "bulk_import"
  sleep 0.5
done

echo "✓ Bulk import complete"
```

## FAQ

**Q: Why aren't my GitHub reports showing automatically?**
A: The Bug Bounty system was implemented after Season 1. Historical reports require manual verification and linking.

**Q: How long does verification take?**
A: Most requests are processed within 5 business days.

**Q: What if I can't remember all the issues I reported?**
A: Provide what you remember. We'll cross-reference with our GitHub issue tracker to find additional reports from your account.

**Q: Do I get retroactive points?**
A: Yes, verified historical reports receive the same points as if they were reported through the Bug Bounty system.

**Q: What if my GitHub username is different from my Dex.Do username?**
A: That's fine. Include both in your linking request, and we'll verify the connection.

## Contact

- Email: bounty@dex.do
- Discord: #bug-bounty channel
- Response time: 1-2 business days
