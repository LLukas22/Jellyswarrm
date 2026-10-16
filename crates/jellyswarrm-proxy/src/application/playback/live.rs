use super::target::{MediaTargetResolver, ResolutionError};
use hyper::StatusCode;
use serde_json::Value;

mod binding;
pub(crate) use binding::prepare_live_source;
use binding::rewrite_live_id;

use crate::{
    application::{
        context::{RequestContext, TargetError},
        error::ApplicationError,
        json::execute_processed_json_request_typed,
        playback::{response::track_play_session, selection::record_playback_sources},
    },
    models::MediaSource,
    processors::{response_processor::ResponseProcessingProfile, url_processor::play_session_id},
    upstream::jellyfin::{json::set_json_body_typed, request::UpstreamRequestBuilder},
    AppState,
};

/// Live commands have caller context but no arbitrary initial backend. The
/// negotiated playback/live binding selects the sole target before translation.
pub(crate) struct LiveRequest {
    pub original_request: reqwest::Request,
    pub context: RequestContext,
}

/// Close preserves the upstream status without a body; refresh returns the
/// translated JSON payload with its existing proxy-owned live identity.
pub(crate) enum LiveStreamActionResult {
    Status(StatusCode),
    Json(Value),
}

fn body(request: &reqwest::Request) -> Result<Value, ApplicationError> {
    match request
        .body()
        .and_then(|body| body.as_bytes())
        .filter(|bytes| !bytes.is_empty())
    {
        Some(bytes) => serde_json::from_slice(bytes).map_err(ApplicationError::InvalidRequestJson),
        None => Ok(serde_json::json!({})),
    }
}

// Jellyfin gives query parameters precedence over the corresponding DTO field.
fn field(
    request: &reqwest::Request,
    body: &Value,
    name: &str,
) -> Result<Option<String>, ApplicationError> {
    let values = request
        .url()
        .query_pairs()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.into_owned())
        .collect::<Vec<_>>();
    if let Some(first) = values.first() {
        if values.iter().any(|value| value != first) {
            return Err(ApplicationError::InvalidInput(
                "conflicting live query fields",
            ));
        }
        return Ok(Some(first.clone()));
    }
    let object = body.as_object().ok_or(ApplicationError::InvalidInput(
        "live body must be an object",
    ))?;
    let mut result = None;
    for (_, value) in object
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
    {
        if value.is_null() {
            continue;
        }
        let value = value.as_str().ok_or(ApplicationError::InvalidInput(
            "live identity must be a string",
        ))?;
        if result.as_deref().is_some_and(|previous| previous != value) {
            return Err(ApplicationError::InvalidInput(
                "conflicting live body fields",
            ));
        }
        result = Some(value.to_string());
    }
    Ok(result)
}

/// Execute a caller-owned live refresh or close against its bound owner.
#[cfg(test)]
pub(crate) async fn post_live_stream_action(
    state: AppState,
    preprocessed: LiveRequest,
) -> Result<LiveStreamActionResult, StatusCode> {
    post_live_stream_action_typed(state, preprocessed)
        .await
        .map_err(|error| error.detail_status())
}

pub(crate) async fn post_live_stream_action_typed(
    state: AppState,
    preprocessed: LiveRequest,
) -> Result<LiveStreamActionResult, ApplicationError> {
    let scope = preprocessed.context.access_scope.as_ref().ok_or_else(|| {
        ApplicationError::MissingIdentity("live viewer scope").with_status(StatusCode::UNAUTHORIZED)
    })?;
    let mut payload = body(&preprocessed.original_request)?;
    let id = field(&preprocessed.original_request, &payload, "LiveStreamId")?
        .ok_or(ApplicationError::InvalidInput("missing live identity"))?;
    let (target, binding) = super::target::live_target(
        &preprocessed.context,
        &state.play_sessions,
        &id,
        scope.user_id(),
    )
    .await
    .map_err(|error| {
        let status = match &error {
            ResolutionError::Target(error) => target_status(error),
            _ => StatusCode::BAD_REQUEST,
        };
        ApplicationError::PlaybackResolution(error).with_status(status)
    })?;
    let server = target.server();
    let mut request = preprocessed
        .original_request
        .try_clone()
        .ok_or(ApplicationError::MissingIdentity("cloneable live request"))?;
    let close = request
        .url()
        .path()
        .trim_end_matches('/')
        .to_ascii_lowercase()
        .ends_with("/close");
    let pairs = request
        .url()
        .query_pairs()
        .filter(|(key, _)| !key.eq_ignore_ascii_case("LiveStreamId"))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    request
        .url_mut()
        .query_pairs_mut()
        .clear()
        .extend_pairs(pairs)
        .append_pair("LiveStreamId", &id);
    let object = payload
        .as_object_mut()
        .ok_or(ApplicationError::InvalidInput(
            "live body must be an object",
        ))?;
    object.retain(|key, _| !key.eq_ignore_ascii_case("LiveStreamId"));
    object.insert("LiveStreamId".into(), Value::String(binding.original_id));
    set_json_body_typed(&mut request, &payload)?;
    let prefix = state.get_url_prefix().await;
    let request = UpstreamRequestBuilder::new(&state.processors.url_processor, prefix.as_deref())
        .build(&target, &request)
        .await
        .map_err(|error| ApplicationError::Internal(error.into()))?;
    if close {
        let response = state
            .reqwest_client
            .execute(request)
            .await
            .map_err(|error| ApplicationError::UpstreamTransport(error.without_url()))?;
        let status = response.status();
        if status.is_success() {
            super::lifecycle::PlaybackLifecycle {
                sessions: &state.play_sessions,
            }
            .close_live(&id, scope.user_id())
            .await;
        }
        return Ok(LiveStreamActionResult::Status(status));
    }
    let mut response = execute_processed_json_request_typed(
        &state,
        request,
        server,
        ResponseProcessingProfile::Media,
        preprocessed
            .context
            .auth
            .as_ref()
            .and_then(|auth| auth.token_ref()),
    )
    .await?;
    rewrite_live_id(&mut response, &id);
    Ok(LiveStreamActionResult::Json(response))
}

