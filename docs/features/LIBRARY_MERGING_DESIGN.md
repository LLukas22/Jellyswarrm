# Library Merging Feature - Technical Design Document

## 1. Introduction

### 1.1 Purpose
This document provides detailed technical specifications for implementing library merging in Jellyswarrm, enabling users to combine specific libraries from multiple Jellyfin servers into unified virtual libraries.

### 1.2 Scope
- Database schema design
- Rust module architecture
- API contract definitions
- Deduplication algorithms
- UI specifications

### 1.3 References
- [Jellyswarrm Architecture](../architecture.md)
- [Jellyfin API Documentation](https://api.jellyfin.org/)
- [Roadmap](./LIBRARY_MERGING_ROADMAP.md)

---

## 2. System Architecture

### 2.1 High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        Jellyfin Clients                          │
│              (Web, Android, iOS, TV Apps)                        │
└─────────────────────────┬───────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────────────┐
│                      Jellyswarrm Proxy                           │
│  ┌─────────────────────────────────────────────────────────┐    │
│  │                    Request Router                        │    │
│  │  - Detects merged library requests                       │    │
│  │  - Routes to appropriate handler                         │    │
│  └─────────────────────────┬───────────────────────────────┘    │
│                            │                                     │
│            ┌───────────────┼───────────────┐                    │
│            ▼               ▼               ▼                    │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────┐         │
│  │  Standard   │  │   Merged    │  │    Admin        │         │
│  │  Federated  │  │   Library   │  │    API          │         │
│  │  Handler    │  │   Handler   │  │    Handler      │         │
│  └─────────────┘  └──────┬──────┘  └─────────────────┘         │
│                          │                                       │
│                          ▼                                       │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │              Merged Library Service                       │   │
│  │  - Configuration management                               │   │
│  │  - Source library resolution                              │   │
│  │  - Query orchestration                                    │   │
│  └──────────────────────────┬───────────────────────────────┘   │
│                             │                                    │
│            ┌────────────────┼────────────────┐                  │
│            ▼                ▼                ▼                  │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────┐         │
│  │   Storage   │  │    Dedup    │  │     Cache       │         │
│  │   Layer     │  │    Engine   │  │     Layer       │         │
│  └─────────────┘  └─────────────┘  └─────────────────┘         │
│                                                                  │
└──────────────────────────────┬──────────────────────────────────┘
                               │
         ┌─────────────────────┼─────────────────────┐
         ▼                     ▼                     ▼
┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐
│  Jellyfin       │  │  Jellyfin       │  │  Jellyfin       │
│  Server A       │  │  Server B       │  │  Server C       │
│  - Movies       │  │  - Movies       │  │  - Films        │
│  - TV Shows     │  │  - Series       │  │  - Documentaries│
└─────────────────┘  └─────────────────┘  └─────────────────┘
```

### 2.2 Module Structure

```
crates/jellyswarrm-proxy/src/
├── merged_libraries/
│   ├── mod.rs                 # Module exports
│   ├── storage.rs             # Database operations
│   ├── service.rs             # Business logic
│   ├── deduplication.rs       # Dedup algorithms
│   └── models.rs              # Data structures
├── handlers/
│   └── merged_libraries.rs    # HTTP handlers
├── ui/
│   └── admin/
│       └── merged_libraries.rs # Admin UI handlers
└── templates/
    └── admin/
        ├── merged_libraries.html
        └── merged_library_edit.html
```

---

## 3. Data Models

### 3.1 Rust Structures

```rust
// merged_libraries/models.rs

use serde::{Deserialize, Serialize};
use sqlx::FromRow;

/// Deduplication strategy for merged libraries
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum DeduplicationStrategy {
    /// Match by provider IDs (TMDB, IMDB, TVDB)
    ProviderIds,
    /// Match by name and year
    NameYear,
    /// No deduplication - show all copies
    None,
}

impl Default for DeduplicationStrategy {
    fn default() -> Self {
        Self::ProviderIds
    }
}

/// A merged library configuration
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct MergedLibrary {
    pub id: i64,
    pub virtual_id: String,
    pub name: String,
    pub collection_type: String,
    pub dedup_strategy: String,
    pub created_by: Option<String>,
    pub is_global: bool,
    pub created_at: chrono::NaiveDateTime,
    pub updated_at: chrono::NaiveDateTime,
}

/// A source library within a merged library
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct MergedLibrarySource {
    pub id: i64,
    pub merged_library_id: i64,
    pub server_id: i64,
    pub library_id: String,
    pub library_name: Option<String>,
    pub priority: i32,
    pub created_at: chrono::NaiveDateTime,
}

/// Full merged library with sources (for API responses)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedLibraryWithSources {
    #[serde(flatten)]
    pub library: MergedLibrary,
    pub sources: Vec<MergedLibrarySourceWithServer>,
}

