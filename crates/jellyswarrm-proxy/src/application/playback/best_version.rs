//! Default owner for a merged item when `prefer_best_version` is enabled and a
//! client starts playback without naming a version. Explicit sources and
//! playback bindings remain authoritative.
use crate::{
    application::{
        error::ApplicationError,
        media_services::MediaServices,
        request::context::{AuthorizedTarget, RequestContext},
    },
    domain::ids::VirtualItemId,
};

/// The server that held the best version the last time this item's detail
/// response was merged. Falls back to normal routing (`None`) when the hint is
/// unknown, the server no longer hosts a member, it is unhealthy, or the
/// caller has no session on it.
pub(crate) async fn preferred_target(
    state: &MediaServices,
    context: &RequestContext,
    item: &VirtualItemId,
) -> Result<Option<AuthorizedTarget>, ApplicationError> {
    if !state.prefer_best_version_enabled().await {
        return Ok(None);
    }
    let Some(group) = state
        .media_storage
        .get_media_version_group(item)
        .await
        .map_err(ApplicationError::Storage)?
    else {
        return Ok(None);
    };
    let Some(server_id) = state.version_hints.best_server(group.id).await else {
        return Ok(None);
    };
    let members = state
        .media_storage
        .get_media_version_members_by_virtual_id(item)
        .await
        .map_err(ApplicationError::Storage)?;
    if !members.iter().any(|member| member.server.id == server_id)
        || !state
            .server_storage
            .server_status(server_id)
            .await
            .is_healthy()
    {
        return Ok(None);
    }
    Ok(context.target(server_id).ok())
}
