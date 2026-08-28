use std::collections::{BTreeSet, HashMap, HashSet};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MovieProvider {
    Tmdb,
    Imdb,
    Tvdb,
}

impl MovieProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tmdb => "tmdb",
            Self::Imdb => "imdb",
            Self::Tvdb => "tvdb",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "tmdb" => Some(Self::Tmdb),
            "imdb" => Some(Self::Imdb),
            "tvdb" => Some(Self::Tvdb),
            _ => None,
        }
    }
}

/// Conservative cross-server identity. Only authoritative movie provider IDs
/// are accepted; collection IDs and title/year guesses are intentionally not
/// safe enough to hide items or authorize playback substitution.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MovieAlias {
    pub provider: MovieProvider,
    pub provider_id: String,
}

impl MovieAlias {
    pub fn from_item(item: &MediaItem) -> BTreeSet<Self> {
        if item.item_type != BaseItemKind::Movie {
            return BTreeSet::new();
        }

        let Some(provider_ids) = item.provider_ids.as_ref().and_then(|ids| ids.as_object()) else {
            return BTreeSet::new();
        };
        [
            (MovieProvider::Tmdb, "Tmdb"),
            (MovieProvider::Imdb, "Imdb"),
            (MovieProvider::Tvdb, "Tvdb"),
        ]
        .into_iter()
        .filter_map(|(provider, expected_key)| {
            provider_ids.iter().find_map(|(key, value)| {
                let provider_id = value.as_str()?.trim();
                (key.eq_ignore_ascii_case(expected_key) && !provider_id.is_empty()).then(|| {
                    let provider_id = match provider {
                        MovieProvider::Imdb => provider_id.to_ascii_lowercase(),
                        MovieProvider::Tmdb | MovieProvider::Tvdb => provider_id.to_string(),
                    };
                    Self {
                        provider,
                        provider_id,
                    }
                })
            })
        })
        .collect()
    }
}

#[derive(Debug, Clone)]
pub struct MovieObservation {
    pub virtual_media_id: String,
    pub aliases: BTreeSet<MovieAlias>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StableMovieGroup {
    pub virtual_media_id: String,
    pub active_member_count: usize,
    pub ambiguous: bool,
    pub published: bool,
}

#[derive(Debug)]
struct CatalogGroup {
    has_movie_aliases: bool,
    members: Vec<TaggedMediaItem>,
}

/// Pure catalog plan. Database-backed stable group IDs are applied only after
/// all observations have been reconciled.
#[derive(Debug)]
pub struct MovieDedupPlan {
    groups: Vec<CatalogGroup>,
}

impl MovieDedupPlan {
    pub fn new(items: Vec<TaggedMediaItem>) -> Self {
        let aliases = items
            .iter()
            .map(|tagged| MovieAlias::from_item(&tagged.item))
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
                has_movie_aliases: !aliases[root].is_empty(),
                members,
            })
            .collect();

        Self { groups }
    }

    pub fn collapse(self, stable_groups: &HashMap<String, StableMovieGroup>) -> Vec<MediaItem> {
        self.groups
            .into_iter()
            .flat_map(|group| {
                if !group.has_movie_aliases {
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
                        vec![merge_movie_group(
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

fn merge_movie_group(members: Vec<TaggedMediaItem>, group_id: &str) -> MediaItem {
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
        .expect("movie group is never empty");

    best.item.id = group_id.to_string();
    best.item.media_source_count = Some(media_source_count);
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

    fn stable_group(member_count: usize) -> StableMovieGroup {
        StableMovieGroup {
            virtual_media_id: "aggregate-id".to_string(),
            active_member_count: member_count,
            ambiguous: false,
            published: member_count > 1,
        }
    }

    fn assignments(ids: &[&str], member_count: usize) -> HashMap<String, StableMovieGroup> {
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
        let plan = MovieDedupPlan::new(vec![
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
        let merged = MovieDedupPlan::new(vec![
            tagged(3, 100, "Same Movie", "same"),
            tagged(1, 100, "Same Movie", "same"),
        ])
        .collapse(&assignments);

        assert_eq!(merged[0].server_id, None);
        assert_eq!(merged[0].id, "aggregate-id");
        // Server 1 wins the canonical name/id tie break, irrespective of input.
        assert_eq!(merged[0].name.as_deref(), Some("Same Movie"));

        let merged_again = MovieDedupPlan::new(vec![
            tagged(1, 100, "Same Movie", "same"),
            tagged(3, 100, "Same Movie", "same"),
        ])
        .collapse(&assignments);

        assert_eq!(merged_again[0].id, merged[0].id);
    }

    #[test]
    fn same_server_copies_are_never_collapsed() {
        let assignments = assignments(&["1-Same Movie"], 3);
        let plan = MovieDedupPlan::new(vec![
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
        let merged = MovieDedupPlan::new(vec![tagged(1, 100, "Same Movie", "same")])
            .collapse(&assignments(&["1-Same Movie"], 2));

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "aggregate-id");
    }

    #[test]
    fn collapse_requires_every_visible_member_to_have_the_same_stable_group() {
        let plan = MovieDedupPlan::new(vec![
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

        let plan = MovieDedupPlan::new(vec![
            tagged(1, 100, "Same Movie", "same"),
            tagged(2, 100, "Same Movie", "same"),
        ]);
        stable_groups.remove("2-Same Movie");
        assert_eq!(plan.collapse(&stable_groups).len(), 2);
    }

    #[test]
    fn only_authoritative_movie_provider_ids_create_an_identity() {
        let collection_only: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": "a",
            "Type": "Movie",
            "Name": "Sequel",
            "ProductionYear": 2026,
            "ProviderIds": { "TmdbCollection": "franchise" }
        }))
        .unwrap();
        assert!(MovieAlias::from_item(&collection_only).is_empty());

        let provider_backed: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": "b",
            "Type": "Movie",
            "ProviderIds": { "Imdb": "tt123", "Tmdb": "42" }
        }))
        .unwrap();
        assert_eq!(
            MovieAlias::from_item(&provider_backed),
            BTreeSet::from([
                MovieAlias {
                    provider: MovieProvider::Imdb,
                    provider_id: "tt123".to_string(),
                },
                MovieAlias {
                    provider: MovieProvider::Tmdb,
                    provider_id: "42".to_string(),
                },
            ])
        );
    }

    #[test]
    fn unidentified_movies_have_no_authoritative_aliases() {
        let unidentified: MediaItem = serde_json::from_value(serde_json::json!({
            "Id": "unidentified",
            "Type": "Movie",
            "Name": "No provider"
        }))
        .unwrap();
        assert!(MovieAlias::from_item(&unidentified).is_empty());
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
        let plan = MovieDedupPlan::new(vec![
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
}
