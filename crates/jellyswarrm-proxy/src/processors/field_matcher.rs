use std::{collections::HashSet, sync::LazyLock};

/// A struct for case-insensitive field name matching
pub struct FieldMatcher {
    fields: HashSet<String>,
}

impl FieldMatcher {
    /// Create a new FieldMatcher with the given field names
    pub fn new(fields: &[&str]) -> Self {
        Self {
            fields: fields.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Check if a field name matches any of the stored fields (case-insensitive)
    pub fn contains(&self, field_name: &str) -> bool {
        self.fields
            .iter()
            .any(|field| field.eq_ignore_ascii_case(field_name))
    }
}

// Static field matchers for different field types
pub static ID_FIELDS: LazyLock<FieldMatcher> = LazyLock::new(|| {
    FieldMatcher::new(&[
        "Id",
        "ItemId",
        "ParentId",
        "SeriesId",
        "SeasonId",
        "MediaSourceId",
        "PlaylistItemId",
    ])
});

pub static SESSION_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["SessionId", "PlaySessionId"]));

pub static USER_FIELDS: LazyLock<FieldMatcher> = LazyLock::new(|| FieldMatcher::new(&["UserId"]));

pub static RESPONSE_MEDIA_ID_FIELDS: LazyLock<FieldMatcher> = LazyLock::new(|| {
    FieldMatcher::new(&[
        "Id",
        "ItemId",
        "ParentId",
        "SeriesId",
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

pub static DELIVERY_URL_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["DeliveryUrl", "TranscodingUrl", "StreamUrl"]));

pub static DISABLED_BOOL_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["CanDelete", "CanDownload"]));

pub static SERVER_ID_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["ServerId"]));

pub static MEDIA_ID_ARRAY_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["BackdropImageTags", "ParentBackdropImageTags"]));

pub static MEDIA_ID_MAP_VALUE_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["ImageTags"]));

pub static MEDIA_ID_MAP_KEY_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["Trickplay"]));

pub static MEDIA_ID_NESTED_MAP_KEY_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["ImageBlurHashes"]));

pub static NAME_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["Name", "SeriesName"]));
