DROP INDEX IF EXISTS idx_servers_url_unique;

DROP INDEX IF EXISTS idx_media_mappings_server_id;
DROP INDEX IF EXISTS idx_media_mappings_original_server_id;
ALTER TABLE media_mappings DROP COLUMN server_id;

DROP INDEX IF EXISTS idx_server_mappings_server_id;
DROP INDEX IF EXISTS idx_server_mappings_user_server_id;
ALTER TABLE server_mappings DROP COLUMN server_id;
