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
        #[derive(Debug, Clone, PartialEq, Eq)]
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
