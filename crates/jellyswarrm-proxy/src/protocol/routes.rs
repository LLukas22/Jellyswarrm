//! Jellyfin route/query identity conventions shared by routing and translation.
pub(crate) static MEDIA_ID_PATH_TAGS: &[&str] = &[
    "Items",
    "Items/RemoteSearch/Apply",
    "Audio",
    "Shows",
    "Videos",
    "Playlists",
    "Collections",
    "PlayedItems",
    "FavoriteItems",
    "MediaSegments",
    "PlayingItems",
    "Recordings",
    "Channels",
    "Programs",
    "SeriesTimers",
    "Timers",
    "UserFavoriteItems",
    "UserItems",
    "UserPlayedItems",
];

// Image Tag values are opaque cache tokens, not media identities.
pub(crate) static MEDIA_ID_QUERY_TAGS: &[&str] = &[
    "AlbumId",
    "AlbumIds",
    "ParentId",
    "ItemId",
    "SeriesId",
    "LibrarySeriesId",
    "ChannelIds",
    "ProgramId",
    "GenreIds",
    "StudioIds",
    "ExcludeItemIds",
    "SeriesTimerId",
    "MediaSourceId",
    "SeasonId",
    "startItemId",
    "IDs",
    "EntryIds",
    "PlaylistItemId",
    "PlaylistItemIds",
    "PersonIds",
    "ArtistIds",
    "ContributingArtistIds",
    "AlbumArtistIds",
    "ExcludeArtistIds",
];
pub(crate) static API_KEY_QUERY_TAGS: &[&str] = &["api_key", "ApiKey"];
pub(crate) static USER_ID_PATH_TAGS: &[&str] = &["Users"];
pub(crate) static USER_ID_QUERY_TAGS: &[&str] = &["UserId"];

pub(crate) fn matches_case_insensitive(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}
