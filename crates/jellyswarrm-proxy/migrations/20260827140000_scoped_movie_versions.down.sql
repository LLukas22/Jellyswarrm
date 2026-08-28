ALTER TABLE movie_version_sources RENAME TO scoped_movie_version_sources;
ALTER TABLE movie_version_members RENAME TO scoped_movie_version_members;
ALTER TABLE movie_version_groups RENAME TO scoped_movie_version_groups;

DROP INDEX idx_movie_version_sources_member;
DROP INDEX idx_movie_version_members_group;

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

INSERT INTO movie_version_groups (
    id, virtual_media_id, provider, provider_id, ambiguous, created_at
)
SELECT
    scoped_group.id,
    group_id.virtual_media_id,
    alias.provider,
    alias.provider_id,
    scoped_group.ambiguous,
    scoped_group.created_at
FROM scoped_movie_version_groups scoped_group
JOIN movie_catalog_scopes scope
    ON scope.id = scoped_group.scope_id AND scope.scope_key = 'legacy'
JOIN movie_version_group_ids group_id
    ON group_id.group_id = scoped_group.id AND group_id.canonical = 1
JOIN scoped_movie_version_members member
    ON member.group_id = scoped_group.id
JOIN movie_version_aliases alias
    ON alias.scope_id = member.scope_id AND alias.media_mapping_id = member.media_mapping_id
GROUP BY scoped_group.id;

INSERT INTO movie_version_members (
    group_id, media_mapping_id, server_id, observed_at
)
SELECT member.group_id, member.media_mapping_id, mapping.server_id, member.observed_at
FROM scoped_movie_version_members member
JOIN movie_catalog_scopes scope
    ON scope.id = member.scope_id AND scope.scope_key = 'legacy'
JOIN media_mappings mapping ON mapping.id = member.media_mapping_id
WHERE member.group_id IS NOT NULL;

INSERT INTO movie_version_sources (
    group_id, member_mapping_id, source_mapping_id, observed_at
)
SELECT route.group_id, route.member_mapping_id, route.source_mapping_id, route.observed_at
FROM scoped_movie_version_sources route
JOIN movie_catalog_scopes scope
    ON scope.id = route.scope_id AND scope.scope_key = 'legacy';

DROP TABLE movie_catalog_sightings;
DROP TABLE movie_version_aliases;
DROP TABLE movie_version_group_aliases;
DROP TABLE scoped_movie_version_sources;
DROP TABLE scoped_movie_version_members;
DROP TABLE movie_version_group_ids;
DROP TABLE scoped_movie_version_groups;
DROP TABLE movie_catalog_sources;
DROP TABLE movie_catalog_scopes;
DROP TABLE movie_version_clock;
