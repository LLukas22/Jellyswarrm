use crate::models::MediaItem;

/// Presentation is applied to server-specific items only, after translation
/// and grouping have decided which items remain visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemNamePolicy {
    Preserve,
    IncludeServerName,
}

impl ItemNamePolicy {
    pub fn apply(self, item: &mut MediaItem, server_name: &str) {
        if self == Self::IncludeServerName
            && !item
                .collection_type
                .as_ref()
                .is_some_and(|kind| kind.as_str().eq_ignore_ascii_case("livetv"))
        {
            for name in [&mut item.name, &mut item.series_name]
                .into_iter()
                .flatten()
            {
                *name = format!("{name} [{server_name}]");
            }
        }
    }
}
