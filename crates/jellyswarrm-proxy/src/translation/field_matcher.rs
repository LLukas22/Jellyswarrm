//! Centralize JSON/URL field names and matching rules used by translation.
use std::{collections::HashSet, sync::LazyLock};

/// A struct for case-insensitive field name matching
pub(crate) struct FieldMatcher {
    fields: HashSet<String>,
}

impl FieldMatcher {
    /// Create a new FieldMatcher with the given field names
    pub(crate) fn new(fields: &[&str]) -> Self {
        Self {
            fields: fields.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Check if a field name matches any of the stored fields (case-insensitive)
    pub(crate) fn contains(&self, field_name: &str) -> bool {
        self.fields
            .iter()
            .any(|field| field.eq_ignore_ascii_case(field_name))
    }
}

// Static field matchers for different field types
pub(crate) static ID_FIELDS: LazyLock<FieldMatcher> = LazyLock::new(|| {
    FieldMatcher::new(&[
        "Id",
        "AlbumId",
        "ItemId",
        "ParentId",
        "SeriesId",
        "ChannelId",
        "ProgramId",
        "TimerId",
        "SeriesTimerId",
        "SeasonId",
        "MediaSourceId",
        "PlaylistItemId",
    ])
});

pub(crate) static SESSION_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["SessionId", "PlaySessionId"]));

pub(crate) static USER_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["UserId"]));

pub(crate) static RESPONSE_MEDIA_ID_FIELDS: LazyLock<FieldMatcher> = LazyLock::new(|| {
    FieldMatcher::new(&[
        "Id",
        "AlbumId",
        "ItemId",
        "ParentId",
        "SeriesId",
        "ChannelId",
        "ProgramId",
        "TimerId",
        "SeriesTimerId",
        "SeasonId",
        "MediaSourceId",
        "PlaylistItemId",
        "Etag",
        "DisplayPreferencesId",
        "ParentLogoItemId",
        "ParentBackdropItemId",
        "ParentLogoImageTag",
        "ParentThumbItemId",
        "ParentThumbImageTag",
        "SeriesPrimaryImageTag",
        "ImageTag",
    ])
});

pub(crate) static DELIVERY_URL_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["DeliveryUrl", "TranscodingUrl", "StreamUrl"]));

pub(crate) static DISABLED_BOOL_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["CanDelete", "CanDownload"]));

pub(crate) static SERVER_ID_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["ServerId"]));

pub(crate) static MEDIA_ID_ARRAY_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["BackdropImageTags", "ParentBackdropImageTags"]));

/// JSON array parents whose string items are media IDs (e.g.
/// `CreatePlaylistDto { Ids: [...] }`). Array items carry the index as key,
/// so they cannot be matched via `ID_FIELDS` and need parent-path matching.
pub(crate) static MEDIA_ID_LIST_PARENT_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["Ids", "ItemIds", "EntryIds", "PlaylistItemIds"]));

pub(crate) static MEDIA_ID_MAP_VALUE_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["ImageTags"]));

pub(crate) static MEDIA_ID_MAP_KEY_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["Trickplay"]));

pub(crate) static MEDIA_ID_NESTED_MAP_KEY_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["ImageBlurHashes"]));