/// Source with server information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergedLibrarySourceWithServer {
    #[serde(flatten)]
    pub source: MergedLibrarySource,
    pub server_name: String,
    pub server_url: String,
}

/// Request to create a merged library
#[derive(Debug, Deserialize)]
pub struct CreateMergedLibraryRequest {
    pub name: String,
    pub collection_type: String,
    #[serde(default)]
    pub dedup_strategy: DeduplicationStrategy,
    #[serde(default)]
    pub is_global: bool,
    #[serde(default)]
    pub sources: Vec<CreateSourceRequest>,
}

/// Request to add a source
#[derive(Debug, Deserialize)]
pub struct CreateSourceRequest {
    pub server_id: i64,
    pub library_id: String,
    pub library_name: Option<String>,
    #[serde(default)]
    pub priority: i32,
}

/// A deduplicated media item with source information
#[derive(Debug, Clone, Serialize)]
pub struct DeduplicatedItem {
    /// The canonical item (from highest priority source)
    pub item: crate::models::MediaItem,
    /// All sources where this item is available
    pub sources: Vec<ItemSource>,
    /// Whether this item was deduplicated
    pub is_deduplicated: bool,
}

/// Source information for a deduplicated item
#[derive(Debug, Clone, Serialize)]
pub struct ItemSource {
    pub server_id: i64,
    pub server_name: String,
    pub original_item_id: String,
    pub virtual_item_id: String,
    pub priority: i32,
}
```

### 3.2 Database Schema

```sql
-- migrations/003_merged_libraries.sql

-- Merged library definitions
CREATE TABLE IF NOT EXISTS merged_libraries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    virtual_id TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    collection_type TEXT NOT NULL,
    dedup_strategy TEXT NOT NULL DEFAULT 'provider_ids',
    created_by TEXT,
    is_global INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Source libraries for each merged library
CREATE TABLE IF NOT EXISTS merged_library_sources (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    merged_library_id INTEGER NOT NULL,
    server_id INTEGER NOT NULL,
    library_id TEXT NOT NULL,
    library_name TEXT,
    priority INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    FOREIGN KEY (merged_library_id) REFERENCES merged_libraries(id) ON DELETE CASCADE,
    FOREIGN KEY (server_id) REFERENCES servers(id) ON DELETE CASCADE,
    UNIQUE(merged_library_id, server_id, library_id)
);

-- Indexes
CREATE INDEX IF NOT EXISTS idx_merged_libs_virtual_id ON merged_libraries(virtual_id);
CREATE INDEX IF NOT EXISTS idx_merged_libs_created_by ON merged_libraries(created_by);
CREATE INDEX IF NOT EXISTS idx_merged_sources_library ON merged_library_sources(merged_library_id);
CREATE INDEX IF NOT EXISTS idx_merged_sources_server ON merged_library_sources(server_id);
```

---

## 4. Storage Layer

### 4.1 Storage Service Interface

```rust
// merged_libraries/storage.rs

use anyhow::Result;
use sqlx::SqlitePool;

pub struct MergedLibraryStorage {
    pool: SqlitePool,
}

impl MergedLibraryStorage {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Initialize database tables
    pub async fn initialize(&self) -> Result<()>;

