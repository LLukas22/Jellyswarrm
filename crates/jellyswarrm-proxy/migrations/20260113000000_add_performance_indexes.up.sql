-- Add performance indexes for frequently queried fields

-- Indexes for authorization_sessions table
CREATE INDEX IF NOT EXISTS idx_authorization_sessions_device_id
    ON authorization_sessions(device_id);

CREATE INDEX IF NOT EXISTS idx_authorization_sessions_client
    ON authorization_sessions(client);

CREATE INDEX IF NOT EXISTS idx_authorization_sessions_expires_at
    ON authorization_sessions(expires_at)
    WHERE expires_at IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_authorization_sessions_user_id_only
    ON authorization_sessions(user_id);

-- Composite index for common query patterns
CREATE INDEX IF NOT EXISTS idx_authorization_sessions_user_device
    ON authorization_sessions(user_id, device_id, client);

-- Index for servers priority ordering
CREATE INDEX IF NOT EXISTS idx_servers_priority
    ON servers(priority DESC, name ASC);

-- Index for server_mappings user lookup
CREATE INDEX IF NOT EXISTS idx_server_mappings_user_id
    ON server_mappings(user_id);
