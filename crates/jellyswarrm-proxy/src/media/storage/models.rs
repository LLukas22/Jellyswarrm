//! Persisted media mappings, version records, and validated reconciliation evidence.
use super::*;
#[derive(Debug, Clone)]
pub(crate) struct MediaMapping {
    pub(crate) id: i64,
    pub(crate) virtual_media_id: String,
    pub(crate) original_media_id: String,
    pub(crate) server_id: ServerId,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Canonical URL is asserted by storage tests")
    )]
    pub(crate) server_url: String,
    #[expect(
        dead_code,
        reason = "Retain timestamp decoding and media mapping diagnostics"
    )]
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone)]
pub(crate) struct MediaVersionGroup {
    pub(crate) id: i64,
    pub(crate) virtual_media_id: String,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Reconciliation scope is asserted by storage tests"
        )
    )]
    pub(crate) scope_id: i64,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Publication transitions are asserted by storage tests"
        )
    )]
    pub(crate) published: bool,
    #[expect(
        dead_code,
        reason = "Eligibility is filtered in SQL; retain ambiguity in group diagnostics"
    )]
    pub(crate) ambiguous: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct MediaVersionMember {
    pub(crate) mapping: MediaMapping,
    pub(crate) server: Server,
}

#[derive(Debug, Clone)]
pub(crate) struct MediaVersionSourceObservation {
    pub(crate) member_mapping_id: i64,
    pub(crate) source_virtual_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct MediaVersionSourceRoute {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Source identity is asserted by routing tests")
    )]
    pub(crate) source_mapping: MediaMapping,
    pub(crate) member_mapping: MediaMapping,
}

#[derive(Debug, Clone)]
pub(crate) struct MediaCatalogSnapshot {
    pub(crate) source_key: String,
    pub(crate) server_id: ServerId,
    pub(crate) complete: bool,
    pub(crate) observations: Vec<MediaObservation>,
}

/// Issued by the persistent reconciliation clock; cannot be fabricated from an
/// item/group ID or a caller-provided integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReconciliationGeneration(pub(super) i64);

impl ReconciliationGeneration {
    pub(crate) fn as_i64(self) -> i64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum ReconciliationMode {
    Disabled,
    Enabled {
        generation: ReconciliationGeneration,
    },
}

/// Fetch evidence is distinct from query coverage. Bounded/partial sources can
/// contribute observations, but neither is a complete inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InventorySourceStatus {
    Complete,
    Bounded,
    Incomplete,
}

pub(crate) struct InventorySourceEvidence {
    pub(crate) source_key: String,
    pub(crate) server_id: ServerId,
    pub(crate) status: InventorySourceStatus,
}

#[derive(Debug, thiserror::Error)]
#[error("inconsistent catalog inventory evidence")]
pub(crate) struct InvalidInventoryEvidence;

/// Only validated construction can grant replacement/pruning authority. The
/// payload cannot be mutated after checking source identity and query coverage.
pub(crate) struct CompleteInventory {
    pub(super) snapshots: Vec<MediaCatalogSnapshot>,
    pub(super) coverage: BTreeSet<MediaKind>,
    pub(super) prune_missing_sources: bool,
}

pub(crate) enum InventoryUpdate {
    Observations(Vec<MediaCatalogSnapshot>),
    CompleteInventory(CompleteInventory),
}

impl InventoryUpdate {
    pub(crate) fn validated_inventory(
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
