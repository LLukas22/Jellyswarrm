use std::collections::HashMap;

use crate::{
    models::{enums::BaseItemKind, MediaItem},
    server_storage::Server,
    virtual_library_service::compare_virtual_library_routes,
};

#[derive(Debug, Clone)]
pub struct TaggedMediaItem {
    pub item: MediaItem,
    pub server: Server,
}

/// Reconcile items that federated servers reported for the same merged library.
///
/// When `deduplicate` is true, content that exists on more than one server is
/// collapsed to a single entry (the highest-priority server's copy). When it is
/// false, every copy is kept but disambiguated with a `[Server]` suffix so the
/// duplicates remain distinguishable in the client.
pub fn label_duplicates(items: Vec<TaggedMediaItem>, deduplicate: bool) -> Vec<MediaItem> {
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
        .flat_map(|group| label_duplicate_group(group, deduplicate))
        .collect()
}

fn label_duplicate_group(group: Vec<TaggedMediaItem>, deduplicate: bool) -> Vec<MediaItem> {
    // A single occurrence is passed through untouched.
    if group.len() == 1 {
        return group.into_iter().map(|tagged| tagged.item).collect();
    }

    if !deduplicate {
        // Deduplication disabled: keep every copy but tag it with its server so
        // the client can still tell the otherwise-identical rows apart.
        return group.into_iter().map(item_with_server_suffix).collect();
    }

    // The same content lives on more than one server. Collapse it to a single
    // entry so the merged library shows one row instead of one per server.
    //
    // The winner is chosen deterministically with the same ordering the merged
    // library folders already use (highest server priority, then a stable
    // tiebreak on server name/id) so content rows and library folders agree on
    // which upstream a duplicate resolves to. Each item's Id is already a
    // per-server virtual id, so the winner still routes to its own upstream for
    // playback; the dropped duplicates' mappings simply go unused.
    group
        .into_iter()
        .max_by(|left, right| {
            compare_virtual_library_routes(
                &left.server,
                left.item.id.as_str(),
                &right.server,
                right.item.id.as_str(),
            )
        })
        .map(|winner| vec![winner.item])
        .unwrap_or_default()
}

fn item_with_server_suffix(mut tagged: TaggedMediaItem) -> MediaItem {
    if let Some(name) = tagged.item.name.as_mut() {
        *name = format!("{name} [{}]", tagged.server.name);
    }
    tagged.item
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
    fn duplicates_collapse_to_the_highest_priority_server() {
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
        // Deliberately list the lower-priority server first to prove the winner
        // is chosen by priority, not encounter order.
        let result = label_duplicates(
            vec![
                TaggedMediaItem {
                    item: more,
                    server: tagged(2, 50, "x", "abc").server,
                },
                TaggedMediaItem {
                    item: less,
                    server: tagged(1, 100, "x", "abc").server,
                },
            ],
            true,
        );

        // One row survives, unlabeled, and it is the higher-priority server's copy.
        assert_eq!(
            result
                .iter()
                .filter_map(|item| item.name.as_deref())
                .collect::<Vec<_>>(),
            vec!["Wistoria"]
        );
        assert_eq!(
            result
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["left"]
        );
    }

    #[test]
    fn equal_priority_duplicates_collapse_deterministically() {
        // When priorities tie, selection must still be stable across runs and
        // independent of input order (matches library-folder winner selection).
        let first_order = label_duplicates(
            vec![
                tagged(1, 100, "Dune", "shared"),
                tagged(2, 100, "Dune", "shared"),
            ],
            true,
        );
        let second_order = label_duplicates(
            vec![
                tagged(2, 100, "Dune", "shared"),
                tagged(1, 100, "Dune", "shared"),
            ],
            true,
        );

        assert_eq!(first_order.len(), 1);
        assert_eq!(second_order.len(), 1);
        assert_eq!(first_order[0].id, second_order[0].id);
    }

    #[test]
    fn same_title_with_different_provider_ids_is_not_a_duplicate() {
        let result = label_duplicates(
            vec![
                tagged(1, 100, "Crash", "1996"),
                tagged(2, 100, "Crash", "2004"),
            ],
            true,
        );

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

        let result = label_duplicates(vec![original, remake], true);

        assert_eq!(
            result
                .iter()
                .filter_map(|item| item.name.as_deref())
                .collect::<Vec<_>>(),
            vec!["The Thing", "The Thing"]
        );
    }

    #[test]
    fn duplicate_episodes_also_collapse_to_a_single_entry() {
        let mut first = tagged(2, 50, "Pilot", "same");
        first.item.item_type = BaseItemKind::Episode;
        let mut second = tagged(1, 100, "Pilot", "same");
        second.item.item_type = BaseItemKind::Episode;

        let result = label_duplicates(vec![first, second], true);

        // Collapses to the higher-priority server's episode, unlabeled.
        assert_eq!(
            result
                .iter()
                .filter_map(|item| item.name.as_deref())
                .collect::<Vec<_>>(),
            vec!["Pilot"]
        );
        assert_eq!(
            result
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["1-Pilot"]
        );
    }

    #[test]
    fn duplicates_are_kept_and_labeled_when_dedup_disabled() {
        // With deduplication turned off we fall back to the previous behavior:
        // keep every copy, disambiguated by server name.
        let result = label_duplicates(
            vec![
                tagged(1, 100, "Wistoria", "abc"),
                tagged(2, 50, "Wistoria", "abc"),
            ],
            false,
        );

        assert_eq!(
            result
                .iter()
                .filter_map(|item| item.name.as_deref())
                .collect::<Vec<_>>(),
            vec!["Wistoria [Server 1]", "Wistoria [Server 2]"]
        );
    }
}
