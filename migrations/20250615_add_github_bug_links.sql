-- Migration: Add GitHub bug report linking for Bug Bounty program
-- Date: 2025-06-15
-- Issue: #107

BEGIN;

-- Create table to track GitHub issue links
CREATE TABLE IF NOT EXISTS bug_bounty_github_links (
  id SERIAL PRIMARY KEY,
  user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  github_issue_url TEXT NOT NULL,
  github_username TEXT NOT NULL,
  verified_at TIMESTAMP NOT NULL DEFAULT NOW(),
  verified_by TEXT NOT NULL,
  severity TEXT NOT NULL CHECK (severity IN ('critical', 'high', 'medium', 'low')),
  points_awarded INTEGER NOT NULL CHECK (points_awarded >= 0),
  notes TEXT,
  created_at TIMESTAMP NOT NULL DEFAULT NOW(),
  CONSTRAINT unique_github_issue UNIQUE(github_issue_url)
);

-- Indexes for performance
CREATE INDEX idx_github_links_user_id ON bug_bounty_github_links(user_id);
CREATE INDEX idx_github_links_verified_at ON bug_bounty_github_links(verified_at DESC);
CREATE INDEX idx_github_links_github_username ON bug_bounty_github_links(github_username);

-- Add columns to users table if they don't exist
DO $$ 
BEGIN
  IF NOT EXISTS (SELECT 1 FROM information_schema.columns 
                 WHERE table_name='users' AND column_name='bug_bounty_points') THEN
    ALTER TABLE users ADD COLUMN bug_bounty_points INTEGER NOT NULL DEFAULT 0;
  END IF;
  
  IF NOT EXISTS (SELECT 1 FROM information_schema.columns 
                 WHERE table_name='users' AND column_name='bug_bounty_reports') THEN
    ALTER TABLE users ADD COLUMN bug_bounty_reports INTEGER NOT NULL DEFAULT 0;
  END IF;
END $$;

-- Create view for easy reporting
CREATE OR REPLACE VIEW bug_bounty_leaderboard AS
SELECT 
  u.id,
  u.email,
  u.username,
  u.bug_bounty_points,
  u.bug_bounty_reports,
  COUNT(bgl.id) as github_linked_reports,
  COALESCE(SUM(bgl.points_awarded), 0) as github_points
FROM users u
LEFT JOIN bug_bounty_github_links bgl ON u.id = bgl.user_id
WHERE u.bug_bounty_reports > 0 OR bgl.id IS NOT NULL
GROUP BY u.id, u.email, u.username, u.bug_bounty_points, u.bug_bounty_reports
ORDER BY u.bug_bounty_points DESC;

-- Grant permissions
GRANT SELECT ON bug_bounty_github_links TO readonly_user;
GRANT SELECT ON bug_bounty_leaderboard TO readonly_user;

COMMIT;

-- Rollback script (save separately as rollback_20250615_add_github_bug_links.sql)
-- BEGIN;
-- DROP VIEW IF EXISTS bug_bounty_leaderboard;
-- DROP TABLE IF EXISTS bug_bounty_github_links;
-- ALTER TABLE users DROP COLUMN IF EXISTS bug_bounty_points;
-- ALTER TABLE users DROP COLUMN IF EXISTS bug_bounty_reports;
-- COMMIT;
