-- Create server_admins table
CREATE TABLE IF NOT EXISTS server_admins (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    server_id INTEGER NOT NULL,
    username TEXT NOT NULL,
    password TEXT NOT NULL,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (server_id) REFERENCES servers (id) ON DELETE CASCADE,
    UNIQUE(server_id)
);

CREATE INDEX IF NOT EXISTS idx_server_admins_server_id ON server_admins(server_id);
