CREATE TEMP TABLE user_id_merge_map AS
WITH ranked_users AS (
    SELECT
        id,
        lower(trim(original_username)) AS username_key,
        ROW_NUMBER() OVER (
            PARTITION BY lower(trim(original_username))
            ORDER BY created_at ASC, id ASC
        ) AS rank_in_group
    FROM users
), canonical_users AS (
    SELECT username_key, id AS canonical_user_id
    FROM ranked_users
    WHERE rank_in_group = 1
)
SELECT
    ranked_users.id AS old_user_id,
    canonical_users.canonical_user_id AS new_user_id
FROM ranked_users
JOIN canonical_users USING (username_key);

DELETE FROM server_mappings AS duplicate_mapping
WHERE duplicate_mapping.user_id IN (
    SELECT old_user_id
    FROM user_id_merge_map
    WHERE old_user_id != new_user_id
)
AND EXISTS (
    SELECT 1
    FROM user_id_merge_map
    JOIN server_mappings AS canonical_mapping
        ON canonical_mapping.user_id = user_id_merge_map.new_user_id
        AND RTRIM(canonical_mapping.server_url, '/') = RTRIM(duplicate_mapping.server_url, '/')
    WHERE user_id_merge_map.old_user_id = duplicate_mapping.user_id
);

UPDATE server_mappings
SET user_id = (
    SELECT new_user_id
    FROM user_id_merge_map
    WHERE old_user_id = server_mappings.user_id
)
WHERE user_id IN (
    SELECT old_user_id
    FROM user_id_merge_map
    WHERE old_user_id != new_user_id
);

UPDATE authorization_sessions
SET user_id = (
    SELECT new_user_id
    FROM user_id_merge_map
    WHERE old_user_id = authorization_sessions.user_id
)
WHERE user_id IN (
    SELECT old_user_id
    FROM user_id_merge_map
    WHERE old_user_id != new_user_id
);

DELETE FROM users
WHERE id IN (
    SELECT old_user_id
    FROM user_id_merge_map
    WHERE old_user_id != new_user_id
);

DROP TABLE user_id_merge_map;

CREATE UNIQUE INDEX IF NOT EXISTS idx_users_username_unique_ci
    ON users(lower(trim(original_username)));
