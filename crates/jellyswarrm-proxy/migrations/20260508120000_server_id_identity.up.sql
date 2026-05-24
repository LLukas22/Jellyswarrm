UPDATE servers
SET url = RTRIM(TRIM(url), '/');

DELETE FROM server_admins
WHERE id NOT IN (
    SELECT MIN(server_admins.id)
    FROM server_admins
    JOIN servers ON servers.id = server_admins.server_id
    GROUP BY servers.url
);

UPDATE server_admins
SET server_id = (
    SELECT MIN(canonical_servers.id)
    FROM servers AS canonical_servers
    JOIN servers AS current_server ON canonical_servers.url = current_server.url
    WHERE current_server.id = server_admins.server_id
);

DELETE FROM servers
WHERE id NOT IN (
    SELECT MIN(id)
    FROM servers
    GROUP BY url
);

CREATE UNIQUE INDEX idx_servers_url_unique
    ON servers(RTRIM(TRIM(url), '/'));

ALTER TABLE server_mappings ADD COLUMN server_id INTEGER REFERENCES servers(id) ON DELETE CASCADE;

UPDATE server_mappings
SET server_id = (
    SELECT servers.id
    FROM servers
    WHERE RTRIM(servers.url, '/') = RTRIM(server_mappings.server_url, '/')
    ORDER BY servers.id ASC
    LIMIT 1
)
WHERE server_id IS NULL;

DELETE FROM authorization_sessions
WHERE mapping_id IN (
    SELECT id
    FROM server_mappings
    WHERE server_id IS NULL
);

DELETE FROM server_mappings
WHERE server_id IS NULL;

DELETE FROM authorization_sessions
WHERE id IN (
    SELECT id
    FROM (
        SELECT
            auth.id,
            ROW_NUMBER() OVER (
                PARTITION BY auth.user_id, mapping.user_id, mapping.server_id, auth.device_id
                ORDER BY auth.updated_at DESC, auth.created_at DESC, auth.id DESC
            ) AS duplicate_rank
        FROM authorization_sessions AS auth
        JOIN server_mappings AS mapping
            ON mapping.id = auth.mapping_id
    ) AS ranked_sessions
    WHERE duplicate_rank > 1
);

UPDATE authorization_sessions
SET mapping_id = (
    SELECT MIN(canonical_mapping.id)
    FROM server_mappings AS current_mapping
    JOIN server_mappings AS canonical_mapping
        ON canonical_mapping.user_id = current_mapping.user_id
        AND canonical_mapping.server_id = current_mapping.server_id
    WHERE current_mapping.id = authorization_sessions.mapping_id
)
WHERE mapping_id IN (
    SELECT id
    FROM server_mappings
);

DELETE FROM server_mappings
WHERE id NOT IN (
    SELECT MIN(id)
    FROM server_mappings
    GROUP BY user_id, server_id
);

CREATE UNIQUE INDEX idx_server_mappings_user_server_id
    ON server_mappings(user_id, server_id);

CREATE INDEX idx_server_mappings_server_id
    ON server_mappings(server_id);

ALTER TABLE media_mappings ADD COLUMN server_id INTEGER REFERENCES servers(id) ON DELETE CASCADE;

UPDATE media_mappings
SET server_id = (
    SELECT servers.id
    FROM servers
    WHERE RTRIM(servers.url, '/') = RTRIM(media_mappings.server_url, '/')
    ORDER BY servers.id ASC
    LIMIT 1
)
WHERE server_id IS NULL;

DELETE FROM media_mappings
WHERE server_id IS NULL;

DELETE FROM media_mappings
WHERE id NOT IN (
    SELECT MIN(id)
    FROM media_mappings
    GROUP BY original_media_id, server_id
);

CREATE UNIQUE INDEX idx_media_mappings_original_server_id
    ON media_mappings(original_media_id, server_id);

CREATE INDEX idx_media_mappings_server_id
    ON media_mappings(server_id);
