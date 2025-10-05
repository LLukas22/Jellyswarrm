-- Add up migration script here
-- Create a new SQLite database schema for Jellyswarrm Proxy
-- We check if tables already exist since sqlx migrations were added later and we want to support existing databases

-- Create the media_mappings table
CREATE TABLE IF NOT EXISTS media_mappings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                virtual_media_id TEXT NOT NULL UNIQUE,
                original_media_id TEXT NOT NULL,
                server_url TEXT NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(original_media_id, server_url)
            );

CREATE INDEX IF NOT EXISTS idx_media_mappings_virtual_id ON media_mappings(virtual_media_id);

CREATE INDEX IF NOT EXISTS idx_media_mappings_original_server ON media_mappings(original_media_id, server_url);


-- Create the servers table
CREATE TABLE IF NOT EXISTS servers (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                url TEXT NOT NULL,
                priority INTEGER NOT NULL DEFAULT 100,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );


-- Create users table
CREATE TABLE IF NOT EXISTS users (
                id TEXT PRIMARY KEY,
                virtual_key TEXT NOT NULL UNIQUE,
                original_username TEXT NOT NULL,
                original_password_hash TEXT NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(original_username, original_password_hash)
            );

-- Server mappings table
CREATE TABLE IF NOT EXISTS server_mappings (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id TEXT NOT NULL,
                server_url TEXT NOT NULL,
                mapped_username TEXT NOT NULL,
                mapped_password TEXT NOT NULL,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE,
                UNIQUE(user_id, server_url)
            );


-- Authorization sessions table
CREATE TABLE IF NOT EXISTS authorization_sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                user_id TEXT NOT NULL,
                mapping_id INTEGER NOT NULL,
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
                FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE,
                FOREIGN KEY (mapping_id) REFERENCES server_mappings (id) ON DELETE CASCADE,
                UNIQUE(user_id, mapping_id, device_id)
            );


CREATE INDEX IF NOT EXISTS idx_authorization_sessions_mapping 
            ON authorization_sessions(mapping_id);

CREATE INDEX IF NOT EXISTS idx_users_virtual_key 
            ON users(virtual_key);

CREATE INDEX IF NOT EXISTS idx_server_mappings_user_server 
            ON server_mappings(user_id, server_url);

CREATE INDEX IF NOT EXISTS idx_authorization_sessions_user_server 
            ON authorization_sessions(user_id, server_url);