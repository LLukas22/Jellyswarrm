CREATE TABLE movie_version_groups (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    virtual_media_id TEXT NOT NULL UNIQUE,
    provider TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    ambiguous INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (provider, provider_id)
);

CREATE TABLE movie_version_members (
    group_id INTEGER NOT NULL,
    media_mapping_id INTEGER NOT NULL UNIQUE,
    server_id INTEGER NOT NULL,
    observed_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (group_id, media_mapping_id),
    UNIQUE (group_id, server_id),
    FOREIGN KEY (group_id) REFERENCES movie_version_groups(id) ON DELETE CASCADE,
    FOREIGN KEY (media_mapping_id) REFERENCES media_mappings(id) ON DELETE CASCADE,
    FOREIGN KEY (server_id) REFERENCES servers(id) ON DELETE CASCADE
);

CREATE INDEX idx_movie_version_members_group
ON movie_version_members (group_id);

CREATE TABLE movie_version_sources (
    group_id INTEGER NOT NULL,
    member_mapping_id INTEGER NOT NULL,
    source_mapping_id INTEGER NOT NULL UNIQUE,
    observed_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (group_id, source_mapping_id),
    FOREIGN KEY (group_id) REFERENCES movie_version_groups(id) ON DELETE CASCADE,
    FOREIGN KEY (group_id, member_mapping_id)
        REFERENCES movie_version_members(group_id, media_mapping_id) ON DELETE CASCADE,
    FOREIGN KEY (source_mapping_id) REFERENCES media_mappings(id) ON DELETE CASCADE
);

CREATE INDEX idx_movie_version_sources_group
ON movie_version_sources (group_id);

CREATE INDEX idx_movie_version_sources_member
ON movie_version_sources (member_mapping_id);
