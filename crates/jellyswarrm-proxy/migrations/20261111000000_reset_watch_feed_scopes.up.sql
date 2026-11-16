-- Continue Watching once listed every video of every library when merging
-- was enabled, and feed scopes were never pruned. Every Continue Watching
-- and Next Up load then regrouped that whole accumulated scope while holding
-- the reconciliation lock, delaying every other home-page row behind it.
-- These feeds are rebuilt from the next request; only their merged entry IDs
-- change once. Groups, members, aliases, sightings and routes cascade.
DELETE FROM movie_catalog_scopes
WHERE scope_key LIKE 'resume:%' OR scope_key LIKE 'nextup:%';
