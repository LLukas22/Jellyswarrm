-- Encrypted per-user values owned by compile-time plugins (e.g. Moonfin's
-- Seerr session). Removed together with the user.
CREATE TABLE plugin_user_secrets (
    plugin_id  TEXT NOT NULL,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL,
    PRIMARY KEY (plugin_id, user_id, key)
);
