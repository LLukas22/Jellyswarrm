-- Invalidate player credentials once. Preserve named integration credentials.
CREATE TABLE pending_upstream_logouts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    server_url TEXT NOT NULL,
    encrypted_token TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
INSERT INTO pending_upstream_logouts(server_url, encrypted_token)
SELECT DISTINCT a.server_url, a.jellyfin_token FROM authorization_sessions a
JOIN server_mappings m ON m.id = a.mapping_id
WHERE m.auth_method = 'password' AND m.encrypted_token IS NULL
  AND a.token_format = 'session_v1' AND a.jellyfin_token IS NOT NULL;

-- Old installations may have never run the startup encryption pass. The
-- session secret is available to Rust at startup, not to SQL migrations.
CREATE TABLE retiring_plaintext_sessions(server_url TEXT NOT NULL, token TEXT NOT NULL);
INSERT INTO retiring_plaintext_sessions SELECT DISTINCT a.server_url, a.jellyfin_token
FROM authorization_sessions a JOIN server_mappings m ON m.id = a.mapping_id
WHERE a.token_format = 'legacy' AND a.jellyfin_token IS NOT NULL
  AND m.auth_method = 'password' AND m.encrypted_token IS NULL;
CREATE TABLE IF NOT EXISTS retiring_duplicate_sessions(server_url TEXT NOT NULL, token TEXT NOT NULL, format TEXT NOT NULL);
INSERT INTO retiring_plaintext_sessions SELECT server_url, token FROM retiring_duplicate_sessions WHERE format = 'legacy'
EXCEPT SELECT server_url, token FROM retiring_plaintext_sessions;
INSERT INTO pending_upstream_logouts(server_url, encrypted_token)
SELECT server_url, token FROM retiring_duplicate_sessions WHERE format = 'session_v1'
EXCEPT SELECT server_url, encrypted_token FROM pending_upstream_logouts;
DROP TABLE retiring_duplicate_sessions;
DROP TABLE authorization_sessions;
DROP TABLE client_logins;

CREATE UNIQUE INDEX mapping_owner_id ON server_mappings(user_id, id);
CREATE UNIQUE INDEX key_owner_id ON virtual_user_api_keys(user_id, id);
CREATE TABLE client_logins (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_hash BLOB UNIQUE,
    api_key_id INTEGER UNIQUE,
    client TEXT NOT NULL,
    device TEXT NOT NULL,
    device_id TEXT NOT NULL,
    version TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    last_used_at INTEGER NOT NULL DEFAULT (unixepoch()),
    FOREIGN KEY (user_id, api_key_id) REFERENCES virtual_user_api_keys(user_id, id) ON DELETE CASCADE,
    CHECK ((token_hash IS NULL) <> (api_key_id IS NULL)),
    UNIQUE (user_id, id)
);
CREATE INDEX login_device ON client_logins(user_id, client, device_id) WHERE token_hash IS NOT NULL;
CREATE TRIGGER bind_new_api_key AFTER INSERT ON virtual_user_api_keys BEGIN
    INSERT INTO client_logins(user_id, api_key_id, client, device, device_id, version)
    VALUES (NEW.user_id, NEW.id, NEW.app_name, 'Integration', 'jellyswarrm-key-' || NEW.id, '1');
END;
INSERT INTO client_logins(user_id, api_key_id, client, device, device_id, version)
SELECT user_id, id, app_name, 'Integration', 'jellyswarrm-key-' || id, '1' FROM virtual_user_api_keys;

CREATE TABLE login_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id TEXT NOT NULL,
    login_id INTEGER NOT NULL,
    mapping_id INTEGER NOT NULL,
    server_url TEXT NOT NULL,
    upstream_device_id TEXT NOT NULL,
    jellyfin_token TEXT NOT NULL,
    owned BOOLEAN NOT NULL,
    original_user_id TEXT NOT NULL,
    expires_at TIMESTAMP,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (user_id, login_id) REFERENCES client_logins(user_id, id) ON DELETE CASCADE,
    FOREIGN KEY (user_id, mapping_id) REFERENCES server_mappings(user_id, id) ON DELETE CASCADE,
    UNIQUE(login_id, mapping_id)
);
CREATE INDEX session_mapping ON login_sessions(mapping_id);
CREATE TRIGGER queue_owned_logout AFTER DELETE ON login_sessions WHEN OLD.owned BEGIN
    INSERT INTO pending_upstream_logouts(server_url, encrypted_token) VALUES (OLD.server_url, OLD.jellyfin_token);
END;
