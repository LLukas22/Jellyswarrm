//! Persisted media mappings, version records, and validated reconciliation evidence.
use super::*;
#[derive(Debug, Clone)]
pub struct MediaMapping {
    pub id: i64,
    pub virtual_media_id: String,
    pub original_media_id: String,
    pub server_id: ServerId,
    pub server_url: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub struct MediaVersionGroup {
    pub id: i64,
    pub virtual_media_id: String,
    pub scope_id: i64,
    pub published: bool,
    pub ambiguous: bool,
}

#[derive(Debug, Clone)]
pub struct MediaVersionMember {
    pub mapping: MediaMapping,
    pub server: Server,
}

#[derive(Debug, Clone)]
pub struct MediaVersionSourceObservation {
    pub member_mapping_id: i64,
    pub source_virtual_id: String,
}

#[derive(Debug, Clone)]
pub struct MediaVersionSourceRoute {
    pub source_mapping: MediaMapping,
    pub member_mapping: MediaMapping,
}

#[derive(Debug, Clone)]
pub struct MediaCatalogSnapshot {
    pub source_key: String,
    pub server_id: ServerId,
    pub complete: bool,
    pub observations: Vec<MediaObservation>,
}

/// Issued by the persistent reconciliation clock; cannot be fabricated from an
/// item/group ID or a caller-provided integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconciliationGeneration(pub(super) i64);

impl ReconciliationGeneration {
    pub(crate) fn as_i64(self) -> i64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ReconciliationMode {
    Disabled,
    Enabled {
        generation: ReconciliationGeneration,
    },
}

/// Fetch evidence is distinct from query coverage. Bounded/partial sources can
/// contribute observations, but neither is a complete inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InventorySourceStatus {
    Complete,
    Bounded,
    Incomplete,
}

pub struct InventorySourceEvidence {
    pub source_key: String,
    pub server_id: ServerId,
    pub status: InventorySourceStatus,
}

#[derive(Debug, thiserror::Error)]
#[error("inconsistent catalog inventory evidence")]
pub struct InvalidInventoryEvidence;

/// Only validated construction can grant replacement/pruning authority. The
/// payload cannot be mutated after checking source identity and query coverage.
pub struct CompleteInventory {
    pub(super) snapshots: Vec<MediaCatalogSnapshot>,
    pub(super) coverage: BTreeSet<MediaKind>,
    pub(super) prune_missing_sources: bool,
}

pub enum InventoryUpdate {
    Observations(Vec<MediaCatalogSnapshot>),
    CompleteInventory(CompleteInventory),
}

impl InventoryUpdate {
    pub fn validated_inventory(
        mut snapshots: Vec<MediaCatalogSnapshot>,
        coverage: BTreeSet<MediaKind>,
        evidence: &[InventorySourceEvidence],
        unavailable_sources: usize,
    ) -> Result<Self, InvalidInventoryEvidence> {
        if coverage.is_empty() || evidence.is_empty() {
            return Err(InvalidInventoryEvidence);
        }
        let mut sources = HashMap::new();
        for source in evidence {
            if source.source_key.is_empty()
                || sources.insert(source.source_key.as_str(), source).is_some()
            {
                return Err(InvalidInventoryEvidence);
            }
        }
        let mut seen = HashSet::new();
        for snapshot in &mut snapshots {
            let source = sources
                .get(snapshot.source_key.as_str())
                .ok_or(InvalidInventoryEvidence)?;
            if source.server_id != snapshot.server_id || !seen.insert(snapshot.source_key.clone()) {
                return Err(InvalidInventoryEvidence);
            }
            // Never trust a separately supplied snapshot.complete flag.
            snapshot.complete = source.status == InventorySourceStatus::Complete;
        }
        // A claimed complete source must actually have an inventory, including
        // an explicit empty snapshot when all items were deleted.
        if evidence.iter().any(|source| {
            source.status == InventorySourceStatus::Complete && !seen.contains(&source.source_key)
        }) {
            return Err(InvalidInventoryEvidence);
        }
        let prune_missing_sources = unavailable_sources == 0
            && evidence
                .iter()
                .all(|source| source.status == InventorySourceStatus::Complete);
        Ok(Self::CompleteInventory(CompleteInventory {
            snapshots,
            coverage,
            prune_missing_sources,
        }))
    }
}

impl<'r> sqlx::FromRow<'r, SqliteRow> for MediaMapping {
    fn from_row(row: &SqliteRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("id")?,
            virtual_media_id: row.try_get("virtual_media_id")?,
            original_media_id: row.try_get("original_media_id")?,
            server_id: ServerId::new(row.try_get("server_id")?),
            server_url: row.try_get("server_url")?,
            created_at: row.try_get("created_at")?,
        })
    }
}
