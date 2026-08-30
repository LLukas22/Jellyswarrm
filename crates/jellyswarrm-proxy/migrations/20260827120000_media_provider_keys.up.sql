ALTER TABLE media_mappings
ADD COLUMN provider_key TEXT;

CREATE INDEX idx_media_mappings_provider_key ON media_mappings (provider_key);