/// OpenToken is backend-local. Use the caller-owned negotiation binding, and
/// retain its exact member even when an aggregate has several copies per server.
#[cfg(test)]
pub(crate) async fn post_livestream_open(
    state: AppState,
    preprocessed: LiveRequest,
) -> Result<Value, StatusCode> {
    post_livestream_open_typed(state, preprocessed)
        .await
        .map_err(|error| error.detail_status())
}

pub(crate) async fn post_livestream_open_typed(
    state: AppState,
    preprocessed: LiveRequest,
) -> Result<Value, ApplicationError> {
    let scope = preprocessed.context.access_scope.as_ref().ok_or_else(|| {
        ApplicationError::MissingIdentity("live viewer scope").with_status(StatusCode::UNAUTHORIZED)
    })?;
    let mut payload = body(&preprocessed.original_request)?;
    let session_id = play_session_id(preprocessed.original_request.url())
        .map_err(ApplicationError::InvalidRouting)?
        .or(field(
            &preprocessed.original_request,
            &payload,
            "PlaySessionId",
        )?)
        .ok_or(ApplicationError::InvalidInput(
            "missing live playback session",
        ))?;
    let item_id = field(&preprocessed.original_request, &payload, "ItemId")?
        .ok_or(ApplicationError::InvalidInput("missing live item"))?;
    let item_id = crate::media_storage_service::MediaStorageService::normalize_uuid(&item_id);
    let item_id = &item_id;
    let binding = (MediaTargetResolver {
        media: &state.media_storage,
    })
    .session(
        &state.play_sessions,
        item_id,
        Some(&session_id),
        scope.user_id(),
        false,
    )
    .await
    .map_err(|error| {
        let status = match &error {
            ResolutionError::ViewerMismatch(_) => StatusCode::FORBIDDEN,
            _ => StatusCode::BAD_REQUEST,
        };
        ApplicationError::PlaybackResolution(error).with_status(status)
    })?
    .ok_or(ApplicationError::InvalidInput(
        "unknown live playback session",
    ))?;
    if !scope.allows(binding.server_id) {
        return Err(ApplicationError::AccessDenied);
    }
    let target = preprocessed
        .context
        .target(binding.server_id)
        .map_err(|error| {
            let status = target_status(&error);
            ApplicationError::Internal(error.into()).with_status(status)
        })?;
    let server = target.server();
    let session = target
        .session()
        .as_ref()
        .ok_or(ApplicationError::UnavailableOwner)?;
    let original_item_id =
        binding
            .original_item_id
            .as_deref()
            .ok_or(ApplicationError::InvalidInput(
                "missing bound upstream item",
            ))?;
    // Query translation consumes client identities. The body below is already
    // backend JSON and must not be run through the client-body translator again.
    let member = state
        .media_storage
        .get_media_mapping_by_original(original_item_id, server.id)
        .await
        .map_err(ApplicationError::Storage)?
        .ok_or(ApplicationError::InvalidInput("unknown bound member"))?;
    let generation = state
        .media_storage
        .begin_media_reconciliation()
        .await
        .map_err(ApplicationError::Storage)?;
    let mut request = preprocessed
        .original_request
        .try_clone()
        .ok_or(ApplicationError::MissingIdentity("cloneable live request"))?;
    let pairs = request
        .url()
        .query_pairs()
        .filter(|(key, _)| {
            !["ItemId", "PlaySessionId", "SessionId"]
                .iter()
                .any(|field| key.eq_ignore_ascii_case(field))
        })
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    request
        .url_mut()
        .query_pairs_mut()
        .clear()
        .extend_pairs(pairs)
        .append_pair("ItemId", &member.virtual_media_id)
        .append_pair("PlaySessionId", &session_id);
    let object = payload
        .as_object_mut()
        .ok_or(ApplicationError::InvalidInput(
            "live body must be an object",
        ))?;
    object.retain(|key, _| {
        !["ItemId", "UserId", "PlaySessionId"]
            .iter()
            .any(|field| key.eq_ignore_ascii_case(field))
    });
    object.insert("ItemId".into(), Value::String(original_item_id.into()));
    object.insert(
        "UserId".into(),
        Value::String(session.original_user_id.clone()),
    );
    object.insert("PlaySessionId".into(), Value::String(session_id.clone()));
    set_json_body_typed(&mut request, &payload)?;
    let prefix = state.get_url_prefix().await;
    let request = UpstreamRequestBuilder::new(&state.processors.url_processor, prefix.as_deref())
        .build(&target, &request)
        .await
        .map_err(|error| ApplicationError::Internal(error.into()))?;
    let token = preprocessed
        .context
        .auth
        .as_ref()
        .and_then(|auth| auth.token_ref());
    let mut response = execute_processed_json_request_typed(
        &state,
        request,
        server,
        ResponseProcessingProfile::Media,
        token,
    )
    .await?;
    // Jellyfin returns a singular MediaSource here, not a PlaybackResponse.
    let mut source: MediaSource = serde_json::from_value(response["MediaSource"].clone())
        .map_err(ApplicationError::InvalidUpstreamJson)?;
    let live_plan = prepare_live_source(&state, &mut source, scope.user_id(), server).await?;
    let mut sources = vec![source];
    record_playback_sources(
        &state,
        item_id,
        server,
        original_item_id,
        generation,
        &mut sources,
    )
    .await?;
    let source = sources.first().ok_or(ApplicationError::UnavailableOwner)?;
    response["MediaSource"] =
        serde_json::to_value(source).map_err(ApplicationError::Serialization)?;
    // Finish all fallible work before publishing either live or ordinary bindings.
    serde_json::to_vec(&response).map_err(ApplicationError::Serialization)?;
    if !(super::lifecycle::PlaybackLifecycle {
        sessions: &state.play_sessions,
    })
    .commit_live(&live_plan.into_iter().collect::<Vec<_>>())
    .await
    {
        return Err(ApplicationError::UnavailableOwner);
    }
    track_play_session(source, &session_id, scope.user_id(), server, &state).await;
    super::lifecycle::PlaybackLifecycle {
        sessions: &state.play_sessions,
    }
    .bind_original(&session_id, scope.user_id(), server.id, original_item_id)
    .await;
    Ok(response)
}

