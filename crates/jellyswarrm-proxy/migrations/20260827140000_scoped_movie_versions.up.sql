ALTER TABLE movie_version_sources RENAME TO legacy_movie_version_sources;
ALTER TABLE movie_version_members RENAME TO legacy_movie_version_members;
ALTER TABLE movie_version_groups RENAME TO legacy_movie_version_groups;

DROP INDEX idx_movie_version_sources_group;
DROP INDEX idx_movie_version_sources_member;
DROP INDEX idx_movie_version_members_group;

CREATE TABLE movie_version_clock (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    generation INTEGER NOT NULL
);

INSERT INTO movie_version_clock (singleton, generation) VALUES (1, 0);

CREATE TABLE movie_catalog_scopes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope_key TEXT NOT NULL UNIQUE,
    committed_generation INTEGER NOT NULL DEFAULT 0,
    sources_generation INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE movie_catalog_sources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope_id INTEGER NOT NULL,
    source_key TEXT NOT NULL,
    server_id INTEGER NOT NULL,
    committed_generation INTEGER NOT NULL DEFAULT 0,
    UNIQUE (id, scope_id),
    UNIQUE (scope_id, source_key),
    FOREIGN KEY (scope_id) REFERENCES movie_catalog_scopes(id) ON DELETE CASCADE,
    FOREIGN KEY (server_id) REFERENCES servers(id) ON DELETE CASCADE
);

CREATE TABLE movie_version_groups (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    scope_id INTEGER NOT NULL,
    published INTEGER NOT NULL DEFAULT 0,
    ambiguous INTEGER NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (id, scope_id),
    FOREIGN KEY (scope_id) REFERENCES movie_catalog_scopes(id) ON DELETE CASCADE
);

