use std::collections::BTreeSet;

use crate::models::{enums::BaseItemKind, MediaItem};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MediaProvider {
    Tmdb,
    Imdb,
    Tvdb,
}

impl MediaProvider {
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

/// Conservative cross-server identity. Only authoritative provider IDs
/// (Tmdb/Imdb/Tvdb) are accepted; collection IDs and title/year guesses are
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
                        MediaProvider::Tmdb | MediaProvider::Tvdb => provider_id.to_string(),
                    };
                    Self {
                        provider,
                        kind,
                        provider_id,
                    }
                })
            })
        })
        .collect()
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
