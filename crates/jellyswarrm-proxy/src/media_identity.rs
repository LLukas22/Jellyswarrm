use std::collections::BTreeSet;

use crate::models::{enums::BaseItemKind, MediaItem};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaProvider {
    Tmdb,
    Imdb,
    Tvdb,
    /// Internal identity derived from a provider-matched parent series.
    SeriesGroup,
}

impl MediaProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tmdb => "tmdb",
            Self::Imdb => "imdb",
            Self::Tvdb => "tvdb",
            Self::SeriesGroup => "series-group",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "tmdb" => Some(Self::Tmdb),
            "imdb" => Some(Self::Imdb),
            "tvdb" => Some(Self::Tvdb),
            "series-group" => Some(Self::SeriesGroup),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaKind {
    Movie,
    Series,
    Season,
    Episode,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Series => "series",
            Self::Season => "season",
            Self::Episode => "episode",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "movie" => Some(Self::Movie),
            "series" => Some(Self::Series),
            "season" => Some(Self::Season),
            "episode" => Some(Self::Episode),
            _ => None,
        }
    }

    pub fn from_item_kind(kind: &BaseItemKind) -> Option<Self> {
        match kind {
            BaseItemKind::Movie => Some(Self::Movie),
            BaseItemKind::Series => Some(Self::Series),
            BaseItemKind::Season => Some(Self::Season),
            BaseItemKind::Episode => Some(Self::Episode),
            _ => None,
        }
    }

    /// Only these item kinds participate in cross-server version collapsing.
    /// Movies keep their original behavior; Jellyfin v12 adds multi-versions
    /// for episodes, and series/seasons collapse alongside them so a show
    /// present on several backends appears once.
    pub fn has_media_sources(self) -> bool {
        matches!(self, Self::Movie | Self::Episode)
    }
}

/// Conservative cross-server identity. Authoritative provider IDs
/// (Tmdb/Imdb/Tvdb), or numbered children of a provider-matched series, are
/// accepted; collection IDs and title/year guesses are
/// intentionally not safe enough to hide items or authorize playback
/// substitution. Applies to movies as well as shows (series, seasons and
/// episodes — Jellyfin v12 supports multi-versions for episodes).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MediaAlias {
    pub provider: MediaProvider,
    pub kind: MediaKind,
    pub provider_id: String,
}

impl MediaAlias {
    pub fn from_item(item: &MediaItem) -> BTreeSet<Self> {
        let Some(kind) = MediaKind::from_item_kind(&item.item_type) else {
            return BTreeSet::new();
        };
        let range_suffix = if kind == MediaKind::Episode {
            let Some(suffix) = Self::episode_range_suffix(item) else {
                return BTreeSet::new();
            };
            suffix
        } else {
            String::new()
        };

        let Some(provider_ids) = item.provider_ids.as_ref().and_then(|ids| ids.as_object()) else {
            return BTreeSet::new();
        };
        [
            (MediaProvider::Tmdb, "Tmdb"),
            (MediaProvider::Imdb, "Imdb"),
            (MediaProvider::Tvdb, "Tvdb"),
        ]
        .into_iter()
        .filter_map(|(provider, expected_key)| {
            provider_ids.iter().find_map(|(key, value)| {
                let provider_id = value.as_str()?.trim();
                (key.eq_ignore_ascii_case(expected_key) && !provider_id.is_empty()).then(|| {
                    let provider_id = match provider {
                        MediaProvider::Imdb => provider_id.to_ascii_lowercase(),
                        _ => provider_id.to_string(),
                    };
                    Self {
                        provider,
                        kind,
                        provider_id: format!("{provider_id}{range_suffix}"),
                    }
                })
            })
        })
        .collect()
    }

