//! Virtual library records, assignment errors, grouping modes, and access-scoped resolution results.

use crate::{media::storage::MediaMapping, servers::id::ServerId, servers::storage::Server};

#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct AutomaticVirtualLibrary {
    pub(crate) virtual_id: String,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Collection type is asserted by library tests")
    )]
    pub(crate) collection_type: String,
    pub(crate) name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct VirtualLibraryMember {
    pub(crate) mapping: MediaMapping,
    pub(crate) server: Server,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VirtualLibraryAccessScope {
    user_id: crate::auth::user_id::UserId,
    server_ids: Vec<ServerId>,
}

impl VirtualLibraryAccessScope {
    pub(crate) fn new(
        user_id: impl Into<crate::auth::user_id::UserId>,
        server_ids: impl IntoIterator<Item = ServerId>,
    ) -> Self {
        let mut server_ids = server_ids.into_iter().collect::<Vec<_>>();
        server_ids.sort_by_key(|server_id| server_id.as_i64());
        server_ids.dedup();
        Self {
            user_id: user_id.into(),
            server_ids,
        }
    }

    pub(crate) fn user_id(&self) -> &str {
        &self.user_id
    }

    pub(crate) fn allows(&self, server_id: ServerId) -> bool {
        self.server_ids.contains(&server_id)
    }

    pub(crate) fn server_ids(&self) -> &[ServerId] {
        &self.server_ids
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LibraryGroup {
    pub(crate) virtual_id: String,
    pub(crate) name: String,
    #[expect(
        dead_code,
        reason = "Ordering is applied in SQL; retain the group record's sort metadata"
    )]
    pub(crate) sort_order: i32,
    pub(crate) collection_type: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct LibraryGroupMemberRecord {
    #[expect(
        dead_code,
        reason = "Retain the owning group in decoded membership diagnostics"
    )]
    pub(crate) group_virtual_id: String,
    pub(crate) server_id: ServerId,
    pub(crate) original_library_id: String,
    pub(crate) library_name: String,
}

#[derive(Debug, Clone)]
pub(crate) struct LibraryAssignment {
    pub(crate) group_virtual_id: String,
    pub(crate) group_name: String,
    pub(crate) group_sort_order: i32,
    pub(crate) collection_type: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AssignLibraryError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("virtual library not found")]
    GroupNotFound,
    #[error("library collection type is required")]
    MissingCollectionType,
    #[error("virtual library accepts {group_type}, not {library_type}")]
    CollectionTypeMismatch {
        group_type: String,
        library_type: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiscoveredLibrary {
    pub(crate) server_id: ServerId,
    pub(crate) original_library_id: String,
    pub(crate) name: String,
    pub(crate) collection_type: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedVirtualLibrary {
    pub(crate) name: String,
    pub(crate) members: Vec<VirtualLibraryMember>,
    pub(crate) catalog_scope_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LibraryGrouping {
    Automatic,
    Configured,
    None,
}

#[derive(Debug, Clone)]
pub(crate) enum VirtualLibraryResolution {
    Unknown,
    Empty { name: String },
    Resolved(ResolvedVirtualLibrary),
}
