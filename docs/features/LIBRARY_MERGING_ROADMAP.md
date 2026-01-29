# Library Merging Feature - Development Roadmap

## Overview

This document outlines the development plan for implementing library merging functionality in Jellyswarrm, allowing users to combine specific libraries from multiple Jellyfin servers into unified virtual libraries.

## Goals

1. Allow users to create "merged libraries" that combine content from specific source libraries across servers
2. Provide deduplication to show single entries for content available on multiple servers
3. Maintain full playback functionality with server selection options
4. Integrate seamlessly with existing Jellyfin client apps

---

## Phase 1: Foundation (Week 1)

### 1.1 Development Environment Setup
- [x] Fork Jellyswarrm repository
- [x] Create feature branch `feature/library-merging`
- [x] Set up separate dev instance with isolated data directory
- [x] Configure debug logging
- [ ] Verify dev instance connects to test servers

### 1.2 Database Schema Design
- [x] Design `merged_libraries` table schema
- [x] Design `merged_library_sources` table schema
- [x] Create migration scripts
- [x] Add schema to `server_storage.rs` initialization

### 1.3 Core Data Structures
- [x] Define `MergedLibrary` struct in models
- [x] Define `MergedLibrarySource` struct
- [x] Define `MergedLibraryConfig` for API responses
- [x] Add serialization/deserialization

**Deliverables:**
- Database migrations
- Core Rust structs
- Unit tests for data layer

---

## Phase 2: Storage Layer (Week 1-2)

### 2.1 Merged Library Storage Service
- [x] Create `merged_library_storage.rs` module
- [x] Implement CRUD operations:
  - `create_merged_library()`
  - `get_merged_library(id)`
  - `list_merged_libraries(user_id)`
  - `update_merged_library()`
  - `delete_merged_library()`
- [x] Implement source management:
  - `add_source_to_merged_library()`
  - `remove_source_from_merged_library()`
  - `get_sources_for_merged_library()`

### 2.2 Caching Layer
- [ ] Add cache for merged library configurations
- [ ] Implement cache invalidation on updates

**Deliverables:**
- `merged_library_storage.rs` with full CRUD
- Integration tests

---

## Phase 3: API Endpoints (Week 2)

### 3.1 Admin API Endpoints
- [x] `POST /ui/admin/merged-libraries` - Create merged library
- [x] `GET /ui/admin/merged-libraries` - List all merged libraries (HTML page)
- [x] `GET /ui/admin/merged-libraries/list` - Get merged library list partial (HTMX)
- [x] `GET /ui/admin/merged-libraries/json` - List all merged libraries (JSON API)
- [x] `GET /ui/admin/merged-libraries/{id}/json` - Get merged library details (JSON API)
- [ ] `PUT /ui/admin/merged-libraries/{id}` - Update merged library
- [x] `DELETE /ui/admin/merged-libraries/{id}` - Delete merged library
- [x] `POST /ui/admin/merged-libraries/{id}/sources` - Add source
- [x] `DELETE /ui/admin/merged-libraries/{library_id}/sources/{source_id}` - Remove source

### 3.2 User-Facing API Integration
- [ ] Modify `/Users/{userId}/Views` to include merged libraries
- [ ] Create handler for merged library content retrieval
- [ ] Implement virtual library ID generation

**Deliverables:**
- REST API endpoints
- OpenAPI documentation
- API integration tests

---

## Phase 4: Federated Query Engine (Week 2-3)

### 4.1 Merged Library Query Handler
- [x] Create `handlers/merged_libraries.rs`
- [x] Implement `get_merged_library_items()`:
  - Fetch source library configurations
  - Query only specified libraries from each server
  - Aggregate results
- [ ] Add pagination support
- [ ] Add sorting support
- [ ] Add filtering support

### 4.2 Deduplication Engine
- [x] Create `deduplication.rs` module
- [x] Implement matching strategies:
  - Provider ID matching (TMDB, IMDB, TVDB)
  - Name + Year fallback matching
  - (Fuzzy matching - future enhancement)
- [x] Implement merge strategies:
  - Keep all versions (show server badges) via `None` strategy
  - Prefer specific server via priority ordering
  - (Highest quality preference - future enhancement)
- [x] Track duplicate sources for playback selection

**Deliverables:**
- Merged library query handler
- Deduplication engine with configurable strategies
- Performance benchmarks

---

## Phase 5: Admin UI (Week 3)

### 5.1 Merged Libraries Management Page
- [x] Create `ui/admin/merged_libraries.rs` handler
- [x] Create `templates/admin/merged_libraries.html`
- [ ] Implement list view with create/edit/delete
- [ ] Add source library selector (dropdown per server)

### 5.2 Source Configuration UI
- [ ] Display available libraries per server
- [ ] Drag-and-drop or checkbox selection
- [ ] Priority ordering for deduplication
- [ ] Preview merged library contents

### 5.3 User Preferences (Optional)
- [ ] Per-user merged library visibility settings
- [ ] Per-user deduplication preferences

