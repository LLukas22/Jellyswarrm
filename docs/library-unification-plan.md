# Library Unification — Option B: Indexed/Cached Architecture

## Executive Summary

Introduce a **local metadata index** that periodically syncs item metadata from all connected upstream Jellyfin servers into SQLite. Unified libraries are configured by admins as logical groupings of server libraries. When browsing, queries hit the local index instead of fanning out live to upstream servers, enabling fast unified views, cross-server search, and smart deduplication by provider IDs. Playback still proxies directly to the original upstream server (no media storage needed).

---

## Architecture Overview

```
┌─────────────┐     ┌──────────────────────┐     ┌─────────────────────┐
│  Jellyfin    │────▶│  Sync Engine         │────▶│  SQLite Index        │
│  Server A    │     │  (periodic polling)  │     │  (unified_items,     │
│              │     │                      │     │   unified_libraries, │
│  Jellyfin    │────▶│                      │     │   item_dedup_map)    │
│  Server B    │     └──────────────────────┘     └────────┬────────────┘
│              │                                           │
└─────────────┘                               ┌───────────▼──────────┐
                                              │  Unified Query       │
                                              │  Handler             │
                                              │  (search, browse,    │
                                              │   dedup, sort)       │
                                              └───────────┬──────────┘
                                                          │
                                              ┌───────────▼──────────┐
                                              │  Existing Proxy      │
                                              │  (playback still     │
                                              │   hits upstream)     │
                                              └──────────────────────┘
```

---

## Part 1: Database Schema Changes

### 1.1 New Migration File: `20260316000000_add_library_unification.up.sql`

Tables:
- `unified_libraries` — admin-configured library groupings with a stable virtual library id
- `unified_library_members` — maps unified library to server libraries
- `synced_items` — the core metadata index
- `synced_items_fts` — FTS5 virtual table for full-text search
- `synced_user_data` — per-user playback state
- `sync_state` — tracks sync status per server
- `dedup_groups` / `dedup_group_members` — deduplication data

Triggers: Keep FTS5 table in sync with synced_items.

---

## Part 2: New Rust Services

### 2.1 `LibrarySyncService` (`library_sync_service.rs`)

Core sync engine that:
- Runs periodic sync loop (configurable interval)
- Phase A (list sync): Fetches basic item metadata, fast, every 5 min
- Phase B (detail fetch): Fetches full metadata for new items, every 30 min
- Builds FTS5 search index
- Rebuilds dedup groups after detail fetch
- Syncs user playback data

### 2.2 `UnifiedLibraryService` (`unified_library_service.rs`)

CRUD operations for unified libraries:
- Create/update/delete unified libraries
- Add/remove member libraries
- List available libraries from all servers
- Reorder libraries

---

## Part 3: New API Handlers

- `/ui/api/unified-libraries` — Admin CRUD for unified libraries
- `/{user_id}/Views` — Modified to include unified libraries
- `/UnifiedLibraries/{library_id}/Items` — Browse unified items
- `/Items/Search` — Cross-server FTS5 search

---

## Part 4: Deduplication

- Canonical key: `tmdb:{id}` > `imdb:{id}` > `tvdb:{id}`
- Quality score: bitrate + resolution + server priority
- Policies: ShowAll / PreferHighestQuality / PreferServerPriority

---

## Part 5: Implementation Sequence

1. DB migration
2. SyncedItem + UnifiedLibrary model structs
3. UnifiedLibraryService (CRUD)
4. LibrarySyncService — list sync (Phase A)
5. LibrarySyncService — detail fetch (Phase B)
6. FTS5 search integration
7. Dedup group rebuild
8. User data sync
9. Unified browsing handler
10. Search handler
11. View merging
12. Server resolution updates
13. Admin UI
14. Config + startup integration

---

## Part 6: Key Risks

| Risk | Mitigation |
|------|------------|
| Large libraries cause slow initial sync | Two-phase sync; paginate; show progress in UI |
| Stale cache showing deleted items | Generation-based cleanup; daily full sync |
| Dedup false positives | Exact provider ID match only; admin manual split |
| FTS5 index bloat | External content mode; index only searchable fields |
| Memory pressure from caches | moka max_capacity limits; SQLite as source of truth |
