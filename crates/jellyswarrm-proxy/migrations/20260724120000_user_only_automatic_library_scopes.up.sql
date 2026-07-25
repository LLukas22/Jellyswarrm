INSERT INTO automatic_library_snapshots (
    automatic_virtual_id,
    access_scope_key,
    updated_at
)
SELECT
    automatic_virtual_id,
    CASE
        WHEN INSTR(access_scope_key, ':') > 0
            THEN SUBSTR(access_scope_key, 1, INSTR(access_scope_key, ':') - 1)
        ELSE access_scope_key
    END,
    MAX(updated_at)
FROM automatic_library_snapshots
GROUP BY automatic_virtual_id,
    CASE
        WHEN INSTR(access_scope_key, ':') > 0
            THEN SUBSTR(access_scope_key, 1, INSTR(access_scope_key, ':') - 1)
        ELSE access_scope_key
    END
ON CONFLICT(automatic_virtual_id, access_scope_key) DO UPDATE SET
    updated_at = MAX(updated_at, excluded.updated_at);

INSERT OR IGNORE INTO automatic_library_members (
    automatic_virtual_id,
    access_scope_key,
    server_id,
    virtual_library_id
)
SELECT
    automatic_virtual_id,
    CASE
        WHEN INSTR(access_scope_key, ':') > 0
            THEN SUBSTR(access_scope_key, 1, INSTR(access_scope_key, ':') - 1)
        ELSE access_scope_key
    END,
    server_id,
    virtual_library_id
FROM automatic_library_members;

DELETE FROM automatic_library_members WHERE INSTR(access_scope_key, ':') > 0;
DELETE FROM automatic_library_snapshots WHERE INSTR(access_scope_key, ':') > 0;