    // CRUD for merged libraries
    pub async fn create_merged_library(
        &self,
        name: &str,
        collection_type: &str,
        dedup_strategy: &str,
        created_by: Option<&str>,
        is_global: bool,
    ) -> Result<MergedLibrary>;

    pub async fn get_merged_library(&self, id: i64) -> Result<Option<MergedLibrary>>;

    pub async fn get_merged_library_by_virtual_id(
        &self,
        virtual_id: &str,
    ) -> Result<Option<MergedLibrary>>;

    pub async fn list_merged_libraries(
        &self,
        user_id: Option<&str>,
    ) -> Result<Vec<MergedLibrary>>;

    pub async fn update_merged_library(
        &self,
        id: i64,
        name: &str,
        dedup_strategy: &str,
    ) -> Result<bool>;

    pub async fn delete_merged_library(&self, id: i64) -> Result<bool>;

    // Source management
    pub async fn add_source(
        &self,
        merged_library_id: i64,
        server_id: i64,
        library_id: &str,
        library_name: Option<&str>,
        priority: i32,
    ) -> Result<MergedLibrarySource>;

    pub async fn remove_source(&self, source_id: i64) -> Result<bool>;

    pub async fn get_sources(
        &self,
        merged_library_id: i64,
    ) -> Result<Vec<MergedLibrarySource>>;

    pub async fn get_sources_with_servers(
        &self,
        merged_library_id: i64,
    ) -> Result<Vec<MergedLibrarySourceWithServer>>;
}
```

---

## 5. Deduplication Engine

### 5.1 Algorithm Overview

```
Input: Items from multiple source libraries
Output: Deduplicated items with source tracking

Algorithm:
1. Build index of all items by provider IDs
2. For items without provider IDs, index by normalized name + year
3. Group items by matching criteria
4. For each group:
   a. Select canonical item (highest priority source)
   b. Attach all sources to the canonical item
   c. Mark as deduplicated if multiple sources
5. Return merged item list
```

### 5.2 Implementation

```rust
// merged_libraries/deduplication.rs

use std::collections::HashMap;
use crate::models::MediaItem;

pub struct DeduplicationEngine {
    strategy: DeduplicationStrategy,
}

impl DeduplicationEngine {
    pub fn new(strategy: DeduplicationStrategy) -> Self {
        Self { strategy }
    }

    /// Deduplicate items from multiple sources
    pub fn deduplicate(
        &self,
        items_by_source: Vec<(ItemSource, Vec<MediaItem>)>,
    ) -> Vec<DeduplicatedItem> {
        match self.strategy {
            DeduplicationStrategy::ProviderIds => {
                self.dedupe_by_provider_ids(items_by_source)
            }
            DeduplicationStrategy::NameYear => {
                self.dedupe_by_name_year(items_by_source)
            }
            DeduplicationStrategy::None => {
                self.no_deduplication(items_by_source)
            }
        }
    }

