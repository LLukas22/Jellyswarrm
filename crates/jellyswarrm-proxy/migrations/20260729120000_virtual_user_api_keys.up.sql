CREATE TABLE virtual_user_api_keys (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    app_name TEXT NOT NULL CHECK (length(trim(app_name)) BETWEEN 1 AND 128),
    access_token TEXT NOT NULL UNIQUE,
    created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_virtual_user_api_keys_user
    ON virtual_user_api_keys(user_id, id);
