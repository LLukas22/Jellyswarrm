UPDATE library_groups
SET duplicate_policy = 'ServerPriority'
WHERE duplicate_policy = 'ShowAll';