    /// A numbered season belongs to its matched series regardless of its
    /// localized title or missing/inconsistent season-level provider metadata.
    /// Missing numbers must not be confused with season zero (specials).
    pub fn for_season(item: &MediaItem, series_group_id: &str) -> Option<Self> {
        if item.item_type != BaseItemKind::Season {
            return None;
        }
        let number = item
            .extra
            .get("IndexNumber")
            .or_else(|| item.extra.get("indexNumber"))?
            .as_i64()?;
        if number < 0 {
            return None;
        }
        Some(Self {
            provider: MediaProvider::SeriesGroup,
            kind: MediaKind::Season,
            provider_id: format!("{series_group_id}:season:{number}"),
        })
    }

    /// Episode titles and episode-level provider metadata can differ between
    /// localized copies. Use the matched series and explicit coordinates.
    pub fn for_episode(item: &MediaItem, series_group_id: &str) -> Option<Self> {
        if item.item_type != BaseItemKind::Episode {
            return None;
        }
        let number = |upper, lower| {
            item.extra
                .get(upper)
                .or_else(|| item.extra.get(lower))?
                .as_i64()
        };
        let season = number("ParentIndexNumber", "parentIndexNumber")?;
        let episode = number("IndexNumber", "indexNumber")?;
        if season < 0 || episode < 0 {
            return None;
        }
        let range_suffix = Self::episode_range_suffix(item)?;
        Some(Self {
            provider: MediaProvider::SeriesGroup,
            kind: MediaKind::Episode,
            provider_id: format!(
                "{series_group_id}:season:{season}:episode:{episode}{range_suffix}"
            ),
        })
    }

    /// A combined episode is not a playback substitute for its first episode.
    /// Preserve legacy single-episode keys, but qualify ranges on every alias.
    fn episode_range_suffix(item: &MediaItem) -> Option<String> {
        let end = item
            .extra
            .get("IndexNumberEnd")
            .or_else(|| item.extra.get("indexNumberEnd"));
        let Some(end) = end.filter(|value| !value.is_null()) else {
            return Some(String::new());
        };
        let end = end.as_i64()?;
        let start = item
            .extra
            .get("IndexNumber")
            .or_else(|| item.extra.get("indexNumber"))?
            .as_i64()?;
        if start < 0 || end < start {
            return None;
        }
        Some(if end == start {
            String::new()
        } else {
            format!(":range:{start}-{end}")
        })
    }
}

impl MediaAlias {
    /// Storage representation for the `provider` DB column. New rows are
    /// qualified with the media kind (`tmdb:series`) so a movie and a show
    /// sharing the same numeric provider ID never merge. Legacy rows written
    /// before show support contain only `tmdb`/`imdb`/`tvdb` and are read
    /// back as movies.
    pub fn storage_provider(&self) -> String {
        format!("{}:{}", self.provider.as_str(), self.kind.as_str())
    }

    pub fn parse_storage(provider: &str, provider_id: &str) -> Option<Self> {
        if let Some((provider_part, kind_part)) = provider.split_once(':') {
            Some(Self {
                provider: MediaProvider::parse(provider_part)?,
                kind: MediaKind::parse(kind_part)?,
                provider_id: provider_id.to_string(),
            })
        } else {
            Some(Self {
                provider: MediaProvider::parse(provider)?,
                kind: MediaKind::Movie,
                provider_id: provider_id.to_string(),
            })
        }
    }
}

