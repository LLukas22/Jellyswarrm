use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::{
    models::{enums::BaseItemKind, MediaItem},
    server_id::ServerId,
    server_storage::Server,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub enum DuplicatePolicy {
    #[default]
    ShowAll,
    LargestSize,
    SmallestSize,
    BestQuality,
    LowestQuality,
    PreferServer,
    ServerPriority,
}

impl DuplicatePolicy {
    pub fn label(self) -> &'static str {
        match self {
            DuplicatePolicy::ShowAll => "Show all duplicates",
            DuplicatePolicy::LargestSize => "Keep largest file",
            DuplicatePolicy::SmallestSize => "Keep smallest file",
            DuplicatePolicy::BestQuality => "Keep best quality",
            DuplicatePolicy::LowestQuality => "Keep lowest quality",
            DuplicatePolicy::PreferServer => "Prefer selected server",
            DuplicatePolicy::ServerPriority => "Prefer highest server priority",
        }
    }
}

impl FromStr for DuplicatePolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "ShowAll" => Ok(DuplicatePolicy::ShowAll),
            "LargestSize" => Ok(DuplicatePolicy::LargestSize),
            "SmallestSize" => Ok(DuplicatePolicy::SmallestSize),
            "BestQuality" => Ok(DuplicatePolicy::BestQuality),
            "LowestQuality" => Ok(DuplicatePolicy::LowestQuality),
            "PreferServer" => Ok(DuplicatePolicy::PreferServer),
            "ServerPriority" => Ok(DuplicatePolicy::ServerPriority),
            _ => Err(format!("Invalid duplicate policy: {value}")),
        }
    }
}

impl fmt::Display for DuplicatePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Clone)]
pub struct DuplicatePolicyConfig {
    pub policy: DuplicatePolicy,
    pub preferred_server_id: Option<ServerId>,
}

#[derive(Debug, Clone)]
pub struct TaggedMediaItem {
    pub item: MediaItem,
    pub server: Server,
}

pub fn apply_duplicate_policy(
    items: Vec<TaggedMediaItem>,
    config: &DuplicatePolicyConfig,
) -> Vec<MediaItem> {
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

    groups
        .into_iter()
        .flat_map(|group| select_from_duplicate_group(group, config))
        .collect()
}

fn select_from_duplicate_group(
    group: Vec<TaggedMediaItem>,
    config: &DuplicatePolicyConfig,
) -> Vec<MediaItem> {
    if group.len() == 1 {
        return group.into_iter().map(|tagged| tagged.item).collect();
    }

    if config.policy == DuplicatePolicy::ShowAll {
        return group.into_iter().map(item_with_server_suffix).collect();
    }

    group
        .into_iter()
        .max_by(|left, right| {
            compare_for_policy(config, left, right).then_with(|| left.item.id.cmp(&right.item.id))
        })
        .map(|tagged| vec![tagged.item])
        .unwrap_or_default()
}

fn compare_for_policy(
    config: &DuplicatePolicyConfig,
    left: &TaggedMediaItem,
    right: &TaggedMediaItem,
) -> std::cmp::Ordering {
    match config.policy {
        DuplicatePolicy::ShowAll => std::cmp::Ordering::Equal,
        DuplicatePolicy::LargestSize => media_size(&left.item).cmp(&media_size(&right.item)),
        DuplicatePolicy::SmallestSize => media_size(&right.item).cmp(&media_size(&left.item)),
        DuplicatePolicy::BestQuality => quality_score(&left.item).cmp(&quality_score(&right.item)),
        DuplicatePolicy::LowestQuality => {
            quality_score(&right.item).cmp(&quality_score(&left.item))
        }
        DuplicatePolicy::PreferServer => prefer_server(left, right, config.preferred_server_id),
        DuplicatePolicy::ServerPriority => left
            .server
            .priority
            .cmp(&right.server.priority)
            .then_with(|| left.server.id.as_i64().cmp(&right.server.id.as_i64())),
    }
}

