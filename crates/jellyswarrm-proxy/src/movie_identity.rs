use std::collections::BTreeSet;

use crate::models::{enums::BaseItemKind, MediaItem};

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
