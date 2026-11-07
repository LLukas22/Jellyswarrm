//! Typed identifiers. Opaque Jellyfin IDs are not validated: they may be
//! non-UUID strings. Deliberately no `Deref`, `AsRef<str>`, or cross-namespace
//! conversions: leaving a namespace requires an explicit wire/storage-boundary decision.
use serde::{Deserialize, Serialize};

/// Backend server identifier with transparent wire representation.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct ServerId(i64);

impl ServerId {
    pub(crate) fn new(value: i64) -> Self {
        Self(value)
    }

    pub(crate) fn as_i64(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for ServerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

macro_rules! opaque_identity {
    ($($name:ident),+ $(,)?) => { $(
        #[derive(Debug, Clone, PartialEq, Eq, Hash)]
        pub(crate) struct $name(String);
        impl $name {
            pub(crate) fn as_str(&self) -> &str { &self.0 }
        }
        impl From<String> for $name {
            fn from(value: String) -> Self { Self(value) }
        }
        impl From<&str> for $name {
            fn from(value: &str) -> Self { Self(value.to_owned()) }
        }
    )+ };
}

opaque_identity!(
    VirtualItemId,
    BackendItemId,
    VirtualLiveStreamId,
    BackendLiveStreamId,
    PlaybackSessionId,
);

/// Canonical form of an opaque media ID: UUIDs become lowercase and undashed;
/// anything else is preserved exactly.
pub(crate) fn normalize_media_id(value: &str) -> String {
    match uuid::Uuid::parse_str(value) {
        Ok(id) => id.simple().to_string(),
        Err(_) => value.to_owned(),
    }
}

macro_rules! media_identity {
    ($($name:ident),+ $(,)?) => { $(
        impl $name {
            /// Storage lookups and session bindings compare the canonical form.
            pub(crate) fn normalized(&self) -> Self {
                Self(normalize_media_id(&self.0))
            }
            /// Leaves the namespace at a wire or JSON boundary without copying.
            pub(crate) fn into_string(self) -> String { self.0 }
        }
    )+ };
}

media_identity!(VirtualItemId, BackendItemId);

/// Caller identity keeps its namespace while sharing local-user normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewerId(super::user_id::UserId);

impl ViewerId {
    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl From<String> for ViewerId {
    fn from(value: String) -> Self {
        Self(value.into())
    }
}

impl From<&str> for ViewerId {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_ids_normalize_uuids_and_preserve_opaque_values() {
        let dashed = VirtualItemId::from("550E8400-E29B-41D4-A716-446655440000");
        assert_eq!(
            dashed.normalized().as_str(),
            "550e8400e29b41d4a716446655440000"
        );
        // Normalization is explicit: construction keeps the wire form verbatim.
        assert_eq!(dashed.as_str(), "550E8400-E29B-41D4-A716-446655440000");
        for opaque in ["", "Opaque-Item", "not-a-valid-uuid"] {
            assert_eq!(BackendItemId::from(opaque).normalized().as_str(), opaque);
        }
    }
}
