//! Deduplication engine for merged libraries.
//!
//! This module provides functionality to identify and merge duplicate media items
//! from multiple Jellyfin servers based on various matching strategies.

use std::collections::HashMap;
use tracing::{debug, trace};

use crate::merged_library_storage::DeduplicationStrategy;
use crate::models::MediaItem;

/// A deduplicated item that may have multiple source copies
#[derive(Debug, Clone)]
pub struct DeduplicatedItem {
    /// The primary item to display (from highest priority source)
    pub primary: MediaItem,
    /// All source copies of this item (including primary), with server info
    pub sources: Vec<ItemSource>,
}

/// Information about a source copy of an item
#[derive(Debug, Clone)]
pub struct ItemSource {
    /// The original media item
    pub item: MediaItem,
    /// Server ID this item came from
    pub server_id: i64,
    /// Server name for display
    pub server_name: String,
    /// Priority of this source (higher = preferred)
    pub priority: i32,
}

/// Extract provider IDs from a MediaItem
fn extract_provider_ids(item: &MediaItem) -> HashMap<String, String> {
    let mut ids = HashMap::new();

    if let Some(ref provider_ids) = item.provider_ids {
        if let Some(obj) = provider_ids.as_object() {
            for (key, value) in obj {
                if let Some(id) = value.as_str() {
                    if !id.is_empty() {
                        ids.insert(key.to_lowercase(), id.to_string());
                    }
                }
            }
        }
    }

    ids
}

/// Generate a deduplication key based on provider IDs
fn provider_id_key(item: &MediaItem) -> Option<String> {
    let ids = extract_provider_ids(item);

    // Priority order for matching
    let priority_providers = ["tmdb", "imdb", "tvdb", "thetvdb", "themoviedb"];

    for provider in priority_providers {
        if let Some(id) = ids.get(provider) {
            return Some(format!("{}:{}", provider, id));
        }
    }

    // If no priority provider found, use any available
    if let Some((provider, id)) = ids.iter().next() {
        return Some(format!("{}:{}", provider, id));
    }

    None
}

/// Generate a deduplication key based on name and year
fn name_year_key(item: &MediaItem) -> Option<String> {
    let name = item.name.as_ref()?;

    // Try to extract year from various fields
    let year = item.extra.get("ProductionYear")
        .or_else(|| item.extra.get("PremiereDate"))
        .and_then(|v| {
            if let Some(y) = v.as_i64() {
                Some(y.to_string())
            } else if let Some(s) = v.as_str() {
                // Extract year from date string like "2023-01-15"
                s.split('-').next().map(|s| s.to_string())
            } else {
                None
            }
        });

    // Normalize the name: lowercase, remove special characters
    let normalized_name: String = name
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    if normalized_name.is_empty() {
        return None;
    }

    match year {
        Some(y) => Some(format!("{}|{}", normalized_name, y)),
        None => Some(normalized_name),
    }
}

/// Deduplicate a list of items from multiple sources
pub fn deduplicate_items(
    items_with_sources: Vec<(MediaItem, i64, String, i32)>, // (item, server_id, server_name, priority)
    strategy: &DeduplicationStrategy,
) -> Vec<DeduplicatedItem> {
    match strategy {
        DeduplicationStrategy::None => {
            // No deduplication - each item is its own entry
            items_with_sources
                .into_iter()
                .map(|(item, server_id, server_name, priority)| {
                    DeduplicatedItem {
                        primary: item.clone(),
                        sources: vec![ItemSource {
                            item,
                            server_id,
                            server_name,
                            priority,
                        }],
                    }
                })
                .collect()
        }
        DeduplicationStrategy::ProviderIds => {
            deduplicate_by_key(items_with_sources, provider_id_key)
        }
        DeduplicationStrategy::NameYear => {
            deduplicate_by_key(items_with_sources, name_year_key)
        }
    }
}