#[derive(Debug, Clone)]
pub struct MediaObservation {
    pub virtual_media_id: String,
    pub aliases: BTreeSet<MediaAlias>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StableMediaGroup {
    pub virtual_media_id: String,
    pub active_member_count: usize,
    pub ambiguous: bool,
    pub published: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn combined_episode_ranges_never_alias_single_episodes() {
        let item = |end| {
            serde_json::from_value::<MediaItem>(json!({"Id": "episode", "Type": "Episode", "ParentIndexNumber": 1, "IndexNumber": 1, "IndexNumberEnd": end, "ProviderIds": {"Tvdb": "42"}})).unwrap()
        };
        let single = item(json!(null));
        assert_eq!(
            MediaAlias::for_episode(&single, "show"),
            MediaAlias::for_episode(&item(json!(1)), "show")
        );
        assert_ne!(
            MediaAlias::for_episode(&single, "show"),
            MediaAlias::for_episode(&item(json!(2)), "show")
        );
        assert!(MediaAlias::from_item(&single).is_disjoint(&MediaAlias::from_item(&item(json!(2)))));
        assert_ne!(
            MediaAlias::for_episode(&item(json!(2)), "show"),
            MediaAlias::for_episode(&item(json!(3)), "show")
        );
        for end in [json!(0), json!(-1), json!("2"), json!(1.5)] {
            assert!(MediaAlias::for_episode(&item(end.clone()), "show").is_none());
            assert!(MediaAlias::from_item(&item(end)).is_empty());
        }
    }

    #[test]
    fn episode_identity_requires_explicit_coordinates_and_matched_series() {
        let item = |name, season, episode| {
            serde_json::from_value::<MediaItem>(json!({
                "Id": name, "Type": "Episode", "Name": name,
                "ParentIndexNumber": season, "IndexNumber": episode
            }))
            .unwrap()
        };
        let english = item("Rebirth", json!(1), json!(1));
        let german = item("Wiedergeburt", json!(1), json!(1));
        let alias = MediaAlias::for_episode(&english, "show").unwrap();
        assert_eq!(
            Some(alias.clone()),
            MediaAlias::for_episode(&german, "show")
        );
        assert_ne!(
            Some(alias.clone()),
            MediaAlias::for_episode(&german, "other")
        );
        assert_ne!(
            Some(alias.clone()),
            MediaAlias::for_episode(&item("Next", json!(1), json!(2)), "show")
        );
        for invalid in [json!(null), json!(-1), json!("1"), json!(1.5)] {
            assert_eq!(
                MediaAlias::for_episode(&item("Invalid", invalid.clone(), json!(1)), "show"),
                None
            );
            assert_eq!(
                MediaAlias::for_episode(&item("Invalid", json!(1), invalid), "show"),
                None
            );
        }
        assert!(MediaAlias::for_episode(&item("Special", json!(0), json!(1)), "show").is_some());
        assert_eq!(
            MediaAlias::parse_storage(&alias.storage_provider(), &alias.provider_id),
            Some(alias)
        );
    }

    #[test]
    fn season_identity_uses_parent_and_number_not_name_or_provider_ids() {
        let season = |name, number, ids| {
            serde_json::from_value::<MediaItem>(json!({
                "Id": name, "Type": "Season", "Name": name,
                "IndexNumber": number, "ProviderIds": ids
            }))
            .unwrap()
        };
        let english = season("Season 1", 1, json!({"Tmdb": "100"}));
        let german = season("Staffel 1", 1, json!({"Imdb": "tt200"}));
        let alias = MediaAlias::for_season(&english, "show-a").unwrap();
        assert_eq!(
            Some(alias.clone()),
            MediaAlias::for_season(&german, "show-a")
        );
        assert_ne!(
            Some(alias.clone()),
            MediaAlias::for_season(&german, "show-b")
        );
        assert_ne!(
            Some(alias.clone()),
            MediaAlias::for_season(&season("Season 2", 2, json!({"Tmdb": "100"})), "show-a")
        );
        assert_eq!(
            MediaAlias::parse_storage(&alias.storage_provider(), &alias.provider_id),
            Some(alias)
        );
    }

    #[test]
    fn unknown_or_invalid_season_numbers_are_not_specials() {
        for number in [json!(null), json!(-1), json!("1"), json!(1.5)] {
            let item = serde_json::from_value(json!({
                "Id": "season", "Type": "Season", "IndexNumber": number
            }))
            .unwrap();
            assert_eq!(MediaAlias::for_season(&item, "show"), None);
        }
        let missing = serde_json::from_value(json!({"Id": "season", "Type": "Season"})).unwrap();
        assert_eq!(MediaAlias::for_season(&missing, "show"), None);
        let specials =
            serde_json::from_value(json!({"Id": "season", "Type": "Season", "indexNumber": 0}))
                .unwrap();
        assert_eq!(
            MediaAlias::for_season(&specials, "show")
                .unwrap()
                .provider_id,
            "show:season:0"
        );
    }
}
