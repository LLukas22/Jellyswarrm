-- Remove performance indexes

DROP INDEX IF EXISTS idx_authorization_sessions_device_id;
DROP INDEX IF EXISTS idx_authorization_sessions_client;
DROP INDEX IF EXISTS idx_authorization_sessions_expires_at;
DROP INDEX IF EXISTS idx_authorization_sessions_user_id_only;
DROP INDEX IF EXISTS idx_authorization_sessions_user_device;
DROP INDEX IF EXISTS idx_servers_priority;
DROP INDEX IF EXISTS idx_server_mappings_user_id;
