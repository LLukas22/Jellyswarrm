//! Virtual library records, assignment errors, grouping modes, and access-scoped resolution results.

use crate::{media::storage::MediaMapping, servers::id::ServerId, servers::storage::Server};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AutomaticVirtualLibrary {
    pub virtual_id: String,
    pub collection_type: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct VirtualLibraryMember {
    pub mapping: MediaMapping,
    pub server: Server,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtualLibraryAccessScope {
    user_id: crate::auth::user_id::UserId,
    server_ids: Vec<ServerId>,
}

impl VirtualLibraryAccessScope {
    pub fn new(
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
pub struct LibraryGroup {
    pub virtual_id: String,
    pub name: String,
    pub sort_order: i32,
    pub collection_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LibraryGroupMemberRecord {
    pub group_virtual_id: String,
    pub server_id: ServerId,
    pub original_library_id: String,
    pub library_name: String,
}

#[derive(Debug, Clone)]
pub struct LibraryAssignment {
    pub group_virtual_id: String,
    pub group_name: String,
    pub group_sort_order: i32,
    pub collection_type: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AssignLibraryError {
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
pub struct DiscoveredLibrary {
    pub server_id: ServerId,
    pub original_library_id: String,
    pub name: String,
    pub collection_type: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedVirtualLibrary {
    pub name: String,
    pub members: Vec<VirtualLibraryMember>,
    pub catalog_scope_key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryGrouping {
    Automatic,
    Configured,
    None,
}

#[derive(Debug, Clone)]
pub enum VirtualLibraryResolution {
    Unknown,
    Empty { name: String },
    Resolved(ResolvedVirtualLibrary),
}
