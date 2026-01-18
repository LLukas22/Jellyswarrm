-- SQLite Performance Tuning Migration
-- Adds indexes to speed up cleanup queries and reduce lock contention

-- Index for health history cleanup (DELETE WHERE checked_at < ...)
CREATE INDEX IF NOT EXISTS idx_server_health_checked_at ON server_health_history(checked_at);

-- Index for audit log cleanup
CREATE INDEX IF NOT EXISTS idx_audit_logs_cleanup ON audit_logs(created_at);

-- Index for session expiry cleanup
CREATE INDEX IF NOT EXISTS idx_authorization_sessions_expires ON authorization_sessions(expires_at)
    WHERE expires_at IS NOT NULL;
