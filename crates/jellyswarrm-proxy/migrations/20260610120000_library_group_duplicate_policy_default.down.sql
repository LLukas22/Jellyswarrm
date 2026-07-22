UPDATE library_groups
SET duplicate_policy = 'ShowAll'
WHERE duplicate_policy = 'ServerPriority';
