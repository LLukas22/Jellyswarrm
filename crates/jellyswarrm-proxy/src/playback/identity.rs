//! Opaque playback identity namespaces shared by workflows and translators.
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

opaque_identity!(VirtualLiveStreamId, BackendLiveStreamId, PlaybackSessionId);

/// Caller identity keeps its namespace while sharing local-user normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewerId(crate::auth::user_id::UserId);

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
