-- Successful viewer inventories constrain configured-library routing as well
-- as presentation. Missing/failed inventories never revoke existing sightings.
CREATE TABLE library_viewer_inventories (
    user_id TEXT NOT NULL,
    server_id INTEGER NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL,
    PRIMARY KEY (user_id, server_id)
);
CREATE TABLE library_viewer_members (
    user_id TEXT NOT NULL,
    server_id INTEGER NOT NULL,
    original_library_id TEXT NOT NULL,
    collection_type TEXT NOT NULL,
    PRIMARY KEY (user_id, server_id, original_library_id),
    FOREIGN KEY (user_id, server_id) REFERENCES library_viewer_inventories(user_id, server_id) ON DELETE CASCADE
);

-- Covers explicit removals and server-deletion cascades alike.
CREATE TRIGGER reset_empty_library_group_type AFTER DELETE ON library_group_members
BEGIN
    UPDATE library_groups SET collection_type = NULL
    WHERE virtual_id = OLD.group_virtual_id
      AND NOT EXISTS (SELECT 1 FROM library_group_members WHERE group_virtual_id = OLD.group_virtual_id);
END;
UPDATE library_groups SET collection_type = NULL
WHERE NOT EXISTS (SELECT 1 FROM library_group_members WHERE group_virtual_id = library_groups.virtual_id);
