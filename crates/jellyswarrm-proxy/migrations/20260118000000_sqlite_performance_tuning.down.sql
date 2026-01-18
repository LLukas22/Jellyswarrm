-- Down migration for SQLite performance tuning
DROP INDEX IF EXISTS idx_server_health_checked_at;
DROP INDEX IF EXISTS idx_audit_logs_cleanup;
DROP INDEX IF EXISTS idx_authorization_sessions_expires;
