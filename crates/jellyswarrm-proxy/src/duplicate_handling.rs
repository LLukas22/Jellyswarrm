use std::collections::HashMap;

use crate::{
    models::{enums::BaseItemKind, MediaItem},
    server_storage::Server,
};

#[derive(Debug, Clone)]
pub struct TaggedMediaItem {
    pub item: MediaItem,
    pub server: Server,
}

pub fn label_duplicates(items: Vec<TaggedMediaItem>) -> Vec<MediaItem> {
    let mut group_indexes: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<Vec<TaggedMediaItem>> = Vec::new();
    for tagged in items {
        let key = duplicate_key(&tagged.item);
        if let Some(&index) = group_indexes.get(&key) {
            groups[index].push(tagged);
        } else {
            group_indexes.insert(key, groups.len());
            groups.push(vec![tagged]);
        }
    }

    groups.into_iter().flat_map(label_duplicate_group).collect()
}

fn label_duplicate_group(group: Vec<TaggedMediaItem>) -> Vec<MediaItem> {
    if group.len() == 1 {
        return group.into_iter().map(|tagged| tagged.item).collect();
    }

    group.into_iter().map(item_with_server_suffix).collect()
}

fn duplicate_key(item: &MediaItem) -> String {
    if item.item_type == BaseItemKind::Episode {
        return episode_duplicate_key(item);
    }

    if let Some(provider) = provider_identity(item) {
        return format!("content:provider:{provider}:{:?}", item.item_type);
    }

    let name = normalized_name(item);
    let year = item
        .production_year
        .map(i64::from)
        .or_else(|| {
            item.extra
                .get("ProductionYear")
                .or_else(|| item.extra.get("productionYear"))
                .and_then(serde_json::Value::as_i64)
        })
        .unwrap_or_default();
    format!("content:title:{name}:{year}:{:?}", item.item_type)
}

fn item_with_server_suffix(mut tagged: TaggedMediaItem) -> MediaItem {
    if let Some(name) = tagged.item.name.as_mut() {
        *name = format!("{name} [{}]", tagged.server.name);
    }
    tagged.item
}

fn episode_duplicate_key(item: &MediaItem) -> String {
    if let Some(user_key) = item.user_data.as_ref().and_then(|data| {
        let key = data.key.trim();
        if key.is_empty() || key.chars().all(|character| character == '0') {
            None
        } else {
            Some(key.to_string())
        }
    }) {
        return format!("episode:userkey:{user_key}");
    }

    if let Some(provider_key) = provider_identity(item) {
        let season = episode_number(item, "ParentIndexNumber");
        let episode = episode_number(item, "IndexNumber");
        return format!("episode:provider:{provider_key}:s{season}:e{episode}");
    }

    let series = item
        .series_name
        .as_deref()
        .map(normalize_title)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| normalized_name(item));
    let season = episode_number(item, "ParentIndexNumber");
    let episode = episode_number(item, "IndexNumber");
    format!("episode:fallback:{series}:s{season}:e{episode}")
}

fn episode_number(item: &MediaItem, field: &str) -> i32 {
    item.extra
        .get(field)
        .or_else(|| {
            item.extra.get(match field {
                "ParentIndexNumber" => "parentIndexNumber",
                _ => "indexNumber",
            })
        })
        .and_then(|value| value.as_i64())
        .unwrap_or(0) as i32
}

fn provider_identity(item: &MediaItem) -> Option<String> {
    let provider_ids = item.provider_ids.as_ref()?.as_object()?;
    for preferred in ["Tmdb", "Imdb", "Tvdb", "TmdbCollection"] {
        for (key, value) in provider_ids {
            if key.eq_ignore_ascii_case(preferred) {
                if let Some(id) = value.as_str() {
                    if !id.is_empty() {
                        return Some(format!("{}:{id}", preferred.to_ascii_lowercase()));
                    }
                }
            }
        }
    }
    None
}

