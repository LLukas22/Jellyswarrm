//! Centralize JSON/URL field names and matching rules used by translation.
use std::{collections::HashSet, sync::LazyLock};

// Shared by client routing and delivery rewriting. Image `Tag` values are
// opaque cache tokens, not media identities.
pub static MEDIA_ID_QUERY_TAGS: &[&str] = &[
    "AlbumId",
    "AlbumIds",
    "ParentId",
    "ItemId",
    "SeriesId",
    "LibrarySeriesId",
    "ChannelIds",
    "ProgramId",
    "GenreIds",
    "StudioIds",
    "ExcludeItemIds",
    "SeriesTimerId",
    "MediaSourceId",
    "SeasonId",
    "startItemId",
    "IDs",
    "EntryIds",
    "PlaylistItemId",
    "PlaylistItemIds",
    "PersonIds",
    "ArtistIds",
    "ContributingArtistIds",
    "AlbumArtistIds",
    "ExcludeArtistIds",
];
pub static API_KEY_QUERY_TAGS: &[&str] = &["api_key", "ApiKey"];

pub fn matches_case_insensitive(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

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

pub static SESSION_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["SessionId", "PlaySessionId"]));

pub static USER_FIELDS: LazyLock<FieldMatcher> = LazyLock::new(|| FieldMatcher::new(&["UserId"]));

pub static RESPONSE_MEDIA_ID_FIELDS: LazyLock<FieldMatcher> = LazyLock::new(|| {
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

pub static DELIVERY_URL_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["DeliveryUrl", "TranscodingUrl", "StreamUrl"]));

pub static DISABLED_BOOL_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["CanDelete", "CanDownload"]));

pub static SERVER_ID_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["ServerId"]));

pub static MEDIA_ID_ARRAY_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["BackdropImageTags", "ParentBackdropImageTags"]));

/// JSON array parents whose string items are media IDs (e.g.
/// `CreatePlaylistDto { Ids: [...] }`). Array items carry the index as key,
/// so they cannot be matched via `ID_FIELDS` and need parent-path matching.
pub static MEDIA_ID_LIST_PARENT_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["Ids", "ItemIds", "EntryIds", "PlaylistItemIds"]));

pub static MEDIA_ID_MAP_VALUE_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["ImageTags"]));

pub static MEDIA_ID_MAP_KEY_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["Trickplay"]));

pub static MEDIA_ID_NESTED_MAP_KEY_FIELDS: LazyLock<FieldMatcher> =
    LazyLock::new(|| FieldMatcher::new(&["ImageBlurHashes"]));
