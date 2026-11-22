CREATE TABLE client_logins (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash BLOB UNIQUE,
    api_key_id INTEGER UNIQUE REFERENCES virtual_user_api_keys(id) ON DELETE CASCADE,
    CHECK ((token_hash IS NULL) <> (api_key_id IS NULL)),
    UNIQUE (user_id, id)
);

-- Existing rows retain their IDs, encrypted tokens and device-based uniqueness.
CREATE TABLE authorization_sessions_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    mapping_id INTEGER NOT NULL REFERENCES server_mappings(id) ON DELETE CASCADE,
    server_url TEXT NOT NULL,
    client TEXT NOT NULL,
    device TEXT NOT NULL,
    device_id TEXT NOT NULL,
    version TEXT NOT NULL,
    jellyfin_token TEXT,
    original_user_id TEXT NOT NULL,
    expires_at TIMESTAMP,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    token_format TEXT NOT NULL DEFAULT 'legacy' CHECK (token_format IN ('legacy', 'session_v1')),
    login_id INTEGER,
    upstream_device_id TEXT,
    FOREIGN KEY (user_id, login_id) REFERENCES client_logins(user_id, id) ON DELETE CASCADE,
    UNIQUE (login_id, mapping_id)
);
INSERT INTO authorization_sessions_new
    (id, user_id, mapping_id, server_url, client, device, device_id, version, jellyfin_token,
     original_user_id, expires_at, created_at, updated_at, token_format)
SELECT id, user_id, mapping_id, server_url, client, device, device_id, version, jellyfin_token,
       original_user_id, expires_at, created_at, updated_at, token_format
FROM authorization_sessions;
DROP TABLE authorization_sessions;
ALTER TABLE authorization_sessions_new RENAME TO authorization_sessions;
CREATE UNIQUE INDEX legacy_device_sessions ON authorization_sessions(user_id, mapping_id, device_id)
    WHERE login_id IS NULL;
CREATE INDEX idx_authorization_sessions_mapping ON authorization_sessions(mapping_id);
CREATE INDEX idx_authorization_sessions_user_server ON authorization_sessions(user_id, server_url);
