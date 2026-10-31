//! Own persisted media storage, its caches, and narrow mapping/version/inventory exports.
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use sqlx::{sqlite::SqliteRow, Row, SqlitePool};
use tracing::{debug, trace};
#[cfg(test)]
use tracing::{error, info};
use uuid::Uuid;

#[cfg(test)]
use crate::config::MediaStreamingMode;
use crate::domain::ids::ServerId;
use crate::servers::storage::Server;
#[cfg(test)]
use crate::servers::url::ServerUrl;
use crate::{
    domain::security::generate_token,
    media::identity::{MediaAlias, MediaKind, MediaObservation, StableMediaGroup},
    media::scope::MediaCatalogScope,
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
pub(crate) struct MediaStorageService {
    pool: SqlitePool,
    original_mapping_cache: Cache<String, MediaMapping>,
    media_version_reconciliation: Arc<tokio::sync::Mutex<()>>,
}

mod inventory;
mod mappings;
mod versions;

impl MediaStorageService {
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            original_mapping_cache: Cache::builder()
                .time_to_live(Duration::from_secs(60 * 30))
                .max_capacity(100_000)
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
