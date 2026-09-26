ALTER TABLE server_mappings ADD COLUMN credential_format TEXT NOT NULL DEFAULT 'legacy'
    CHECK (credential_format IN ('legacy', 'session_v1'));
