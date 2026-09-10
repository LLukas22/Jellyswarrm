use std::collections::{HashMap, HashSet};

use crate::{
    media_identity::{MediaAlias, MediaKind, StableMediaGroup},
    models::{enums::BaseItemKind, MediaItem},
    server_storage::Server,
    virtual_library_service::compare_virtual_library_routes,
};

#[derive(Debug, Clone)]
pub struct TaggedMediaItem {
    pub item: MediaItem,
    pub server: Server,
}

#[derive(Debug)]
struct CatalogGroup {
    has_media_aliases: bool,
    members: Vec<TaggedMediaItem>,
}

/// Pure catalog plan. Database-backed stable group IDs are applied only after
/// all observations have been reconciled.
#[derive(Debug)]
pub struct MediaDedupPlan {
    groups: Vec<CatalogGroup>,
}

impl MediaDedupPlan {
    pub fn new(items: Vec<TaggedMediaItem>) -> Self {
        let aliases = items
            .iter()
            .map(|tagged| MediaAlias::from_item(&tagged.item))
            .collect::<Vec<_>>();
        let mut parents = (0..items.len()).collect::<Vec<_>>();
        let mut alias_owners = HashMap::new();
        let mut fallback_owners = HashMap::new();
        for (index, (tagged, item_aliases)) in items.iter().zip(&aliases).enumerate() {
            if item_aliases.is_empty() {
                let key = duplicate_key(&tagged.item);
                if let Some(owner) = fallback_owners.insert(key, index) {
                    union(&mut parents, owner, index);
                }
            } else {
                for alias in item_aliases {
                    if let Some(owner) = alias_owners.insert(alias.clone(), index) {
                        union(&mut parents, owner, index);
                    }
                }
            }
        }

        let mut grouped = HashMap::<usize, Vec<TaggedMediaItem>>::new();
        for (index, tagged) in items.into_iter().enumerate() {
            let root = find(&mut parents, index);
            grouped.entry(root).or_default().push(tagged);
        }
        let mut grouped = grouped.into_iter().collect::<Vec<_>>();
        grouped.sort_by_key(|(root, _)| *root);
        let groups = grouped
            .into_iter()
            .map(|(root, members)| CatalogGroup {
                has_media_aliases: !aliases[root].is_empty(),
                members,
            })
            .collect();

        Self { groups }
    }

    pub fn collapse(self, stable_groups: &HashMap<String, StableMediaGroup>) -> Vec<MediaItem> {
        let mut groups: Vec<CatalogGroup> = Vec::new();
        let mut stable_indexes = HashMap::new();
        for group in self.groups {
            let stable_group = group
                .members
                .iter()
                .map(|member| stable_groups.get(&member.item.id))
                .collect::<Option<Vec<_>>>()
                .and_then(|groups| {
                    let first = groups.first().copied()?;
                    (group.has_media_aliases
                        && first.published
                        && !first.ambiguous
                        && groups.iter().all(|group| *group == first))
                    .then_some(first)
                });
            // The persisted alias bridge may be absent from this response.
            if let Some(stable_group) = stable_group {
                let index = *stable_indexes
                    .entry(&stable_group.virtual_media_id)
                    .or_insert(groups.len());
                if index < groups.len() {
                    groups[index].members.extend(group.members);
                    continue;
                }
            }
            groups.push(group);
        }

        groups
            .into_iter()
            .flat_map(|group| {
                if !group.has_media_aliases {
                    return label_duplicate_group(group.members);
                }
                let distinct_servers = group
                    .members
                    .iter()
                    .map(|member| member.server.id)
                    .collect::<HashSet<_>>();
                let is_unambiguous_group = distinct_servers.len() == group.members.len();

                let stable_group = group
                    .members
                    .iter()
                    .map(|member| stable_groups.get(&member.item.id))
                    .collect::<Option<Vec<_>>>()
                    .and_then(|groups| {
                        let first = groups.first().copied()?;
                        groups.iter().all(|group| *group == first).then_some(first)
                    });
                match (is_unambiguous_group, stable_group) {
                    (true, Some(stable_group))
                        if stable_group.published && !stable_group.ambiguous =>
                    {
                        vec![merge_media_group(
                            group.members,
                            &stable_group.virtual_media_id,
                        )]
                    }
                    _ => label_duplicate_group(group.members),
                }
            })
            .collect()
    }
}

