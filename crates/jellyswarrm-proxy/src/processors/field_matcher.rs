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
