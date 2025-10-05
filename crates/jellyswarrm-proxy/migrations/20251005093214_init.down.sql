-- Add down migration script here
-- Not much to do here since we are just dropping the table
DROP TABLE IF EXISTS media_mappings;

DROP INDEX IF EXISTS idx_media_mappings_virtual_id;
DROP INDEX IF EXISTS idx_media_mappings_original_server;

DROP TABLE IF EXISTS servers;
DROP TABLE IF EXISTS users;
DROP TABLE IF EXISTS server_mappings;
DROP TABLE IF EXISTS authorization_sessions;

DROP INDEX IF EXISTS idx_authorization_sessions_mapping;
DROP INDEX IF EXISTS idx_users_virtual_key;
DROP INDEX IF EXISTS idx_server_mappings_user_server;
DROP INDEX IF EXISTS idx_authorization_sessions_user_server;

