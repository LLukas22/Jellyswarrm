-- Enhanced features migration
-- Adds additional columns for user permissions, API keys, and rate limiting

-- Update user_permissions table to support allow/deny model
ALTER TABLE user_permissions ADD COLUMN permission_type TEXT NOT NULL DEFAULT 'allow';
ALTER TABLE user_permissions ADD COLUMN created_by TEXT NOT NULL DEFAULT 'system';

-- Update api_keys table with additional fields
ALTER TABLE api_keys ADD COLUMN key_prefix TEXT NOT NULL DEFAULT '';
ALTER TABLE api_keys ADD COLUMN permissions TEXT NOT NULL DEFAULT '[]';
ALTER TABLE api_keys ADD COLUMN created_by TEXT NOT NULL DEFAULT '';

-- Drop old foreign key constraint and make admin_id optional
-- SQLite doesn't support dropping columns, so we work around it

-- Add rate limiting tracking table
CREATE TABLE IF NOT EXISTS rate_limit_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ip_address TEXT NOT NULL,
    endpoint TEXT NOT NULL,
    blocked_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX IF NOT EXISTS idx_rate_limit_ip_time ON rate_limit_events(ip_address, blocked_at DESC);

-- Cleanup old rate limit events (run periodically)
-- DELETE FROM rate_limit_events WHERE blocked_at < datetime('now', '-1 day');
