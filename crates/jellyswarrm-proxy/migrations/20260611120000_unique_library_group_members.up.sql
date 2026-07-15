DELETE FROM library_group_members
WHERE rowid NOT IN (
    SELECT MAX(rowid)
    FROM library_group_members
    GROUP BY server_id, original_library_id
);

DROP INDEX IF EXISTS idx_library_group_members_server_library;

CREATE UNIQUE INDEX idx_library_group_members_server_library
    ON library_group_members (server_id, original_library_id);