CREATE TABLE movie_version_group_ids (
    virtual_media_id TEXT PRIMARY KEY,
    group_id INTEGER NOT NULL,
    canonical INTEGER NOT NULL DEFAULT 1 CHECK (canonical IN (0, 1)),
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (group_id) REFERENCES movie_version_groups(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX idx_movie_version_group_canonical_id
ON movie_version_group_ids (group_id)
WHERE canonical = 1;

CREATE TABLE movie_version_group_aliases (
    scope_id INTEGER NOT NULL,
    group_id INTEGER NOT NULL,
    provider TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    PRIMARY KEY (scope_id, provider, provider_id),
    FOREIGN KEY (group_id, scope_id)
        REFERENCES movie_version_groups(id, scope_id) ON DELETE CASCADE
);

CREATE INDEX idx_movie_version_group_aliases_group
ON movie_version_group_aliases (group_id);

CREATE TABLE movie_version_members (
    scope_id INTEGER NOT NULL,
    media_mapping_id INTEGER NOT NULL,
    group_id INTEGER,
    aliases_generation INTEGER NOT NULL DEFAULT 0,
    sources_generation INTEGER NOT NULL DEFAULT 0,
    observed_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (scope_id, media_mapping_id),
    UNIQUE (scope_id, group_id, media_mapping_id),
    FOREIGN KEY (scope_id) REFERENCES movie_catalog_scopes(id) ON DELETE CASCADE,
    FOREIGN KEY (media_mapping_id) REFERENCES media_mappings(id) ON DELETE CASCADE,
    FOREIGN KEY (group_id, scope_id) REFERENCES movie_version_groups(id, scope_id)
);

CREATE INDEX idx_movie_version_members_group
ON movie_version_members (group_id);

CREATE TABLE movie_version_aliases (
    scope_id INTEGER NOT NULL,
    media_mapping_id INTEGER NOT NULL,
    provider TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    PRIMARY KEY (scope_id, media_mapping_id, provider, provider_id),
    FOREIGN KEY (scope_id, media_mapping_id)
        REFERENCES movie_version_members(scope_id, media_mapping_id) ON DELETE CASCADE
);

CREATE INDEX idx_movie_version_alias_lookup
ON movie_version_aliases (scope_id, provider, provider_id);

CREATE TABLE movie_catalog_sightings (
    source_id INTEGER NOT NULL,
    scope_id INTEGER NOT NULL,
    media_mapping_id INTEGER NOT NULL,
    generation INTEGER NOT NULL,
    PRIMARY KEY (source_id, media_mapping_id),
    FOREIGN KEY (source_id, scope_id)
        REFERENCES movie_catalog_sources(id, scope_id) ON DELETE CASCADE,
    FOREIGN KEY (scope_id, media_mapping_id)
        REFERENCES movie_version_members(scope_id, media_mapping_id) ON DELETE CASCADE
);

CREATE INDEX idx_movie_catalog_sightings_member
ON movie_catalog_sightings (scope_id, media_mapping_id);

CREATE TABLE movie_version_sources (
    scope_id INTEGER NOT NULL,
    group_id INTEGER NOT NULL,
    member_mapping_id INTEGER NOT NULL,
    source_mapping_id INTEGER NOT NULL,
    observed_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (scope_id, group_id, source_mapping_id),
    FOREIGN KEY (group_id, scope_id)
        REFERENCES movie_version_groups(id, scope_id) ON DELETE CASCADE,
    FOREIGN KEY (scope_id, group_id, member_mapping_id)
        REFERENCES movie_version_members(scope_id, group_id, media_mapping_id) ON DELETE CASCADE,
    FOREIGN KEY (source_mapping_id) REFERENCES media_mappings(id) ON DELETE CASCADE
);

CREATE INDEX idx_movie_version_sources_member
ON movie_version_sources (scope_id, group_id, member_mapping_id);

INSERT INTO movie_catalog_scopes (scope_key)
VALUES ('legacy');

INSERT INTO movie_version_groups (id, scope_id, published, ambiguous, created_at)
SELECT
    legacy_group.id,
    scope.id,
    CASE WHEN legacy_group.ambiguous = 0 AND COUNT(legacy_member.media_mapping_id) > 1 THEN 1 ELSE 0 END,
    legacy_group.ambiguous,
    legacy_group.created_at
FROM legacy_movie_version_groups legacy_group
JOIN movie_catalog_scopes scope ON scope.scope_key = 'legacy'
LEFT JOIN legacy_movie_version_members legacy_member ON legacy_member.group_id = legacy_group.id
GROUP BY legacy_group.id;

INSERT INTO movie_version_group_ids (virtual_media_id, group_id, canonical, created_at)
SELECT virtual_media_id, id, 1, created_at
FROM legacy_movie_version_groups;

INSERT INTO movie_version_members (
    scope_id, media_mapping_id, group_id, aliases_generation, observed_at
)
SELECT scope.id, member.media_mapping_id, member.group_id, 0, member.observed_at
FROM legacy_movie_version_members member
JOIN movie_catalog_scopes scope ON scope.scope_key = 'legacy';

INSERT INTO movie_version_aliases (scope_id, media_mapping_id, provider, provider_id)
SELECT scope.id, member.media_mapping_id, legacy_group.provider, legacy_group.provider_id
FROM legacy_movie_version_members member
JOIN legacy_movie_version_groups legacy_group ON legacy_group.id = member.group_id
JOIN movie_catalog_scopes scope ON scope.scope_key = 'legacy';

INSERT INTO movie_version_group_aliases (scope_id, group_id, provider, provider_id)
SELECT scope.id, legacy_group.id, legacy_group.provider, legacy_group.provider_id
FROM legacy_movie_version_groups legacy_group
JOIN movie_catalog_scopes scope ON scope.scope_key = 'legacy';

INSERT INTO movie_catalog_sources (scope_id, source_key, server_id)
SELECT DISTINCT scope.id, 'legacy:' || member.server_id, member.server_id
FROM legacy_movie_version_members member
JOIN movie_catalog_scopes scope ON scope.scope_key = 'legacy';

INSERT INTO movie_catalog_sightings (source_id, scope_id, media_mapping_id, generation)
SELECT source.id, scope.id, member.media_mapping_id, 0
FROM legacy_movie_version_members member
JOIN movie_catalog_scopes scope ON scope.scope_key = 'legacy'
JOIN movie_catalog_sources source
    ON source.scope_id = scope.id AND source.server_id = member.server_id;

INSERT INTO movie_version_sources (
    scope_id, group_id, member_mapping_id, source_mapping_id, observed_at
)
SELECT scope.id, route.group_id, route.member_mapping_id, route.source_mapping_id, route.observed_at
FROM legacy_movie_version_sources route
JOIN movie_catalog_scopes scope ON scope.scope_key = 'legacy';

DROP TABLE legacy_movie_version_sources;
DROP TABLE legacy_movie_version_members;
DROP TABLE legacy_movie_version_groups;
