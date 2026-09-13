use crate::{virtual_library_service::VirtualLibraryAccessScope, AppState};
use axum::http::StatusCode;

/// Uses the existing virtual-media resolver and local authorization mappings only.
/// Backend library restrictions are still enforced when the target requests playback.
pub(crate) async fn user_has_media_access(
    state: &AppState,
    user_id: &str,
    items: &[String],
    source_id: Option<&str>,
) -> Result<bool, StatusCode> {
    if items.is_empty() {
        return Ok(true);
    }
    let Some((_, sessions)) = state
        .user_authorization
        .get_user_sessions_by_user_id(user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Ok(false);
    };
    let scope =
        VirtualLibraryAccessScope::new(user_id.to_string(), sessions.iter().map(|(_, s)| s.id));
    for item in items {
        if state
            .processors
            .url_processor
            .client_media_mapping(item, Some(&scope), None)
            .await
            .is_none()
        {
            return Ok(false);
        }
    }
    if let Some(source) = source_id {
        // A source selection applies to the first item; the queue retains virtual IDs.
        if let Some(group) = state
            .media_storage
            .get_media_version_group(&items[0])
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        {
            let Some(route) = state
                .media_storage
                .get_media_version_source_route(group.id, source)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            else {
                return Ok(false);
            };
            if !sessions
                .iter()
                .any(|(_, server)| server.id == route.member_mapping.server_id)
            {
                return Ok(false);
            }
        } else {
            let Some(item) = state
                .processors
                .url_processor
                .client_media_mapping(&items[0], Some(&scope), None)
                .await
            else {
                return Ok(false);
            };
            if state
                .processors
                .url_processor
                .client_media_mapping(source, Some(&scope), Some(item.server_id))
                .await
                .is_none()
            {
                return Ok(false);
            }
        }
    }
    Ok(true)
}