    fn dedupe_by_provider_ids(
        &self,
        items_by_source: Vec<(ItemSource, Vec<MediaItem>)>,
    ) -> Vec<DeduplicatedItem> {
        // Index: provider_id -> (source, item)
        let mut tmdb_index: HashMap<String, Vec<(ItemSource, MediaItem)>> = HashMap::new();
        let mut imdb_index: HashMap<String, Vec<(ItemSource, MediaItem)>> = HashMap::new();
        let mut unmatched: Vec<(ItemSource, MediaItem)> = Vec::new();

        // Build indexes
        for (source, items) in items_by_source {
            for item in items {
                let mut indexed = false;

                if let Some(ref provider_ids) = item.provider_ids {
                    if let Some(tmdb_id) = provider_ids.get("Tmdb") {
                        tmdb_index.entry(tmdb_id.clone())
                            .or_default()
                            .push((source.clone(), item.clone()));
                        indexed = true;
                    }
                    if let Some(imdb_id) = provider_ids.get("Imdb") {
                        imdb_index.entry(imdb_id.clone())
                            .or_default()
                            .push((source.clone(), item.clone()));
                        indexed = true;
                    }
                }

                if !indexed {
                    unmatched.push((source.clone(), item));
                }
            }
        }

        // Merge duplicates
        let mut result: Vec<DeduplicatedItem> = Vec::new();
        let mut processed: std::collections::HashSet<String> = std::collections::HashSet::new();

        // Process TMDB matches
        for (_, group) in tmdb_index {
            if group.len() > 1 {
                result.push(self.merge_group(group));
            } else if let Some((source, item)) = group.into_iter().next() {
                if !processed.contains(&item.id) {
                    processed.insert(item.id.clone());
                    result.push(DeduplicatedItem {
                        item,
                        sources: vec![source],
                        is_deduplicated: false,
                    });
                }
            }
        }

        // Add unmatched items
        for (source, item) in unmatched {
            if !processed.contains(&item.id) {
                processed.insert(item.id.clone());
                result.push(DeduplicatedItem {
                    item,
                    sources: vec![source],
                    is_deduplicated: false,
                });
            }
        }

        result
    }

    fn merge_group(
        &self,
        mut group: Vec<(ItemSource, MediaItem)>,
    ) -> DeduplicatedItem {
        // Sort by priority (highest first)
        group.sort_by(|a, b| b.0.priority.cmp(&a.0.priority));

        let (primary_source, primary_item) = group.remove(0);
        let mut sources = vec![primary_source];

        for (source, _) in group {
            sources.push(source);
        }

        DeduplicatedItem {
            item: primary_item,
            sources,
            is_deduplicated: true,
        }
    }

    fn normalize_name(name: &str) -> String {
        name.to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric() || c.is_whitespace())
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }
}
```

---

## 6. API Handlers

### 6.1 Admin API Handlers

```rust
// handlers/merged_libraries.rs

use axum::{
    extract::{Path, State},
    Json,
};
use hyper::StatusCode;

