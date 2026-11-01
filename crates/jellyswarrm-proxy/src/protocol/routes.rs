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

/// Playback-report route kinds that change proxy playback-session bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlaybackSessionAction {
    Start,
    Refresh,
    Remove,
}

/// Recognize playback reports after the caller has stripped its proxy prefix.
pub(crate) fn playback_session_action(
    method: &hyper::Method,
    request_path: &str,
) -> Option<PlaybackSessionAction> {
    if method != hyper::Method::POST {
        return None;
    }

    let path = request_path.trim_end_matches('/');
    if path.eq_ignore_ascii_case("/Sessions/Playing") {
        Some(PlaybackSessionAction::Start)
    } else if path.eq_ignore_ascii_case("/Sessions/Playing/Progress") {
        Some(PlaybackSessionAction::Refresh)
    } else if path.eq_ignore_ascii_case("/Sessions/Playing/Stopped") {
        Some(PlaybackSessionAction::Remove)
    } else {
        None
    }
}
