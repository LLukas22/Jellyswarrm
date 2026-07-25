CREATE TABLE library_groups (
    virtual_id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL UNIQUE,
    sort_order INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE library_group_members (
    group_virtual_id TEXT NOT NULL REFERENCES library_groups(virtual_id) ON DELETE CASCADE,
    server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    original_library_id TEXT NOT NULL,
    library_name TEXT NOT NULL,
    PRIMARY KEY (group_virtual_id, server_id, original_library_id)
);

CREATE UNIQUE INDEX idx_library_group_members_server_library
    ON library_group_members (server_id, original_library_id);

-- Automatic memberships are snapshots scoped to a Jellyswarrm user ID.
CREATE TABLE automatic_library_snapshots (
    automatic_virtual_id TEXT NOT NULL REFERENCES merged_libraries(virtual_id) ON DELETE CASCADE,
    user_id TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (automatic_virtual_id, user_id)
);

CREATE TABLE automatic_library_members (
    automatic_virtual_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    virtual_library_id TEXT NOT NULL,
    PRIMARY KEY (automatic_virtual_id, user_id, server_id, virtual_library_id),
    FOREIGN KEY (automatic_virtual_id, user_id)
        REFERENCES automatic_library_snapshots(automatic_virtual_id, user_id)
        ON DELETE CASCADE
);

CREATE INDEX idx_automatic_library_members_lookup
    ON automatic_library_members (automatic_virtual_id, user_id);

CREATE TABLE discovered_libraries (
    server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    original_library_id TEXT NOT NULL,
    name TEXT NOT NULL,
    collection_type TEXT NOT NULL,
    last_seen_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (server_id, original_library_id)
);
