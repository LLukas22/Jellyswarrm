//! Playlists and collections live on exactly one backend. Before forwarding,
//! check that every container, member and recipient ID in the request maps
//! onto the selected backend; otherwise the backend would silently drop or
//! misinterpret part of the request.
use anyhow::Result;

use crate::{
    auth::service::AuthorizationSession,
    domain::ids::ServerId,
    libraries::service::VirtualLibraryAccessScope,
    protocol::routes::{
        matches_case_insensitive, MEDIA_ID_PATH_TAGS, MEDIA_ID_QUERY_TAGS, USER_ID_QUERY_TAGS,
    },
    protocol::url::{contains_id, decoded_path_segments},
    translation::{url_processor::UrlProcessor, MixedServerSelection},
};

pub(crate) async fn validate(
    urls: &UrlProcessor,
    url: &url::Url,
    session: Option<&AuthorizationSession>,
    scope: Option<&VirtualLibraryAccessScope>,
    server_id: ServerId,
    body_media_ids: &[String],
) -> Result<()> {
    let segments = decoded_path_segments(url.path());
    let is_playlist = segments
        .iter()
        .any(|part| part.eq_ignore_ascii_case("Playlists"));
    let is_collection = segments
        .iter()
        .any(|part| part.eq_ignore_ascii_case("Collections"));
    if !is_playlist && !is_collection {
        return Ok(());
    }
    let unavailable = if is_playlist {
        MixedServerSelection::Playlist
    } else {
        MixedServerSelection::Collection
    };
    // Check container IDs even when they are malformed, since contains_id
    // intentionally only matches UUID-like path segments.
    for pair in segments.windows(2) {
        if (pair[0].eq_ignore_ascii_case("Playlists")
            || pair[0].eq_ignore_ascii_case("Collections"))
            && urls
                .mapping_on_server(&pair[1].as_str().into(), scope, Some(server_id))
                .await?
                .is_none()
        {
            return Err(unavailable.into());
        }
    }
    for tag in MEDIA_ID_PATH_TAGS {
        if let Some(id) = contains_id(url, tag) {
            if urls
                .mapping_on_server(&id.into(), scope, Some(server_id))
                .await?
                .is_none()
            {
                return Err(unavailable.into());
            }
        }
    }
    for (key, value) in url.query_pairs() {
        if matches_case_insensitive(&key, USER_ID_QUERY_TAGS) {
            urls.upstream_user_id(&value, session, server_id).await?;
        }
        if matches_case_insensitive(&key, MEDIA_ID_QUERY_TAGS) {
            for id in value.split(',').map(str::trim).filter(|id| !id.is_empty()) {
                if urls
                    .mapping_on_server(&id.into(), scope, Some(server_id))
                    .await?
                    .is_none()
                {
                    return Err(unavailable.into());
                }
            }
        }
    }
    for id in body_media_ids {
        if urls
            .mapping_on_server(&id.as_str().into(), scope, Some(server_id))
            .await?
            .is_none()
        {
            return Err(unavailable.into());
        }
    }
    // Validate even malformed recipient IDs: contains_id deliberately skips
    // non-UUID path values, which must not bypass sharing validation.
    if let Some(pair) = segments
        .windows(2)
        .find(|pair| pair[0].eq_ignore_ascii_case("Users"))
    {
        urls.upstream_user_id(&pair[1], session, server_id).await?;
    }
    Ok(())
}
