ALTER TABLE library_groups
ADD COLUMN duplicate_policy TEXT NOT NULL DEFAULT 'ShowAll'
CHECK (duplicate_policy IN (
    'ShowAll',
    'LargestSize',
    'SmallestSize',
    'BestQuality',
    'LowestQuality',
    'PreferServer',
    'ServerPriority'
));

ALTER TABLE library_groups
ADD COLUMN preferred_server_id INTEGER REFERENCES servers(id) ON DELETE SET NULL;