**Deliverables:**
- Admin UI pages
- User-facing configuration options
- UI tests

---

## Phase 6: Client Integration (Week 3-4)

### 6.1 Jellyfin API Compatibility
- [ ] Ensure merged libraries appear as CollectionFolders
- [ ] Test with Jellyfin Web client
- [ ] Test with Jellyfin Android app
- [ ] Test with Jellyfin iOS app
- [ ] Test with Jellyfin TV apps

### 6.2 Playback Integration
- [ ] Handle playback requests for deduplicated items
- [ ] Implement server selection logic
- [ ] Add "Play from..." option for multi-server items

**Deliverables:**
- Client compatibility matrix
- Playback routing tests
- End-to-end tests

---

## Phase 7: Testing & Documentation (Week 4)

### 7.1 Testing
- [ ] Unit tests for all new modules
- [ ] Integration tests for API endpoints
- [ ] End-to-end tests with multiple servers
- [ ] Performance testing with large libraries
- [ ] Edge case testing (offline servers, auth failures)

### 7.2 Documentation
- [ ] Update README with feature description
- [ ] Add configuration documentation
- [ ] Create user guide for merged libraries
- [ ] Add API documentation

### 7.3 Code Review & Cleanup
- [ ] Code review checklist
- [ ] Remove debug code
- [ ] Optimize performance bottlenecks
- [ ] Ensure error handling is comprehensive

**Deliverables:**
- Test suite with >80% coverage
- Complete documentation
- PR ready for review

---

## Phase 8: Release (Week 4+)

### 8.1 Pre-Release
- [ ] Create PR to upstream repository
- [ ] Address review feedback
- [ ] Beta testing with community volunteers

### 8.2 Release
- [ ] Merge to main branch
- [ ] Update changelog
- [ ] Release notes

---

## Technical Specifications

### Database Schema

```sql
-- Merged library definitions
CREATE TABLE merged_libraries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    virtual_id TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    collection_type TEXT NOT NULL,
    dedup_strategy TEXT DEFAULT 'provider_ids',
    created_by TEXT,
    is_global BOOLEAN DEFAULT FALSE,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

-- Source libraries for each merged library
CREATE TABLE merged_library_sources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    merged_library_id INTEGER NOT NULL,
    server_id INTEGER NOT NULL,
    library_id TEXT NOT NULL,
    library_name TEXT,
    priority INTEGER DEFAULT 0,
    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (merged_library_id) REFERENCES merged_libraries(id) ON DELETE CASCADE,
    FOREIGN KEY (server_id) REFERENCES servers(id) ON DELETE CASCADE,
    UNIQUE(merged_library_id, server_id, library_id)
);

-- Index for faster lookups
CREATE INDEX idx_merged_sources_library ON merged_library_sources(merged_library_id);
CREATE INDEX idx_merged_sources_server ON merged_library_sources(server_id);
```

### API Endpoints Summary

| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/ui/admin/merged-libraries` | List merged libraries |
| POST | `/ui/admin/merged-libraries` | Create merged library |
| GET | `/ui/admin/merged-libraries/{id}` | Get merged library |
| PUT | `/ui/admin/merged-libraries/{id}` | Update merged library |
| DELETE | `/ui/admin/merged-libraries/{id}` | Delete merged library |
| GET | `/Items?ParentId={merged_virtual_id}` | Get merged library items |

### Deduplication Strategies

| Strategy | Description | Use Case |
|----------|-------------|----------|
| `provider_ids` | Match by TMDB/IMDB/TVDB IDs | Most accurate, requires metadata |
| `name_year` | Match by title + release year | Fallback when IDs unavailable |
| `none` | No deduplication | Show all copies |

---

## Risk Assessment

| Risk | Impact | Mitigation |
|------|--------|------------|
| Performance with large libraries | High | Implement caching, pagination |
| Deduplication false positives | Medium | Allow manual override, fuzzy threshold config |
| Client compatibility issues | Medium | Extensive testing, fallback modes |
| Database migration issues | Low | Backup procedures, rollback scripts |

---

## Success Criteria

1. Users can create merged libraries via Admin UI
2. Merged libraries appear in Jellyfin clients as regular libraries
3. Deduplication correctly identifies same content across servers
4. Playback works seamlessly with server selection
5. Performance remains acceptable (<2s library load time)
6. All existing functionality continues to work

---

## Timeline Summary

| Phase | Duration | Dependencies |
|-------|----------|--------------|
| Phase 1: Foundation | 3-4 days | None |
| Phase 2: Storage Layer | 2-3 days | Phase 1 |
| Phase 3: API Endpoints | 2-3 days | Phase 2 |
| Phase 4: Query Engine | 4-5 days | Phase 3 |
| Phase 5: Admin UI | 3-4 days | Phase 3 |
| Phase 6: Client Integration | 3-4 days | Phase 4, 5 |
| Phase 7: Testing & Docs | 3-4 days | Phase 6 |
| Phase 8: Release | 2-3 days | Phase 7 |

**Total Estimated Time: 3-4 weeks**
