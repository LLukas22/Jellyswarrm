DROP TABLE IF EXISTS server_library_cache;
DROP TABLE IF EXISTS unified_library_sources;
ALTER TABLE unified_library_groups DROP COLUMN global_tag_filter;
ALTER TABLE unified_library_groups DROP COLUMN mode;
