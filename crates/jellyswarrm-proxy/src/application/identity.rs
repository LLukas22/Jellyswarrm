//! Identity namespaces at selection/translation boundaries. These wrappers do
//! not validate ID syntax: Jellyfin IDs may be opaque, non-UUID strings.
//! Local viewer UUIDs use the canonical user-ID representation.
//! Deliberately no Deref, AsRef<str>, or cross-namespace conversions: leaving a
//! namespace requires an explicit wire/storage-boundary decision.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VirtualItemId(String);

impl VirtualItemId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for VirtualItemId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for VirtualItemId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackendItemId(String);

impl BackendItemId {
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for BackendItemId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

impl From<&str> for BackendItemId {
    fn from(id: &str) -> Self {
        Self(id.to_owned())
    }
}