fn prefer_server(
    left: &TaggedMediaItem,
    right: &TaggedMediaItem,
    preferred_server_id: Option<ServerId>,
) -> std::cmp::Ordering {
    let Some(preferred) = preferred_server_id else {
        return left
            .server
            .priority
            .cmp(&right.server.priority)
            .then_with(|| left.server.id.as_i64().cmp(&right.server.id.as_i64()));
    };

    let left_matches = left.server.id == preferred;
    let right_matches = right.server.id == preferred;
    match (left_matches, right_matches) {
        (true, false) => std::cmp::Ordering::Greater,
        (false, true) => std::cmp::Ordering::Less,
        _ => left
            .server
            .priority
            .cmp(&right.server.priority)
            .then_with(|| media_size(&left.item).cmp(&media_size(&right.item))),
    }
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
    if matches!(
        tagged.item.item_type,
        BaseItemKind::Movie | BaseItemKind::Series
    ) {
        if let Some(name) = tagged.item.name.as_mut() {
            *name = format!("{name} [{}]", tagged.server.name);
        }
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

fn media_size(item: &MediaItem) -> i64 {
    if let Some(sources) = &item.media_sources {
        return sources.iter().filter_map(|source| source.size).sum();
    }

    item.extra
        .get("Size")
        .or_else(|| item.extra.get("size"))
        .and_then(|value| value.as_i64())
        .unwrap_or(0)
}

fn quality_score(item: &MediaItem) -> i64 {
    let mut best = 0i64;

    if let Some(sources) = &item.media_sources {
        for source in sources {
            if let Some(bitrate) = source.bitrate {
                best = best.max(bitrate);
            }
            if let Some(streams) = &source.media_streams {
                for stream in streams {
                    if stream.stream_type.as_deref() == Some("Video") {
                        if let Some(height) = stream.height {
                            best = best.max(i64::from(height) * 1_000_000);
                        }
                        if let Some(bit_rate) = stream.bit_rate {
                            best = best.max(bit_rate);
                        }
                    }
                }
            }
        }
    }

    if best > 0 {
        return best;
    }

    if let Some(streams) = &item.media_streams {
        for stream in streams {
            if stream.stream_type.as_deref() == Some("Video") {
                if let Some(height) = stream.height {
                    best = best.max(i64::from(height) * 1_000_000);
                }
                if let Some(bit_rate) = stream.bit_rate {
                    best = best.max(bit_rate);
                }
            }
        }
    }

    best.max(media_size(item))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::MediaStreamingMode, server_url::ServerUrl};

    fn tagged(
        server_id: i64,
        priority: i32,
        name: &str,
        size: i64,
        provider: &str,
    ) -> TaggedMediaItem {
        let item: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": format!("{server_id}-{name}"),
            "Name": name,
            "Type": "Movie",
            "ProviderIds": { "Tmdb": provider },
            "MediaSources": [{ "Id": "1", "Size": size }]
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
    fn keeps_largest_duplicate() {
        let items = vec![
            tagged(1, 100, "Movie", 1000, "abc"),
            tagged(2, 100, "Movie", 5000, "abc"),
        ];
        let result = apply_duplicate_policy(
            items,
            &DuplicatePolicyConfig {
                policy: DuplicatePolicy::LargestSize,
                preferred_server_id: None,
            },
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "2-Movie");
    }

    #[test]
    fn show_all_keeps_every_series_copy_and_adds_server_name() {
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
        let result = apply_duplicate_policy(
            vec![
                TaggedMediaItem {
                    item: less,
                    server: tagged(1, 100, "x", 1, "abc").server,
                },
                TaggedMediaItem {
                    item: more,
                    server: tagged(2, 50, "x", 1, "abc").server,
                },
            ],
            &DuplicatePolicyConfig {
                policy: DuplicatePolicy::ShowAll,
                preferred_server_id: None,
            },
        );
        assert_eq!(result.len(), 2);
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
        let result = apply_duplicate_policy(
            vec![
                tagged(1, 100, "Crash", 1_000, "1996"),
                tagged(2, 100, "Crash", 2_000, "2004"),
            ],
            &DuplicatePolicyConfig {
                policy: DuplicatePolicy::ServerPriority,
                preferred_server_id: None,
            },
        );

        assert_eq!(result.len(), 2);
    }

    #[test]
    fn same_title_with_different_production_years_is_not_a_duplicate() {
        let mut original = tagged(1, 100, "The Thing", 1_000, "unused");
        original.item.provider_ids = None;
        original.item.production_year = Some(1982);
        let mut remake = tagged(2, 100, "The Thing", 2_000, "unused");
        remake.item.provider_ids = None;
        remake.item.production_year = Some(2011);

        let result = apply_duplicate_policy(
            vec![original, remake],
            &DuplicatePolicyConfig {
                policy: DuplicatePolicy::LargestSize,
                preferred_server_id: None,
            },
        );

        assert_eq!(result.len(), 2);
    }

    #[test]
    fn configured_policy_is_not_overridden_by_child_count() {
        let mut smaller = tagged(1, 100, "Movie", 1_000, "same");
        smaller.item.child_count = Some(100);
        let mut larger = tagged(2, 100, "Movie", 5_000, "same");
        larger.item.child_count = Some(1);

        let result = apply_duplicate_policy(
            vec![smaller, larger],
            &DuplicatePolicyConfig {
                policy: DuplicatePolicy::LargestSize,
                preferred_server_id: None,
            },
        );

        assert_eq!(result[0].id, "2-Movie");
    }
}