/// List all merged libraries
pub async fn list_merged_libraries(
    State(state): State<AppState>,
) -> Result<Json<Vec<MergedLibraryWithSources>>, StatusCode> {
    let libraries = state.merged_library_storage
        .list_merged_libraries(None)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut result = Vec::new();
    for lib in libraries {
        let sources = state.merged_library_storage
            .get_sources_with_servers(lib.id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

        result.push(MergedLibraryWithSources {
            library: lib,
            sources,
        });
    }

    Ok(Json(result))
}

/// Create a new merged library
pub async fn create_merged_library(
    State(state): State<AppState>,
    Json(request): Json<CreateMergedLibraryRequest>,
) -> Result<Json<MergedLibraryWithSources>, StatusCode> {
    // Create library
    let library = state.merged_library_storage
        .create_merged_library(
            &request.name,
            &request.collection_type,
            &format!("{:?}", request.dedup_strategy).to_lowercase(),
            None,
            request.is_global,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Add sources
    for source in request.sources {
        state.merged_library_storage
            .add_source(
                library.id,
                source.server_id,
                &source.library_id,
                source.library_name.as_deref(),
                source.priority,
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }

    // Return with sources
    let sources = state.merged_library_storage
        .get_sources_with_servers(library.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(MergedLibraryWithSources { library, sources }))
}

/// Get merged library items (federated query)
pub async fn get_merged_library_items(
    State(state): State<AppState>,
    Path(virtual_id): Path<String>,
    // ... pagination params
) -> Result<Json<ItemsResponseWithCount>, StatusCode> {
    // 1. Get merged library config
    let library = state.merged_library_storage
        .get_merged_library_by_virtual_id(&virtual_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    // 2. Get sources
    let sources = state.merged_library_storage
        .get_sources_with_servers(library.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // 3. Query each source library in parallel
    let items_by_source = query_source_libraries(&state, &sources).await?;

    // 4. Deduplicate
    let strategy = DeduplicationStrategy::from_str(&library.dedup_strategy);
    let engine = DeduplicationEngine::new(strategy);
    let deduplicated = engine.deduplicate(items_by_source);

    // 5. Convert to response format
    let items: Vec<MediaItem> = deduplicated
        .into_iter()
        .map(|d| d.item)
        .collect();

    Ok(Json(ItemsResponseWithCount {
        items,
        total_record_count: items.len() as i32,
        start_index: 0,
    }))
}
```

---

## 7. User Views Integration

### 7.1 Modifying UserViews Response

To make merged libraries appear in client apps, we need to inject them into the `/Users/{userId}/Views` response:

```rust
// In handlers/federated.rs or new handler

pub async fn get_user_views_with_merged(
    State(state): State<AppState>,
    // ... existing params
) -> Result<Json<ItemsResponse>, StatusCode> {
    // 1. Get regular federated views
    let mut views = get_federated_user_views(&state).await?;

    // 2. Get merged libraries for this user
    let merged_libs = state.merged_library_storage
        .list_merged_libraries(Some(&user_id))
        .await?;

    // 3. Convert merged libraries to MediaItem format
    for lib in merged_libs {
        let view_item = MediaItem {
            id: lib.virtual_id.clone(),
            name: Some(lib.name.clone()),
            collection_type: Some(lib.collection_type.parse().unwrap_or_default()),
            item_type: BaseItemKind::CollectionFolder,
            is_folder: Some(true),
            // ... other fields
        };
        views.push(view_item);
    }

    Ok(Json(ItemsResponse { items: views }))
}
```

---

## 8. Admin UI Templates

### 8.1 Merged Libraries List

```html
<!-- templates/admin/merged_libraries.html -->
{% extends "admin/base.html" %}

{% block content %}
<div class="container">
    <h2>Merged Libraries</h2>

    <div class="card mb-4">
        <div class="card-header">
            <h5>Create New Merged Library</h5>
        </div>
        <div class="card-body">
            <form hx-post="/ui/admin/merged-libraries" hx-swap="afterbegin" hx-target="#library-list">
                <div class="row">
                    <div class="col-md-4">
                        <input type="text" name="name" class="form-control" placeholder="Library Name" required>
                    </div>
                    <div class="col-md-3">
                        <select name="collection_type" class="form-select">
                            <option value="movies">Movies</option>
                            <option value="tvshows">TV Shows</option>
                            <option value="music">Music</option>
                        </select>
                    </div>
                    <div class="col-md-3">
                        <select name="dedup_strategy" class="form-select">
                            <option value="provider_ids">By Provider IDs (Recommended)</option>
                            <option value="name_year">By Name + Year</option>
                            <option value="none">No Deduplication</option>
                        </select>
                    </div>
                    <div class="col-md-2">
                        <button type="submit" class="btn btn-primary">Create</button>
                    </div>
                </div>
            </form>
        </div>
    </div>

    <div id="library-list">
        {% for library in libraries %}
        <div class="card mb-3">
            <div class="card-header d-flex justify-content-between">
                <span>{{ library.name }} ({{ library.collection_type }})</span>
                <button class="btn btn-sm btn-danger"
                        hx-delete="/ui/admin/merged-libraries/{{ library.id }}"
                        hx-confirm="Delete this merged library?">
                    Delete
                </button>
            </div>
            <div class="card-body">
                <h6>Source Libraries:</h6>
                <ul class="list-group mb-3">
                    {% for source in library.sources %}
                    <li class="list-group-item d-flex justify-content-between">
                        <span>{{ source.server_name }}: {{ source.library_name }}</span>
                        <span class="badge bg-secondary">Priority: {{ source.priority }}</span>
                    </li>
                    {% endfor %}
                </ul>

                <!-- Add source form -->
                <form hx-post="/ui/admin/merged-libraries/{{ library.id }}/sources" hx-swap="outerHTML" hx-target="closest .card">
                    <div class="row">
                        <div class="col-md-4">
                            <select name="server_id" class="form-select" required>
                                {% for server in servers %}
                                <option value="{{ server.id }}">{{ server.name }}</option>
                                {% endfor %}
                            </select>
                        </div>
                        <div class="col-md-4">
                            <input type="text" name="library_id" class="form-control" placeholder="Library ID" required>
                        </div>
                        <div class="col-md-2">
                            <input type="number" name="priority" class="form-control" placeholder="Priority" value="0">
                        </div>
                        <div class="col-md-2">
                            <button type="submit" class="btn btn-secondary">Add Source</button>
                        </div>
                    </div>
                </form>
            </div>
        </div>
        {% endfor %}
    </div>
</div>
{% endblock %}
```

---

## 9. Testing Strategy

### 9.1 Unit Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deduplication_by_tmdb() {
        let engine = DeduplicationEngine::new(DeduplicationStrategy::ProviderIds);

        let source_a = ItemSource {
            server_id: 1,
            server_name: "Server A".to_string(),
            original_item_id: "item-a-1".to_string(),
            virtual_item_id: "v-1".to_string(),
            priority: 10,
        };

        let source_b = ItemSource {
            server_id: 2,
            server_name: "Server B".to_string(),
            original_item_id: "item-b-1".to_string(),
            virtual_item_id: "v-2".to_string(),
            priority: 5,
        };

        let item_a = MediaItem {
            id: "v-1".to_string(),
            name: Some("The Matrix".to_string()),
            provider_ids: Some(HashMap::from([
                ("Tmdb".to_string(), "603".to_string()),
            ])),
            ..Default::default()
        };

        let item_b = MediaItem {
            id: "v-2".to_string(),
            name: Some("Matrix".to_string()), // Different name
            provider_ids: Some(HashMap::from([
                ("Tmdb".to_string(), "603".to_string()), // Same TMDB ID
            ])),
            ..Default::default()
        };

        let items_by_source = vec![
            (source_a, vec![item_a]),
            (source_b, vec![item_b]),
        ];

        let result = engine.deduplicate(items_by_source);

        assert_eq!(result.len(), 1);
        assert!(result[0].is_deduplicated);
        assert_eq!(result[0].sources.len(), 2);
        assert_eq!(result[0].item.name, Some("The Matrix".to_string())); // Higher priority
    }
}
```

### 9.2 Integration Tests

```rust
#[tokio::test]
async fn test_merged_library_crud() {
    let storage = setup_test_storage().await;

    // Create
    let lib = storage.create_merged_library(
        "All Movies",
        "movies",
        "provider_ids",
        None,
        true,
    ).await.unwrap();

    assert_eq!(lib.name, "All Movies");

    // Add source
    let source = storage.add_source(
        lib.id,
        1, // server_id
        "library-123",
        Some("Movies"),
        10,
    ).await.unwrap();

    // Get with sources
    let sources = storage.get_sources(lib.id).await.unwrap();
    assert_eq!(sources.len(), 1);

    // Delete
    storage.delete_merged_library(lib.id).await.unwrap();
    assert!(storage.get_merged_library(lib.id).await.unwrap().is_none());
}
```

---

## 10. Performance Considerations

### 10.1 Caching Strategy

- Cache merged library configurations (TTL: 5 minutes)
- Cache deduplicated item lists per merged library (TTL: 1 minute)
- Invalidate cache on configuration changes

### 10.2 Query Optimization

- Parallel queries to source servers (existing pattern)
- Limit items per source before deduplication
- Stream results where possible

### 10.3 Memory Management

- Process items in batches for large libraries
- Use iterators instead of collecting all items
- Limit deduplication index size

---

## 11. Security Considerations

- Validate user has access to all source libraries
- Sanitize library names for XSS prevention
- Rate limit API endpoints
- Audit log for configuration changes

---

## 12. Future Enhancements

1. **Smart Deduplication**: Machine learning for fuzzy matching
2. **Quality Selection**: Prefer higher bitrate versions
3. **Watch Progress Sync**: Unified progress across duplicates
4. **Custom Ordering**: User-defined sort within merged libraries
5. **Filters**: Apply filters across merged content
