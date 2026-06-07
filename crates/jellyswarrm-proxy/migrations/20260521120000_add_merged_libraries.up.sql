CREATE TABLE IF NOT EXISTS merged_libraries (
    virtual_id   TEXT PRIMARY KEY NOT NULL,
    collection_type TEXT NOT NULL UNIQUE,
    name         TEXT NOT NULL,
    created_at   TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- One row per (merged library, server).
-- virtual_library_id is the virtual ID already present in media_mappings
-- that refers to the actual per-server library.
CREATE TABLE IF NOT EXISTS merged_library_members (
    merged_virtual_id  TEXT NOT NULL REFERENCES merged_libraries(virtual_id) ON DELETE CASCADE,
    server_url         TEXT NOT NULL,
    virtual_library_id TEXT NOT NULL,
    PRIMARY KEY (merged_virtual_id, server_url)
);
