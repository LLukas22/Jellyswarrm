ALTER TABLE server_mappings ADD COLUMN auth_method TEXT NOT NULL DEFAULT 'password' CHECK (auth_method IN ('password', 'quick_connect'));
ALTER TABLE server_mappings ADD COLUMN backend_user_id TEXT;
ALTER TABLE server_mappings ADD COLUMN encrypted_token TEXT;
