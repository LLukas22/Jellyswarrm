//! PlaybackInfo workflow: resolve owner → fetch sources → translate/reconcile → commit bindings.
//! Start with `post_playback_info_typed`; delivery is handled separately in `resources`.
use super::services::PlaybackService;
#[cfg(test)]
use super::target::ResolutionError;
#[cfg(test)]
use hyper::StatusCode;
use tracing::{debug, error, warn};

#[cfg(test)]
use crate::AppState;
use crate::{
    application::{
        error::ApplicationError,
        input::OperationRequest,
        playback::{
            response::{
                process_playback_response, remap_playback_request, track_play_session,
                track_playback_alias,
            },
            selection::record_playback_sources,
            target::{MediaTargetResolver, PlaybackIntent, SelectionPolicy},
        },
    },
    models::{PlaybackRequest, PlaybackResponse},
    upstream::jellyfin::{
        json::{payload_from_request_typed, set_json_body_typed},
        request::UpstreamRequestBuilder,
    },
    url_helper::{contains_id, replace_path_id},
};

/// Negotiate sources, register their routes, and bind the successful playback.
#[cfg(test)]
pub(crate) async fn post_playback_info(
    state: AppState,
    input: OperationRequest,
) -> Result<PlaybackResponse, StatusCode> {
    post_playback_info_typed(&state.playback_service(), input)
        .await
        .map_err(|error| error.detail_status())
}

