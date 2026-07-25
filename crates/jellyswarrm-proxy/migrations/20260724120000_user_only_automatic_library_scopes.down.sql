-- User-only scopes cannot be split back into their original server-set scopes.
-- Clear them so an older binary can rebuild safe scoped snapshots.
DELETE FROM automatic_library_members WHERE INSTR(access_scope_key, ':') = 0;
DELETE FROM automatic_library_snapshots WHERE INSTR(access_scope_key, ':') = 0;
