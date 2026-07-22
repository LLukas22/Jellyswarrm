DROP INDEX IF EXISTS idx_library_group_members_server_library;

CREATE INDEX idx_library_group_members_server_library
    ON library_group_members (server_id, original_library_id);