pub(crate) async fn post_playback_info_typed(
    state: &PlaybackService<'_>,
    mut input: OperationRequest,
) -> Result<PlaybackResponse, ApplicationError> {
    // An aggregate ID means "use the member's default source", not a recorded
    // source route. Normalize this query sentinel before the initial policy
    // checks, which otherwise reject it as an unknown explicit source.
    let default_aggregate_query = match (
        contains_id(input.original_request.url(), "Items"),
        input
            .original_request
            .url()
            .query_pairs()
            .find_map(|(key, value)| {
                key.eq_ignore_ascii_case("MediaSourceId")
                    .then(|| value.into_owned())
            }),
    ) {
        (Some(item), Some(source))
            if crate::media_storage_service::MediaStorageService::normalize_uuid(&source)
                == crate::media_storage_service::MediaStorageService::normalize_uuid(&item)
                && input
                    .original_request
                    .url()
                    .query_pairs()
                    .filter(|(key, _)| key.eq_ignore_ascii_case("MediaSourceId"))
                    .all(|(_, value)| {
                        crate::media_storage_service::MediaStorageService::normalize_uuid(&value)
                            == crate::media_storage_service::MediaStorageService::normalize_uuid(
                                &item,
                            )
                    }) =>
        {
            state
                .media_storage
                .get_media_version_group(&item)
                .await
                .map_err(ApplicationError::Storage)?
                .is_some()
        }
        _ => false,
    };
    if default_aggregate_query {
        remove_source_query(&mut input.original_request);
    }
    // Keep the old early routing errors and RequireSession boundary, including
    // rejection of backend-token passthrough without a proxy device session.
    let initial = input
        .resolve_target_using(&state.routing_services())
        .await?;
    if initial.target.session().is_none() {
        return Err(ApplicationError::MissingIdentity("proxy playback session")
            .at(crate::application::error::FailureStage::CallerAuthentication));
    }
    let proxy_api_key = input
        .context
        .authorization()
        .and_then(|auth| auth.token_ref())
        .map(str::to_string);
    let payload: PlaybackRequest = payload_from_request_typed(&input.original_request)?;
    let requested_item_id = contains_id(input.original_request.url(), "Items")
        .ok_or(ApplicationError::InvalidInput("missing playback item"))?;
    let source_generation = state
        .media_storage
        .begin_media_reconciliation()
        .await
        .map_err(ApplicationError::Storage)?;

    if payload.device_profile.is_none() {
        warn!("Got playback request from client without device profile. Transcoding will be enforced!")
    }
    let mut payload = payload;
    let selected_source = selected_source(&input.original_request, &payload);

    // Jellyfin Web uses item.Id as the default source when carrying track
    // preferences into the next queued item. An aggregate has no source of
    // its own: let the already selected member negotiate its default sources.
    let default_aggregate_source = default_aggregate_query
        || selected_source.as_deref().is_some_and(|source| {
            crate::media_storage_service::MediaStorageService::normalize_uuid(source)
                == crate::media_storage_service::MediaStorageService::normalize_uuid(
                    &requested_item_id,
                )
        }) && state
            .media_storage
            .get_media_version_group(&requested_item_id)
            .await
            .map_err(ApplicationError::Storage)?
            .is_some();
    if default_aggregate_source {
        payload.media_source_id = None;
    }

    let plan = (MediaTargetResolver {
        media: state.media_storage,
    })
    .plan(
        &input.context,
        initial.target,
        PlaybackIntent {
            item: &requested_item_id,
            source: selected_source
                .as_deref()
                .filter(|_| !default_aggregate_source),
            binding: None,
        },
        state.server_storage,
        SelectionPolicy::Negotiation,
    )
    .await
    .map_err(|error| {
        ApplicationError::PlaybackResolution(error)
            .at(crate::application::error::FailureStage::NegotiationSelection)
    })?;
    let mut request = input.original_request;
    debug!(reason = ?plan.reason, "Playback selection planned");
    if let Some(member) = plan.target.member {
        *request.url_mut() = replace_path_id(request.url(), "Items", &member.virtual_media_id)
            .ok_or(ApplicationError::InvalidInput("missing playback item path"))?;
    }
    let target = plan.target.backend;
    if default_aggregate_source {
        remove_source_query(&mut request);
    }
    let server = target.server();
    let session = target.session().as_ref().ok_or_else(|| {
        ApplicationError::MissingIdentity("selected playback session")
            .at(crate::application::error::FailureStage::CallerAuthentication)
    })?;
    remap_playback_request(&mut payload, state, session, server).await?;
    debug!(server_id = %server.id, "Forwarding playback negotiation");
    set_json_body_typed(&mut request, &payload)?;

    let prefix = state.get_url_prefix().await;
    let request = UpstreamRequestBuilder::new(state.processors.url_processor, prefix.as_deref())
        .build(&target, &request)
        .await
        .map_err(|error| ApplicationError::InvalidRouting(error.into()))?;

    let original_item_id = contains_id(request.url(), "Items").ok_or(
        ApplicationError::InvalidInput("missing upstream playback item"),
    )?;
    match crate::application::upstream::execute_payload::<PlaybackResponse>(
        state.upstream.as_ref(),
        request,
    )
    .await
    {
        Ok(mut response) => {
            // Preserve the client's intent to use the member's own source.
            // Web otherwise may select a different version with other tracks.
            if default_aggregate_source && !response.media_sources.is_empty() {
                let index = response
                    .media_sources
                    .iter()
                    .position(|source| {
                        crate::media_storage_service::MediaStorageService::normalize_uuid(
                            &source.id,
                        ) == crate::media_storage_service::MediaStorageService::normalize_uuid(
                            &original_item_id,
                        )
                    })
                    .unwrap_or(0);
                let source = response.media_sources.remove(index);
                response.media_sources = vec![source];
            }
            let mut live_plans = process_playback_response(
                &mut response,
                state,
                server,
                session,
                proxy_api_key.as_deref(),
            )
            .await?;
            record_playback_sources(
                state.media_storage,
                &requested_item_id,
                server,
                &original_item_id,
                source_generation,
                &mut response.media_sources,
            )
            .await?;
            let final_payload =
                serde_json::to_value(&response).map_err(ApplicationError::Serialization)?;
            serde_json::to_vec(&final_payload).map_err(ApplicationError::Serialization)?;
            // Reconciliation can discard stale sources. Publish only IDs actually
            // advertised, and validate the whole batch before touching any binding.
            live_plans.retain(|plan| {
                final_payload["MediaSources"]
                    .as_array()
                    .is_some_and(|sources| {
                        sources.iter().any(|source| {
                            source.as_object().is_some_and(|source| {
                                source.iter().any(|(key, value)| {
                                    key.eq_ignore_ascii_case("LiveStreamId")
                                        && value.as_str() == Some(plan.virtual_id())
                                })
                            })
                        })
                    })
            });
            super::lifecycle::PlaybackLifecycle::new(state.play_sessions)
                .commit_live(&live_plans)
                .await
                .map_err(|error| {
                    ApplicationError::Internal(error.into())
                        .at(crate::application::error::FailureStage::PlaybackCommit)
                })?;
            // Register routable identities before publishing playback bindings.
            // A stale reconciliation may have removed unadvertisable sources.
            for source in &response.media_sources {
                track_play_session(
                    source,
                    &response.play_session_id,
                    &session.user_id,
                    server,
                    state,
                )
                .await;
            }
            track_playback_alias(
                &requested_item_id,
                &response.play_session_id,
                &session.user_id,
                server,
                state,
            )
            .await;
            super::lifecycle::PlaybackLifecycle::new(state.play_sessions)
                .bind_original(
                    &response.play_session_id,
                    &session.user_id,
                    server.id,
                    &original_item_id,
                )
                .await;
            debug!(server_id = %server.id, source_count = response.media_sources.len(), "Playback negotiation completed");
            if let Some(token) = proxy_api_key.as_deref() {
                crate::application::response::media_response(
                    state.client_sessions,
                    Some(token),
                    &final_payload,
                )
                .await;
            }
            Ok(response)
        }
        Err(e) => {
            error!("Failed to get playback info: {:?}", e);
            Err(e)
        }
    }
}

// Jellyfin binds the query parameter before the corresponding request DTO field.
fn selected_source(request: &reqwest::Request, payload: &PlaybackRequest) -> Option<String> {
    request
        .url()
        .query_pairs()
        .find(|(key, _)| key.eq_ignore_ascii_case("MediaSourceId"))
        .map(|(_, value)| value.into_owned())
        .or_else(|| payload.media_source_id.clone())
}

fn remove_source_query(request: &mut reqwest::Request) {
    let pairs = request
        .url()
        .query_pairs()
        .filter(|(key, _)| !key.eq_ignore_ascii_case("MediaSourceId"))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    request
        .url_mut()
        .query_pairs_mut()
        .clear()
        .extend_pairs(pairs);
}

#[cfg(test)]
mod tests;
#[cfg(test)]
use super::selection::{resolve_playback_route, PlaybackRouteDecision};
