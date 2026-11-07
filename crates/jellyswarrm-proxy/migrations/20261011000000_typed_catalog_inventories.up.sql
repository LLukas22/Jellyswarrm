-- Keep published group IDs and sightings while giving each media type its own
-- inventory watermark. Movie-only scans must not remove series or their children.
ALTER TABLE movie_version_members ADD COLUMN media_kind TEXT;

UPDATE movie_version_members
SET media_kind = (
    SELECT CASE
        WHEN instr(alias.provider, ':') = 0 THEN 'movie'
        ELSE substr(alias.provider, instr(alias.provider, ':') + 1)
    END
    FROM movie_version_aliases alias
    WHERE alias.scope_id = movie_version_members.scope_id
      AND alias.media_mapping_id = movie_version_members.media_mapping_id
    LIMIT 1
);

CREATE TABLE movie_catalog_type_generations (
    source_id INTEGER NOT NULL,
    media_kind TEXT NOT NULL,
    committed_generation INTEGER NOT NULL,
    PRIMARY KEY (source_id, media_kind),
    FOREIGN KEY (source_id) REFERENCES movie_catalog_sources(id) ON DELETE CASCADE
);