fn normalized_name(item: &MediaItem) -> String {
    let raw = item
        .sort_name
        .as_deref()
        .or(item.original_title.as_deref())
        .or(item.name.as_deref())
        .unwrap_or("");
    normalize_title(raw)
}

fn normalize_title(value: &str) -> String {
    let value = value.trim();
    let value = value
        .rsplit_once('[')
        .filter(|(_, suffix)| suffix.ends_with(']'))
        .map(|(prefix, _)| prefix.trim_end())
        .unwrap_or(value);

    value
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::MediaStreamingMode, server_id::ServerId, server_url::ServerUrl};

    fn tagged(server_id: i64, priority: i32, name: &str, provider: &str) -> TaggedMediaItem {
        let item: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": format!("{server_id}-{name}"),
            "Name": name,
            "Type": "Movie",
            "ProviderIds": { "Tmdb": provider }
        }))
        .unwrap();

        TaggedMediaItem {
            item,
            server: Server {
                id: ServerId::new(server_id),
                name: format!("Server {server_id}"),
                url: ServerUrl::parse("http://example:8096").unwrap(),
                priority,
                media_streaming_mode: MediaStreamingMode::Redirect,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            },
        }
    }

    #[test]
    fn duplicates_are_kept_and_labeled_with_their_server() {
        let less: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": "left",
            "Name": "Wistoria",
            "Type": "Series",
            "ChildCount": 12,
            "ProviderIds": { "Tmdb": "abc" }
        }))
        .unwrap();
        let more: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": "right",
            "Name": "Wistoria",
            "Type": "Series",
            "ChildCount": 21,
            "ProviderIds": { "Tmdb": "abc" }
        }))
        .unwrap();
        let result = label_duplicates(vec![
            TaggedMediaItem {
                item: less,
                server: tagged(1, 100, "x", "abc").server,
            },
            TaggedMediaItem {
                item: more,
                server: tagged(2, 50, "x", "abc").server,
            },
        ]);
        assert_eq!(
            result
                .iter()
                .filter_map(|item| item.name.as_deref())
                .collect::<Vec<_>>(),
            vec!["Wistoria [Server 1]", "Wistoria [Server 2]"]
        );
    }

    #[test]
    fn same_title_with_different_provider_ids_is_not_a_duplicate() {
        let result = label_duplicates(vec![
            tagged(1, 100, "Crash", "1996"),
            tagged(2, 100, "Crash", "2004"),
        ]);

        assert_eq!(
            result
                .iter()
                .filter_map(|item| item.name.as_deref())
                .collect::<Vec<_>>(),
            vec!["Crash", "Crash"]
        );
    }

    #[test]
    fn same_title_with_different_production_years_is_not_a_duplicate() {
        let mut original = tagged(1, 100, "The Thing", "unused");
        original.item.provider_ids = None;
        original.item.production_year = Some(1982);
        let mut remake = tagged(2, 100, "The Thing", "unused");
        remake.item.provider_ids = None;
        remake.item.production_year = Some(2011);

        let result = label_duplicates(vec![original, remake]);

        assert_eq!(
            result
                .iter()
                .filter_map(|item| item.name.as_deref())
                .collect::<Vec<_>>(),
            vec!["The Thing", "The Thing"]
        );
    }

    #[test]
    fn duplicate_episodes_are_also_labeled_with_their_server() {
        let mut first = tagged(1, 100, "Pilot", "same");
        first.item.item_type = BaseItemKind::Episode;
        let mut second = tagged(2, 100, "Pilot", "same");
        second.item.item_type = BaseItemKind::Episode;

        let result = label_duplicates(vec![first, second]);

        assert_eq!(
            result
                .iter()
                .filter_map(|item| item.name.as_deref())
                .collect::<Vec<_>>(),
            vec!["Pilot [Server 1]", "Pilot [Server 2]"]
        );
    }
}
