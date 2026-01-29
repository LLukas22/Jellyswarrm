-- Create merged_libraries table
-- Stores definitions of merged libraries that combine content from multiple source libraries
CREATE TABLE IF NOT EXISTS merged_libraries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    virtual_id TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    collection_type TEXT NOT NULL,
    dedup_strategy TEXT DEFAULT 'provider_ids',
    created_by TEXT,
    is_global BOOLEAN DEFAULT FALSE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Create merged_library_sources table
-- Maps which source libraries are included in each merged library
CREATE TABLE IF NOT EXISTS merged_library_sources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    merged_library_id INTEGER NOT NULL,
    server_id INTEGER NOT NULL,
    library_id TEXT NOT NULL,
    library_name TEXT,
    priority INTEGER DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (merged_library_id) REFERENCES merged_libraries(id) ON DELETE CASCADE,
    FOREIGN KEY (server_id) REFERENCES servers(id) ON DELETE CASCADE,
    UNIQUE(merged_library_id, server_id, library_id)
);

-- Indexes for faster lookups
CREATE INDEX IF NOT EXISTS idx_merged_libraries_virtual_id ON merged_libraries(virtual_id);
CREATE INDEX IF NOT EXISTS idx_merged_sources_library ON merged_library_sources(merged_library_id);
CREATE INDEX IF NOT EXISTS idx_merged_sources_server ON merged_library_sources(server_id);
