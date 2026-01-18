use std::{collections::HashMap, sync::LazyLock};

/// A struct for case-insensitive field name matching
/// Uses HashMap for O(1) lookups instead of O(n) iteration
pub struct FieldMatcher {
    // Store lowercase versions of field names for fast O(1) lookup
    fields: HashMap<String, ()>,
}

impl FieldMatcher {
    /// Create a new FieldMatcher with the given field names
    pub fn new(fields: &[&str]) -> Self {
        Self {
            fields: fields
                .iter()
                .map(|s| (s.to_ascii_lowercase(), ()))
                .collect(),
        }
    }

    /// Check if a field name matches any of the stored fields (case-insensitive)
    /// Now O(1) instead of O(n)
    pub fn contains(&self, field_name: &str) -> bool {
        self.fields.contains_key(&field_name.to_ascii_lowercase())
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
