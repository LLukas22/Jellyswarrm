CREATE TABLE unified_library_groups (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    name         TEXT    NOT NULL UNIQUE,
    library_type TEXT    NOT NULL,
    virtual_id   TEXT    NOT NULL UNIQUE,
    created_at   TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at   TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX idx_unified_library_groups_virtual_id ON unified_library_groups(virtual_id);