fn find(parents: &mut [usize], index: usize) -> usize {
    if parents[index] != index {
        parents[index] = find(parents, parents[index]);
    }
    parents[index]
}

fn union(parents: &mut [usize], left: usize, right: usize) {
    let left = find(parents, left);
    let right = find(parents, right);
    if left != right {
        let (root, child) = if left < right {
            (left, right)
        } else {
            (right, left)
        };
        parents[child] = root;
    }
}

fn merge_media_group(members: Vec<TaggedMediaItem>, group_id: &str) -> MediaItem {
    let advertises_versions = members.iter().any(|member| {
        MediaKind::from_item_kind(&member.item.item_type)
            .is_some_and(|kind| kind.has_media_sources())
    });
    let media_source_count = members
        .iter()
        .map(|member| member.item.media_source_count.unwrap_or(1).max(1) as i64)
        .sum::<i64>()
        .min(i32::MAX as i64) as i32;

    let mut best = members
        .into_iter()
        .max_by(|left, right| {
            compare_virtual_library_routes(
                &left.server,
                &left.item.id,
                &right.server,
                &right.item.id,
            )
        })
        .expect("media group is never empty");

    best.item.id = group_id.to_string();
    // Movies and (Jellyfin v12+) episodes carry one MediaSource per version,
    // so the collapsed item advertises the summed count. Series/seasons have
    // no versions on the item itself — keep their original count instead of
    // inventing one.
    if advertises_versions {
        best.item.media_source_count = Some(media_source_count);
    }
    best.item
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
    for preferred in ["Tmdb", "Imdb", "Tvdb"] {
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
    use std::collections::BTreeSet;

    use crate::{
        config::MediaStreamingMode,
        media_identity::{MediaKind, MediaProvider},
        server_id::ServerId,
        server_url::ServerUrl,
    };

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
            server: server_fixture(server_id, priority),
        }
    }

    fn server_fixture(server_id: i64, priority: i32) -> Server {
        Server {
            id: ServerId::new(server_id),
            name: format!("Server {server_id}"),
            url: ServerUrl::parse("http://example:8096").unwrap(),
            priority,
            media_streaming_mode: MediaStreamingMode::Redirect,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn stable_group(member_count: usize) -> StableMediaGroup {
        StableMediaGroup {
            virtual_media_id: "aggregate-id".to_string(),
            active_member_count: member_count,
            ambiguous: false,
            published: member_count > 1,
        }
    }

    fn assignments(ids: &[&str], member_count: usize) -> HashMap<String, StableMediaGroup> {
        ids.iter()
            .map(|id| ((*id).to_string(), stable_group(member_count)))
            .collect()
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

    #[test]
    fn dedup_collapse_keeps_highest_priority_representative_and_advertises_versions() {
        let plan = MediaDedupPlan::new(vec![
            tagged(1, 50, "The Thing", "same"),
            tagged(2, 100, "The Thing", "same"),
        ]);
        let merged = plan.collapse(&assignments(&["1-The Thing", "2-The Thing"], 2));

        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].media_source_count,
            Some(2),
            "collapsed group must advertise its version count"
        );
        assert_eq!(merged[0].id, "aggregate-id");
        assert_eq!(merged[0].name.as_deref(), Some("The Thing"));
    }

    #[test]
    fn dedup_uses_the_canonical_server_order_for_equal_priorities() {
        let assignments = assignments(&["3-Same Movie", "1-Same Movie"], 2);
        let merged = MediaDedupPlan::new(vec![
            tagged(3, 100, "Same Movie", "same"),
            tagged(1, 100, "Same Movie", "same"),
        ])
        .collapse(&assignments);

        assert_eq!(merged[0].server_id, None);
        assert_eq!(merged[0].id, "aggregate-id");
        // Server 1 wins the canonical name/id tie break, irrespective of input.
        assert_eq!(merged[0].name.as_deref(), Some("Same Movie"));

        let merged_again = MediaDedupPlan::new(vec![
            tagged(1, 100, "Same Movie", "same"),
            tagged(3, 100, "Same Movie", "same"),
        ])
        .collapse(&assignments);

        assert_eq!(merged_again[0].id, merged[0].id);
    }

    #[test]
    fn same_server_copies_are_never_collapsed() {
        let assignments = assignments(&["1-Same Movie"], 3);
        let plan = MediaDedupPlan::new(vec![
            tagged(1, 100, "Same Movie", "same"),
            tagged(1, 100, "Same Movie", "same"),
        ]);
        let merged = plan.collapse(&assignments);

        assert_eq!(merged.len(), 2);
        assert_ne!(merged[0].id, "aggregate-id");
        assert_ne!(merged[1].id, "aggregate-id");
    }

    #[test]
    fn persisted_multi_server_group_keeps_aggregate_id_when_one_server_is_absent() {
        let merged = MediaDedupPlan::new(vec![tagged(1, 100, "Same Movie", "same")])
            .collapse(&assignments(&["1-Same Movie"], 2));

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "aggregate-id");
    }

    #[test]
    fn persisted_group_collapses_without_the_transitive_bridge_in_the_response() {
        let mut first = tagged(1, 50, "First", "42");
        first.item.media_source_count = Some(2);
        let mut last = tagged(3, 100, "Preferred", "unused");
        last.item.provider_ids = Some(serde_json::json!({"Imdb": "tt123"}));
        last.item.media_source_count = Some(3);
        // A persisted third member carries both Tmdb:42 and Imdb:tt123.
        let stable_groups = assignments(&["1-First", "bridge", "3-Preferred"], 3);

        for members in [vec![first.clone(), last.clone()], vec![last, first]] {
            let plan = MediaDedupPlan::new(members);
            assert_eq!(plan.groups.len(), 2);
            let merged = plan.collapse(&stable_groups);

            assert_eq!(merged.len(), 1);
            assert_eq!(merged[0].id, "aggregate-id");
            assert_eq!(merged[0].name.as_deref(), Some("Preferred"));
            assert_eq!(merged[0].media_source_count, Some(5));
            assert_eq!(
                merged[0].provider_ids,
                Some(serde_json::json!({"Imdb": "tt123"}))
            );
        }
    }

    #[test]
    fn collapse_requires_every_visible_member_to_have_the_same_stable_group() {
        let plan = MediaDedupPlan::new(vec![
            tagged(1, 100, "Same Movie", "same"),
            tagged(2, 100, "Same Movie", "same"),
        ]);
        let mut stable_groups = assignments(&["1-Same Movie", "2-Same Movie"], 2);
        stable_groups
            .get_mut("2-Same Movie")
            .unwrap()
            .virtual_media_id = "different-aggregate".to_string();

        let visible = plan.collapse(&stable_groups);

        assert_eq!(visible.len(), 2);
        assert!(visible.iter().all(|item| item.id != "aggregate-id"));

        let plan = MediaDedupPlan::new(vec![
            tagged(1, 100, "Same Movie", "same"),
            tagged(2, 100, "Same Movie", "same"),
        ]);
        stable_groups.remove("2-Same Movie");
        assert_eq!(plan.collapse(&stable_groups).len(), 2);
    }

    #[test]
    fn only_authoritative_media_provider_ids_create_an_identity() {
        let collection_only: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": "a",
            "Type": "Movie",
            "Name": "Sequel",
            "ProductionYear": 2026,
            "ProviderIds": { "TmdbCollection": "franchise" }
        }))
        .unwrap();
        assert!(MediaAlias::from_item(&collection_only).is_empty());

        let provider_backed: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": "b",
            "Type": "Movie",
            "ProviderIds": { "Imdb": "tt123", "Tmdb": "42" }
        }))
        .unwrap();
        assert_eq!(
            MediaAlias::from_item(&provider_backed),
            BTreeSet::from([
                MediaAlias {
                    provider: MediaProvider::Imdb,
                    kind: MediaKind::Movie,
                    provider_id: "tt123".to_string(),
                },
                MediaAlias {
                    provider: MediaProvider::Tmdb,
                    kind: MediaKind::Movie,
                    provider_id: "42".to_string(),
                },
            ])
        );
    }

    #[test]
    fn unidentified_media_have_no_authoritative_aliases() {
        let unidentified: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": "unidentified",
            "Type": "Movie",
            "Name": "No provider"
        }))
        .unwrap();
        assert!(MediaAlias::from_item(&unidentified).is_empty());
    }

    #[test]
    fn aliases_merge_transitively_across_provider_sets() {
        let first: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": "first", "Type": "Movie", "ProviderIds": {"Tmdb": "42"}
        }))
        .unwrap();
        let bridge: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": "bridge", "Type": "Movie", "ProviderIds": {"Tmdb": "42", "Imdb": "TT123"}
        }))
        .unwrap();
        let last: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": "last", "Type": "Movie", "ProviderIds": {"Imdb": "tt123"}
        }))
        .unwrap();
        let plan = MediaDedupPlan::new(vec![
            TaggedMediaItem {
                item: first,
                server: server_fixture(1, 100),
            },
            TaggedMediaItem {
                item: bridge,
                server: server_fixture(2, 100),
            },
            TaggedMediaItem {
                item: last,
                server: server_fixture(3, 100),
            },
        ]);

        let merged = plan.collapse(&assignments(&["first", "bridge", "last"], 3));
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "aggregate-id");
    }

    fn typed_tagged(
        server_id: i64,
        priority: i32,
        id: &str,
        item_type: &str,
        provider: &str,
    ) -> TaggedMediaItem {
        let item: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": id,
            "Name": "Show",
            "Type": item_type,
            "ProviderIds": { "Tvdb": provider }
        }))
        .unwrap();
        TaggedMediaItem {
            item,
            server: server_fixture(server_id, priority),
        }
    }

    #[test]
    fn series_with_matching_providers_collapse_to_one_item_without_version_count() {
        let plan = MediaDedupPlan::new(vec![
            typed_tagged(1, 100, "s1", "Series", "tv-1"),
            typed_tagged(2, 100, "s2", "Series", "tv-1"),
        ]);
        let merged = plan.collapse(&assignments(&["s1", "s2"], 2));

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "aggregate-id");
        // Series carry no MediaSources, so collapsing must not invent a count.
        assert_eq!(merged[0].media_source_count, None);
    }

    #[test]
    fn episodes_with_matching_providers_collapse_and_advertise_versions() {
        let plan = MediaDedupPlan::new(vec![
            typed_tagged(1, 100, "e1", "Episode", "ep-9"),
            typed_tagged(2, 100, "e2", "Episode", "ep-9"),
        ]);
        let merged = plan.collapse(&assignments(&["e1", "e2"], 2));

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "aggregate-id");
        assert_eq!(merged[0].media_source_count, Some(2));
    }

    #[test]
    fn same_provider_id_across_types_never_merges() {
        let plan = MediaDedupPlan::new(vec![
            typed_tagged(1, 100, "movie", "Movie", "42"),
            typed_tagged(2, 100, "series", "Series", "42"),
            typed_tagged(3, 100, "episode", "Episode", "42"),
        ]);
        // No stable groups: nothing may collapse across kinds.
        let merged = plan.collapse(&HashMap::new());

        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn series_aliases_carry_their_kind() {
        let series: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": "s", "Type": "Series", "ProviderIds": {"Tvdb": "abc", "Tmdb": "42"}
        }))
        .unwrap();
        assert_eq!(
            MediaAlias::from_item(&series),
            BTreeSet::from([
                MediaAlias {
                    provider: MediaProvider::Tmdb,
                    kind: MediaKind::Series,
                    provider_id: "42".to_string(),
                },
                MediaAlias {
                    provider: MediaProvider::Tvdb,
                    kind: MediaKind::Series,
                    provider_id: "abc".to_string(),
                },
            ])
        );
    }

    #[test]
    fn legacy_storage_provider_strings_read_back_as_media() {
        let alias = MediaAlias::parse_storage("tmdb", "42").unwrap();
        assert_eq!(alias.provider, MediaProvider::Tmdb);
        assert_eq!(alias.kind, MediaKind::Movie);

        let series = MediaAlias {
            provider: MediaProvider::Tvdb,
            kind: MediaKind::Series,
            provider_id: "abc".to_string(),
        };
        assert_eq!(series.storage_provider(), "tvdb:series");
        assert_eq!(
            MediaAlias::parse_storage("tvdb:series", "abc").unwrap(),
            series
        );
    }
}
