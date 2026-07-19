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

pub fn deduplicate_tagged_items(
    items: Vec<TaggedMediaItem>,
    config: &DuplicatePolicyConfig,
) -> Vec<MediaItem> {
    let mut groups: HashMap<String, Vec<TaggedMediaItem>> = HashMap::new();
    for tagged in items {
        groups
            .entry(duplicate_key(&tagged.item))
            .or_default()
            .push(tagged);
    }

    let mut winners = Vec::with_capacity(groups.len());
    for (_, group) in groups {
        winners.extend(select_from_duplicate_group(group, config));
    }

    winners
}

fn select_from_duplicate_group(
    group: Vec<TaggedMediaItem>,
    config: &DuplicatePolicyConfig,
) -> Vec<MediaItem> {
    if group.len() == 1 {
        return vec![group[0].item.clone()];
    }

    let max_score = group
        .iter()
        .map(|tagged| duplicate_preference_score(&tagged.item))
        .max()
        .unwrap_or(0);
    let mut best_matches = group
        .into_iter()
        .filter(|tagged| duplicate_preference_score(&tagged.item) == max_score)
        .collect::<Vec<_>>();

    if best_matches.len() == 1 {
        return vec![best_matches.remove(0).item];
    }

    if config.policy == DuplicatePolicy::ShowAll {
        return best_matches.into_iter().map(|tagged| tagged.item).collect();
    }

    best_matches
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

    let name = normalized_name(item);
    format!("content:{name}:{:?}", item.item_type)
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

fn duplicate_preference_score(item: &MediaItem) -> i64 {
    if item.item_type == BaseItemKind::Episode {
        if let Some(user_data) = &item.user_data {
            if user_data.playback_position_ticks > 0 {
                return user_data.playback_position_ticks;
            }
            if user_data.play_count > 0 {
                return i64::from(user_data.play_count);
            }
        }
        return 0;
    }

    item.child_count
        .map(|count| i64::from(count.max(0)))
        .or_else(|| {
            item.extra
                .get("ChildCount")
                .or_else(|| item.extra.get("childCount"))
                .and_then(|value| value.as_i64())
        })
        .unwrap_or(0)
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
        let result = deduplicate_tagged_items(
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
    fn prefers_more_complete_series() {
        let mut less: MediaItem = serde_json::from_value(serde_json::json!({
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
        let _ = &mut less;

        let result = deduplicate_tagged_items(
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
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "right");
    }
}