/// Generic deduplication using a key extraction function
fn deduplicate_by_key<F>(
    items_with_sources: Vec<(MediaItem, i64, String, i32)>,
    key_fn: F,
) -> Vec<DeduplicatedItem>
where
    F: Fn(&MediaItem) -> Option<String>,
{
    let mut groups: HashMap<String, Vec<ItemSource>> = HashMap::new();
    let mut no_key_items: Vec<ItemSource> = Vec::new();

    for (item, server_id, server_name, priority) in items_with_sources {
        let source = ItemSource {
            item: item.clone(),
            server_id,
            server_name,
            priority,
        };

        match key_fn(&item) {
            Some(key) => {
                groups.entry(key).or_default().push(source);
            }
            None => {
                // Items without a key can't be deduplicated
                no_key_items.push(source);
            }
        }
    }

    let mut result: Vec<DeduplicatedItem> = Vec::new();

    // Process grouped items
    for (_key, mut sources) in groups {
        // Sort by priority (highest first)
        sources.sort_by(|a, b| b.priority.cmp(&a.priority));

        let primary = sources.first().unwrap().item.clone();

        debug!(
            "Deduplicated '{}' - {} sources from servers: {:?}",
            primary.name.as_deref().unwrap_or("Unknown"),
            sources.len(),
            sources.iter().map(|s| &s.server_name).collect::<Vec<_>>()
        );

        result.push(DeduplicatedItem { primary, sources });
    }

    // Add items that couldn't be deduplicated
    for source in no_key_items {
        trace!(
            "Item '{}' has no dedup key, keeping as-is",
            source.item.name.as_deref().unwrap_or("Unknown")
        );
        result.push(DeduplicatedItem {
            primary: source.item.clone(),
            sources: vec![source],
        });
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use crate::models::enums::BaseItemKind;

    fn create_test_item(name: &str, provider_ids: Option<serde_json::Value>) -> MediaItem {
        MediaItem {
            id: uuid::Uuid::new_v4().to_string(),
            name: Some(name.to_string()),
            provider_ids,
            item_type: BaseItemKind::Movie,
            server_id: None,
            item_id: None,
            series_id: None,
            series_name: None,
            season_id: None,
            etag: None,
            date_created: None,
            can_delete: None,
            can_download: None,
            sort_name: None,
            external_urls: None,
            path: None,
            enable_media_source_display: None,
            channel_id: None,
            is_folder: None,
            parent_id: None,
            parent_logo_item_id: None,
            parent_backdrop_item_id: None,
            parent_backdrop_image_tags: None,
            parent_logo_image_tag: None,
            parent_thumb_item_id: None,
            parent_thumb_image_tag: None,
            collection_type: None,
            user_data: None,
            child_count: None,
            display_preferences_id: None,
            tags: None,
            series_primary_image_tag: None,
            image_tags: None,
            backdrop_image_tags: None,
            image_blur_hashes: None,
            original_title: None,
            media_sources: None,
            media_streams: None,
            chapters: None,
            trickplay: None,
            extra: HashMap::new(),
        }
    }

    #[test]
    fn test_provider_id_deduplication() {
        let items = vec![
            (
                create_test_item("The Matrix", Some(json!({"Tmdb": "603", "Imdb": "tt0133093"}))),
                1, "Server A".to_string(), 100,
            ),
            (
                create_test_item("The Matrix (1999)", Some(json!({"Tmdb": "603"}))),
                2, "Server B".to_string(), 50,
            ),
            (
                create_test_item("Inception", Some(json!({"Tmdb": "27205"}))),
                1, "Server A".to_string(), 100,
            ),
        ];

        let result = deduplicate_items(items, &DeduplicationStrategy::ProviderIds);

        assert_eq!(result.len(), 2); // Matrix + Inception

        // Find the Matrix entry
        let matrix = result.iter().find(|d| d.primary.name.as_deref() == Some("The Matrix")).unwrap();
        assert_eq!(matrix.sources.len(), 2);
    }

    #[test]
    fn test_no_deduplication() {
        let items = vec![
            (
                create_test_item("Movie A", None),
                1, "Server A".to_string(), 100,
            ),
            (
                create_test_item("Movie A", None),
                2, "Server B".to_string(), 50,
            ),
        ];

        let result = deduplicate_items(items, &DeduplicationStrategy::None);

        assert_eq!(result.len(), 2); // Both copies kept
    }

    #[test]
    fn test_name_year_deduplication() {
        let mut item1 = create_test_item("The Matrix", None);
        item1.extra.insert("ProductionYear".to_string(), json!(1999));

        let mut item2 = create_test_item("the matrix", None);
        item2.extra.insert("ProductionYear".to_string(), json!(1999));

        let items = vec![
            (item1, 1, "Server A".to_string(), 100),
            (item2, 2, "Server B".to_string(), 50),
        ];

        let result = deduplicate_items(items, &DeduplicationStrategy::NameYear);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].sources.len(), 2);
    }
}
