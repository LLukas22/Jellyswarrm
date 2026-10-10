use crate::models::MediaItem;

#[derive(Debug, Clone)]
pub enum ItemPresentation {
    Original,
    Server(String),
    Duplicate(String),
}

/// Original metadata plus a deferred display decision. Sorting and identity
/// reconciliation never see proxy-decorated strings.
#[derive(Debug, Clone)]
pub struct CatalogItem {
    pub item: MediaItem,
    pub presentation: ItemPresentation,
}

impl From<MediaItem> for CatalogItem {
    fn from(item: MediaItem) -> Self {
        Self {
            item,
            presentation: ItemPresentation::Original,
        }
    }
}

impl std::ops::Deref for CatalogItem {
    type Target = MediaItem;
    fn deref(&self) -> &MediaItem {
        &self.item
    }
}
impl std::ops::DerefMut for CatalogItem {
    fn deref_mut(&mut self) -> &mut MediaItem {
        &mut self.item
    }
}

impl CatalogItem {
    pub fn present(mut self) -> MediaItem {
        match &self.presentation {
            ItemPresentation::Original => {}
            ItemPresentation::Server(name) => {
                ItemNamePolicy::IncludeServerName.apply(&mut self.item, name)
            }
            ItemPresentation::Duplicate(server) => {
                if let Some(name) = &mut self.item.name {
                    append_server(name, server);
                }
            }
        }
        self.item
    }
}

fn append_server(name: &mut String, server: &str) {
    *name = format!("{name} [{server}]");
}

/// Presentation is applied to server-specific items only, after translation
/// and grouping have decided which items remain visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemNamePolicy {
    Preserve,
    IncludeServerName,
}

impl ItemNamePolicy {
    pub fn annotate(self, item: MediaItem, server_name: &str) -> CatalogItem {
        CatalogItem {
            item,
            presentation: match self {
                Self::Preserve => ItemPresentation::Original,
                Self::IncludeServerName => ItemPresentation::Server(server_name.to_string()),
            },
        }
    }
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
                append_server(name, server_name);
            }
        }
    }
}
