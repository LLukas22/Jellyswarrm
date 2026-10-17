//! Own persisted media storage, its caches, and narrow mapping/version/inventory exports.
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use sqlx::{sqlite::SqliteRow, Row, SqlitePool};
use tracing::{debug, error, info, trace};
use uuid::Uuid;

#[cfg(test)]
use crate::config::MediaStreamingMode;
use crate::servers::id::ServerId;
use crate::servers::storage::Server;
#[cfg(test)]
use crate::servers::url::ServerUrl;
use crate::{
    media::identity::{MediaAlias, MediaKind, MediaObservation, StableMediaGroup},
    media::scope::MediaCatalogScope,
    models::generate_token,
};
use moka::future::Cache;

mod models;
pub(crate) use models::{
    InventorySourceEvidence, InventorySourceStatus, InventoryUpdate, MediaCatalogSnapshot,
    MediaMapping, MediaVersionGroup, MediaVersionMember, MediaVersionSourceObservation,
    MediaVersionSourceRoute, ReconciliationGeneration, ReconciliationMode,
};

#[derive(Debug)]
struct ActiveMediaMember {
    mapping_id: i64,
    virtual_media_id: String,
    server_id: ServerId,
    group_id: Option<i64>,
    aliases: BTreeSet<MediaAlias>,
}

#[derive(Debug, Clone)]
pub struct MediaStorageService {
    pool: SqlitePool,
    original_mapping_cache: Cache<String, MediaMapping>,
    mapping_with_server_cache: Cache<String, (MediaMapping, Server)>,
    media_version_reconciliation: Arc<tokio::sync::Mutex<()>>,
}

mod inventory;
mod mappings;
mod versions;

impl MediaStorageService {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            original_mapping_cache: Cache::builder()
                .time_to_live(Duration::from_secs(60 * 30))
                .max_capacity(100_000)
                .build(),
            mapping_with_server_cache: Cache::builder()
                .time_to_live(Duration::from_secs(60 * 30))
                .max_capacity(10_000)
                .build(),
            media_version_reconciliation: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

fn find_index(parents: &mut [usize], index: usize) -> usize {
    if parents[index] != index {
        parents[index] = find_index(parents, parents[index]);
    }
    parents[index]
}

fn union_indexes(parents: &mut [usize], left: usize, right: usize) {
    let left = find_index(parents, left);
    let right = find_index(parents, right);
    if left != right {
        let (root, child) = if left < right {
            (left, right)
        } else {
            (right, left)
        };
        parents[child] = root;
    }
}

#[cfg(test)]
#[path = "../../unit_tests/media_storage.rs"]
mod tests;
