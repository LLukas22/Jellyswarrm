ALTER TABLE servers
ADD COLUMN media_streaming_mode TEXT NOT NULL DEFAULT 'Redirect'
CHECK (media_streaming_mode IN ('Redirect', 'Proxy'));