fn target_status(error: &TargetError) -> StatusCode {
    match error {
        TargetError::AccessDenied | TargetError::ViewerMismatch => StatusCode::FORBIDDEN,
        TargetError::UnavailableOwner => StatusCode::SERVICE_UNAVAILABLE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(query: &str) -> reqwest::Request {
        reqwest::Request::new(
            reqwest::Method::POST,
            format!("http://localhost/LiveStreams/Open{query}")
                .parse()
                .unwrap(),
        )
    }

    #[test]
    fn query_field_takes_precedence_over_conflicting_body_fields() {
        let request = request("?LiveStreamId=query&livestreamid=query");
        assert_eq!(
            field(
                &request,
                &json!({"LiveStreamId": "body", "livestreamid": "other"}),
                "LiveStreamId",
            )
            .unwrap(),
            Some("query".into()),
        );
    }

    #[test]
    fn conflicting_query_values_are_rejected_before_body_fallback() {
        assert_eq!(
            field(
                &request("?LiveStreamId=one&livestreamid=two"),
                &json!({"LiveStreamId": "body"}),
                "LiveStreamId",
            )
            .unwrap_err()
            .detail_status(),
            StatusCode::BAD_REQUEST,
        );
    }

    #[test]
    fn body_fields_are_case_insensitive_and_ignore_null_but_reject_conflicts() {
        let request = request("");
        assert_eq!(
            field(
                &request,
                &json!({"LiveStreamId": null, "livestreamid": "body"}),
                "LiveStreamId",
            )
            .unwrap(),
            Some("body".into()),
        );
        assert_eq!(
            field(
                &request,
                &json!({"LiveStreamId": "one", "livestreamid": "two"}),
                "LiveStreamId",
            )
            .unwrap_err()
            .detail_status(),
            StatusCode::BAD_REQUEST,
        );
    }
}
