CREATE TABLE IF NOT EXISTS library_groups (
    virtual_id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL UNIQUE,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS library_group_members (
    group_virtual_id TEXT NOT NULL REFERENCES library_groups(virtual_id) ON DELETE CASCADE,
    server_id INTEGER NOT NULL,
    original_library_id TEXT NOT NULL,
    library_name TEXT NOT NULL,
    PRIMARY KEY (group_virtual_id, server_id, original_library_id),
    FOREIGN KEY (server_id) REFERENCES servers(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_library_group_members_server_library
    ON library_group_members (server_id, original_library_id);
