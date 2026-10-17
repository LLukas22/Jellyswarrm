//! Identity namespaces at selection/translation boundaries. These wrappers do
//! not validate ID syntax: Jellyfin IDs may be opaque, non-UUID strings.
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

macro_rules! opaque_identity {
    ($($name:ident),+ $(,)?) => { $(
        #[derive(Debug, Clone, PartialEq, Eq)]
        pub struct $name(String);
        impl $name {
            pub fn as_str(&self) -> &str { &self.0 }
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
    VirtualLiveStreamId,
    BackendLiveStreamId,
    PlaybackSessionId,
    ViewerId
);
