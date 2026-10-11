use axum::Json;
use hyper::StatusCode;
use serde_json::Value;

use crate::{
    handlers::common::execute_json_request,
    processors::response_processor::ResponseProcessingProfile,
    request_preprocessing::{apply_to_request, remap_authorization, PreprocessedRequest},
    url_helper::{decoded_path_segments, replace_path_id},
    virtual_library_service::compare_virtual_library_routes,
    AppState,
};

/// A watched-state action on a merged item applies to every authorized copy.
/// Preflight availability before writing; never acknowledge a partial update
/// as success. Explicit member IDs continue through the ordinary proxy.
pub(crate) async fn update_aggregate(
    state: &AppState,
    request: &PreprocessedRequest,
) -> Result<Option<Json<Value>>, StatusCode> {
    if !matches!(
        *request.original_request.method(),
        reqwest::Method::POST | reqwest::Method::DELETE
    ) {
        return Ok(None);
    }
    let segments = decoded_path_segments(
        state
            .remove_prefix_from_path(request.original_request.url().path())
            .await,
    );
    let (tag, id) = match segments.as_slice() {
        [users, _, tag, id]
            if users.eq_ignore_ascii_case("Users") && tag.eq_ignore_ascii_case("PlayedItems") =>
        {
            (tag, id)
        }
        [tag, id] if tag.eq_ignore_ascii_case("UserPlayedItems") => (tag, id),
        _ => return Ok(None),
    };
    let Some(group) = state
        .media_storage
        .get_media_version_group(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    else {
        return Ok(None);
    };
    let scope = request
        .access_scope
        .as_ref()
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let mut members = state
        .media_storage
        .get_media_version_members(group.id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    members.retain(|member| scope.allows(member.server.id));
    if members.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    members.sort_by(|left, right| {
        compare_virtual_library_routes(
            &right.server,
            &right.mapping.original_media_id,
            &left.server,
            &left.mapping.original_media_id,
        )
    });
    let mut targets = Vec::new();
    for member in members {
        let session = request
            .sessions
            .as_ref()
            .and_then(|sessions| {
                sessions
                    .iter()
                    .find(|(_, server)| server.id == member.server.id)
            })
            .map(|(session, _)| session.clone())
            .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let mut upstream = request
            .original_request
            .try_clone()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
        *upstream.url_mut() =
            replace_path_id(upstream.url(), tag, &member.mapping.original_media_id)
                .ok_or(StatusCode::BAD_REQUEST)?;
        let session = Some(session);
        let auth = remap_authorization(&request.auth, &session)
            .await
            .map_err(|_| StatusCode::UNAUTHORIZED)?;
        apply_to_request(
            &mut upstream,
            &member.server,
            &session,
            &auth,
            state,
            Some(scope),
        )
        .await;
        targets.push((upstream, member.server));
    }
    let mut result = None;
    for (upstream, server) in targets {
        let mut data: Value = execute_json_request(&state.reqwest_client, upstream).await?;
        // Also invalidate after a partial write if a later member rejects it.
        state.catalog_snapshots.invalidate_viewer(scope.user_id());
        if result.is_none() {
            state
                .process_response_json(
                    &mut data,
                    &server,
                    ResponseProcessingProfile::Media,
                    request.auth.as_ref().and_then(|auth| auth.token_ref()),
                )
                .await?;
            data.as_object_mut().ok_or(StatusCode::BAD_GATEWAY)?.insert(
                "ItemId".into(),
                Value::String(group.virtual_media_id.clone()),
            );
            result = Some(Json(data));
        }
    }
    Ok(result)
}
