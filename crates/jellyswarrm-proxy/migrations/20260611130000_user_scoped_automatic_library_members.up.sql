CREATE TABLE automatic_library_snapshots (
    automatic_virtual_id TEXT NOT NULL REFERENCES merged_libraries(virtual_id) ON DELETE CASCADE,
    access_scope_key TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (automatic_virtual_id, access_scope_key)
);

CREATE TABLE automatic_library_members (
    automatic_virtual_id TEXT NOT NULL,
    access_scope_key TEXT NOT NULL,
    server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    virtual_library_id TEXT NOT NULL,
    PRIMARY KEY (automatic_virtual_id, access_scope_key, server_id, virtual_library_id),
    FOREIGN KEY (automatic_virtual_id, access_scope_key)
        REFERENCES automatic_library_snapshots(automatic_virtual_id, access_scope_key)
        ON DELETE CASCADE
);

CREATE INDEX idx_automatic_library_members_lookup
    ON automatic_library_members (automatic_virtual_id, access_scope_key);
