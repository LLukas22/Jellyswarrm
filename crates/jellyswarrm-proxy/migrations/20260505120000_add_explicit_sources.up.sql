ALTER TABLE unified_library_groups ADD COLUMN mode TEXT NOT NULL DEFAULT 'auto';
ALTER TABLE unified_library_groups ADD COLUMN global_tag_filter TEXT;

CREATE TABLE unified_library_sources (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id              INTEGER NOT NULL REFERENCES unified_library_groups(id) ON DELETE CASCADE,
    server_id             INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    jellyfin_library_id   TEXT    NOT NULL,
    jellyfin_library_name TEXT    NOT NULL,
    tag_filter            TEXT,
    created_at            TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(group_id, server_id, jellyfin_library_id)
);

CREATE TABLE server_library_cache (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    server_id             INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    jellyfin_library_id   TEXT    NOT NULL,
    jellyfin_library_name TEXT    NOT NULL,
    collection_type       TEXT    NOT NULL,
    cached_at             TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(server_id, jellyfin_library_id)
);
