//! Own virtual-library dependencies and coordinate discovery, membership, and resolution operations.
use std::{collections::HashMap, sync::Arc};

use sqlx::{Sqlite, SqlitePool, Transaction};
use tokio::sync::Mutex;
use tracing::debug;
use uuid::Uuid;

use crate::{
    domain::server_id::ServerId, media::storage::MediaStorageService,
    servers::storage::ServerStorageService,
};

pub(crate) fn normalize_library_id(id: &str) -> String {
    match Uuid::parse_str(id) {
        Ok(uuid) => uuid.simple().to_string(),
        Err(_) => id.to_string(),
    }
}

pub(crate) use super::models::{
    AssignLibraryError, AutomaticVirtualLibrary, DiscoveredLibrary, LibraryAssignment,
    LibraryGroup, LibraryGroupMemberRecord, LibraryGrouping, ResolvedVirtualLibrary,
    VirtualLibraryAccessScope, VirtualLibraryMember, VirtualLibraryResolution,
};

#[derive(Debug, Default)]
struct AutomaticRefreshGeneration {
    latest_started: u64,
    latest_committed: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct VirtualLibraryService {
    pool: SqlitePool,
    server_storage: ServerStorageService,
    media_storage: MediaStorageService,
    automatic_refresh_generations: Arc<Mutex<HashMap<String, AutomaticRefreshGeneration>>>,
}

mod discovery;
mod membership;
mod resolution;

impl VirtualLibraryService {
    pub(crate) fn new(
        pool: SqlitePool,
        server_storage: ServerStorageService,
        media_storage: MediaStorageService,
    ) -> Self {
        Self {
            pool,
            server_storage,
            media_storage,
            automatic_refresh_generations: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

async fn reset_empty_group_type(
    tx: &mut Transaction<'_, Sqlite>,
    group_virtual_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE library_groups SET collection_type = NULL \
         WHERE virtual_id = ? \
           AND NOT EXISTS ( \
               SELECT 1 FROM library_group_members WHERE group_virtual_id = ? \
           )",
    )
    .bind(group_virtual_id)
    .bind(group_virtual_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn upsert_discovered_libraries(
    tx: &mut Transaction<'_, Sqlite>,
    libraries: &[DiscoveredLibrary],
) -> Result<(), sqlx::Error> {
    for library in libraries {
        sqlx::query(
            "INSERT INTO discovered_libraries \
             (server_id, original_library_id, name, collection_type, last_seen_at) \
             VALUES (?, ?, ?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(server_id, original_library_id) DO UPDATE SET \
                 name = excluded.name, \
                  collection_type = CASE WHEN TRIM(excluded.collection_type) = '' \
                      THEN discovered_libraries.collection_type ELSE excluded.collection_type END, \
                 last_seen_at = CURRENT_TIMESTAMP",
        )
        .bind(library.server_id.as_i64())
        .bind(normalize_library_id(&library.original_library_id))
        .bind(&library.name)
        .bind(library.collection_type.trim().to_ascii_lowercase())
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

fn resolution(
    name: String,
    members: Vec<VirtualLibraryMember>,
    catalog_scope_key: String,
) -> VirtualLibraryResolution {
    if members.is_empty() {
        VirtualLibraryResolution::Empty { name }
    } else {
        VirtualLibraryResolution::Resolved(ResolvedVirtualLibrary {
            name,
            members,
            catalog_scope_key,
        })
    }
}

fn server_is_allowed(
    server_id: ServerId,
    access_scope: Option<&VirtualLibraryAccessScope>,
    required_server_id: Option<ServerId>,
) -> bool {
    required_server_id.is_none_or(|required| required == server_id)
        && access_scope.is_none_or(|scope| scope.allows(server_id))
}

#[cfg(test)]
#[path = "../../unit_tests/libraries.rs"]
mod tests;
