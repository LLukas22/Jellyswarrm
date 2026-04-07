-- Unified library definitions (configured by admin)
CREATE TABLE IF NOT EXISTS unified_libraries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    virtual_library_id TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    collection_type TEXT NOT NULL,
    sort_order INTEGER NOT NULL DEFAULT 0,
    dedup_policy TEXT NOT NULL DEFAULT 'show_all',
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Maps a unified library to its constituent server libraries
CREATE TABLE IF NOT EXISTS unified_library_members (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    unified_library_id INTEGER NOT NULL,
    server_id INTEGER NOT NULL,
    original_library_id TEXT NOT NULL,
    original_library_name TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (unified_library_id) REFERENCES unified_libraries(id) ON DELETE CASCADE,
    FOREIGN KEY (server_id) REFERENCES servers(id) ON DELETE CASCADE,
    UNIQUE(unified_library_id, server_id, original_library_id)
);

-- Synced item metadata index
CREATE TABLE IF NOT EXISTS synced_items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    virtual_id TEXT NOT NULL,
    server_id INTEGER NOT NULL,
    server_url TEXT NOT NULL,
    visibility_scope TEXT NOT NULL DEFAULT 'global',
    source_user_id TEXT,
    original_id TEXT NOT NULL,
    original_parent_id TEXT,
    root_library_id TEXT NOT NULL,
    root_library_name TEXT,
    item_type TEXT NOT NULL,
    collection_type TEXT,

    name TEXT,
    sort_name TEXT,
    original_title TEXT,
    overview TEXT,
    production_year INTEGER,
    community_rating REAL,
    run_time_ticks INTEGER,
    premiere_date TEXT,
    index_number INTEGER,
    parent_index_number INTEGER,
    is_folder INTEGER NOT NULL DEFAULT 0,
    child_count INTEGER,

    series_id TEXT,
    series_name TEXT,
    season_id TEXT,
    season_name TEXT,

    parent_id TEXT,
    parent_name TEXT,

    provider_ids_json TEXT,
    genres_json TEXT,
    tags_json TEXT,
    studios_json TEXT,
    people_json TEXT,

    image_tags_json TEXT,
    backdrop_image_tags_json TEXT,

    last_synced_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    sync_generation INTEGER NOT NULL DEFAULT 0,
    needs_detail_fetch INTEGER NOT NULL DEFAULT 0,

    FOREIGN KEY (server_id) REFERENCES servers(id) ON DELETE CASCADE,
    UNIQUE(server_url, original_id, visibility_scope)
);

CREATE INDEX IF NOT EXISTS idx_synced_items_virtual_id ON synced_items(virtual_id);
CREATE INDEX IF NOT EXISTS idx_synced_items_server_original ON synced_items(server_url, original_id);
CREATE INDEX IF NOT EXISTS idx_synced_items_visibility_scope ON synced_items(visibility_scope);
CREATE INDEX IF NOT EXISTS idx_synced_items_source_user_id ON synced_items(source_user_id);
CREATE INDEX IF NOT EXISTS idx_synced_items_root_library ON synced_items(root_library_id);
CREATE INDEX IF NOT EXISTS idx_synced_items_type ON synced_items(item_type);
CREATE INDEX IF NOT EXISTS idx_synced_items_parent_id ON synced_items(parent_id);
CREATE INDEX IF NOT EXISTS idx_synced_items_series_id ON synced_items(series_id);
CREATE INDEX IF NOT EXISTS idx_synced_items_name ON synced_items(name);
CREATE INDEX IF NOT EXISTS idx_synced_items_sort_name ON synced_items(sort_name);
CREATE INDEX IF NOT EXISTS idx_synced_items_production_year ON synced_items(production_year);
CREATE INDEX IF NOT EXISTS idx_synced_items_sync_gen ON synced_items(sync_generation);

-- FTS5 table for full-text search
CREATE VIRTUAL TABLE IF NOT EXISTS synced_items_fts USING fts5(
    name,
    original_title,
    overview,
    series_name,
    genres_json,
    people_json,
    studios_json,
    content='synced_items',
    content_rowid='id'
);

CREATE TRIGGER IF NOT EXISTS synced_items_ai AFTER INSERT ON synced_items BEGIN
    INSERT INTO synced_items_fts(rowid, name, original_title, overview, series_name, genres_json, people_json, studios_json)
    VALUES (new.id, new.name, new.original_title, new.overview, new.series_name, new.genres_json, new.people_json, new.studios_json);
END;

CREATE TRIGGER IF NOT EXISTS synced_items_ad AFTER DELETE ON synced_items BEGIN
    INSERT INTO synced_items_fts(synced_items_fts, rowid, name, original_title, overview, series_name, genres_json, people_json, studios_json)
    VALUES ('delete', old.id, old.name, old.original_title, old.overview, old.series_name, old.genres_json, old.people_json, old.studios_json);
END;

CREATE TRIGGER IF NOT EXISTS synced_items_au AFTER UPDATE ON synced_items BEGIN
    INSERT INTO synced_items_fts(synced_items_fts, rowid, name, original_title, overview, series_name, genres_json, people_json, studios_json)
    VALUES ('delete', old.id, old.name, old.original_title, old.overview, old.series_name, old.genres_json, old.people_json, old.studios_json);
    INSERT INTO synced_items_fts(rowid, name, original_title, overview, series_name, genres_json, people_json, studios_json)
    VALUES (new.id, new.name, new.original_title, new.overview, new.series_name, new.genres_json, new.people_json, new.studios_json);
END;

-- Per-user playback state
CREATE TABLE IF NOT EXISTS synced_user_data (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    synced_item_id INTEGER NOT NULL,
    user_id TEXT NOT NULL,
    playback_position_ticks INTEGER NOT NULL DEFAULT 0,
    play_count INTEGER NOT NULL DEFAULT 0,
    is_favorite INTEGER NOT NULL DEFAULT 0,
    played INTEGER NOT NULL DEFAULT 0,
    played_percentage REAL,
    last_played_date TEXT,
    last_synced_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (synced_item_id) REFERENCES synced_items(id) ON DELETE CASCADE,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    UNIQUE(synced_item_id, user_id)
);

CREATE INDEX IF NOT EXISTS idx_synced_user_data_user ON synced_user_data(user_id);
CREATE INDEX IF NOT EXISTS idx_synced_user_data_item ON synced_user_data(synced_item_id);

-- Sync state tracking
CREATE TABLE IF NOT EXISTS sync_state (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    server_id INTEGER NOT NULL UNIQUE,
    last_full_sync_at TIMESTAMP,
    last_incremental_sync_at TIMESTAMP,
    last_sync_generation INTEGER NOT NULL DEFAULT 0,
    sync_status TEXT NOT NULL DEFAULT 'idle',
    last_error TEXT,
    items_synced INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (server_id) REFERENCES servers(id) ON DELETE CASCADE
);

-- Dedup groups
CREATE TABLE IF NOT EXISTS dedup_groups (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    canonical_provider_key TEXT NOT NULL,
    preferred_item_id INTEGER,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(canonical_provider_key)
);

CREATE TABLE IF NOT EXISTS dedup_group_members (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    dedup_group_id INTEGER NOT NULL,
    synced_item_id INTEGER NOT NULL,
    quality_score INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (dedup_group_id) REFERENCES dedup_groups(id) ON DELETE CASCADE,
    FOREIGN KEY (synced_item_id) REFERENCES synced_items(id) ON DELETE CASCADE,
    UNIQUE(dedup_group_id, synced_item_id)
);
