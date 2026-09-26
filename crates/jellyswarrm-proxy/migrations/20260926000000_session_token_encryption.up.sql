ALTER TABLE authorization_sessions ADD COLUMN token_format TEXT NOT NULL DEFAULT 'legacy'
    CHECK (token_format IN ('legacy', 'session_v1'));
