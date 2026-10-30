//! Live open/refresh/close workflows and their identity rewriting helpers.
//! The existing binding selects the owner; only a successful workflow commits changes.
//! Live commands have caller context but no arbitrary initial backend: the negotiated
//! playback/live binding selects the sole target before translation.
use super::target::MediaTargetResolver;
use crate::application::error::FailureStage;
use crate::application::services::MediaServices;
use crate::media::storage::MediaStorageService;
use hyper::StatusCode;
use serde_json::Value;

use std::sync::Arc;

use crate::{
    application::{
        error::ApplicationError,
        input::OperationRequest,
        playback::{lifecycle::record_playback_sources, response::track_play_session},
    },
    models::MediaSource,
    processors::set_json_body,
    processors::{response_processor::ResponseProcessingProfile, url_processor::play_session_id},
};

/// Virtualize opaque live IDs and their delivery URL references together.
pub(crate) async fn prepare_live_source(
    state: &crate::playback::SessionStorage,
    source: &mut MediaSource,
    user: &str,
    server: &crate::servers::storage::Server,
) -> Result<Option<Arc<crate::playback::PreparedLiveStream>>, ApplicationError> {
    let mut value = serde_json::to_value(&*source).map_err(ApplicationError::Serialization)?;
    let Some(original) = value
        .as_object()
        .and_then(|object| {
            object
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case("LiveStreamId"))
        })
        .and_then(|(_, value)| value.as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
    else {
        return Ok(None);
    };
    let plan = state
        .prepare_live_binding(&original.as_str().into(), &user.into(), server.id)
        .await;
    rewrite_live_id(&mut value, plan.virtual_id());
    *source = serde_json::from_value(value).map_err(ApplicationError::InvalidUpstreamJson)?;
    Ok(Some(plan))
}

fn rewrite_live_id(value: &mut Value, id: &str) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if key.eq_ignore_ascii_case("LiveStreamId") {
                    *value = Value::String(id.into());
                } else if ["TranscodingUrl", "StreamUrl", "DeliveryUrl"]
                    .iter()
                    .any(|field| key.eq_ignore_ascii_case(field))
                {
                    if let Some(url) = value.as_str() {
                        if let Some((path, query)) = url.split_once('?') {
                            let mut pairs = url::form_urlencoded::Serializer::new(String::new());
                            for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
                                pairs.append_pair(
                                    &key,
                                    if key.eq_ignore_ascii_case("LiveStreamId") {
                                        id
                                    } else {
                                        &value
                                    },
                                );
                            }
                            *value = Value::String(format!("{path}?{}", pairs.finish()));
                        }
                    }
                } else {
                    rewrite_live_id(value, id);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                rewrite_live_id(value, id);
            }
        }
        _ => {}
    }
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
pub(crate) async fn post_live_stream_action(
    state: &MediaServices,
    preprocessed: OperationRequest,
) -> Result<LiveStreamActionResult, ApplicationError> {
    let scope = preprocessed.context.access_scope().ok_or_else(|| {
        ApplicationError::MissingIdentity("live viewer scope")
            .at(FailureStage::CallerAuthentication)
    })?;
    let mut payload = body(&preprocessed.original_request)?;
    let id = field(&preprocessed.original_request, &payload, "LiveStreamId")?
        .ok_or(ApplicationError::InvalidInput("missing live identity"))?;
    let (target, binding) = super::target::live_target(
        &preprocessed.context,
        &state.play_sessions,
        &id.as_str().into(),
        &scope.user_id().into(),
    )
    .await
    .map_err(|error| {
        ApplicationError::PlaybackResolution(error).at(FailureStage::LiveBindingLookup)
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
    object.insert(
        "LiveStreamId".into(),
        Value::String(binding.original_id.as_str().to_owned()),
    );
    set_json_body(&mut request, &payload)?;
    let request = state.request_builder().build(&target, &request).await?;
    if close {
        let status = crate::upstream::json::execute_status(&state.reqwest_client, request).await?;
        if status.is_success() {
            super::lifecycle::PlaybackLifecycle::new(&state.play_sessions)
                .close_live(&id.as_str().into(), &scope.user_id().into())
                .await;
        }
        return Ok(LiveStreamActionResult::Status(status));
    }
    let mut response = state
        .execute_processed_json(
            request,
            server,
            ResponseProcessingProfile::Media,
            preprocessed
                .context
                .authorization()
                .and_then(|auth| auth.token_ref()),
        )
        .await?;
    rewrite_live_id(&mut response, &id);
    Ok(LiveStreamActionResult::Json(response))
}

/// OpenToken is backend-local. Use the caller-owned negotiation binding, and
/// retain its exact member even when an aggregate has several copies per server.
pub(crate) async fn post_livestream_open(
    state: &MediaServices,
    preprocessed: OperationRequest,
) -> Result<Value, ApplicationError> {
    let scope = preprocessed.context.access_scope().ok_or_else(|| {
        ApplicationError::MissingIdentity("live viewer scope")
            .at(FailureStage::CallerAuthentication)
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
    let item_id = MediaStorageService::normalize_uuid(&item_id);
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
        ApplicationError::PlaybackResolution(error).at(FailureStage::LiveSessionLookup)
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
        .map_err(ApplicationError::TargetSelection)?;
    let server = target.server();
    let session = target.session().ok_or(ApplicationError::UnavailableOwner)?;
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
    set_json_body(&mut request, &payload)?;
    let request = state.request_builder().build(&target, &request).await?;
    let token = preprocessed
        .context
        .authorization()
        .and_then(|auth| auth.token_ref());
    let mut response = state
        .execute_processed_json(request, server, ResponseProcessingProfile::Media, token)
        .await?;
    // Jellyfin returns a singular MediaSource here, not a PlaybackResponse.
    let mut source: MediaSource = serde_json::from_value(response["MediaSource"].clone())
        .map_err(ApplicationError::InvalidUpstreamJson)?;
    let live_plan =
        prepare_live_source(&state.play_sessions, &mut source, scope.user_id(), server).await?;
    let mut sources = vec![source];
    record_playback_sources(
        &state.media_storage,
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
    super::lifecycle::PlaybackLifecycle::new(&state.play_sessions)
        .commit_live(&live_plan.into_iter().collect::<Vec<_>>())
        .await
        .map_err(|error| {
            ApplicationError::Internal(error.into()).at(FailureStage::PlaybackCommit)
        })?;
    track_play_session(source, &session_id, scope.user_id(), server, state).await;
    super::lifecycle::PlaybackLifecycle::new(&state.play_sessions)
        .bind_original(&session_id, scope.user_id(), server.id, original_item_id)
        .await;
    Ok(response)
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
        assert!(matches!(
            field(
                &request("?LiveStreamId=one&livestreamid=two"),
                &json!({"LiveStreamId": "body"}),
                "LiveStreamId",
            )
            .unwrap_err(),
            ApplicationError::InvalidInput(_)
        ));
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
        assert!(matches!(
            field(
                &request,
                &json!({"LiveStreamId": "one", "livestreamid": "two"}),
                "LiveStreamId",
            )
            .unwrap_err(),
            ApplicationError::InvalidInput(_)
        ));
    }
}
