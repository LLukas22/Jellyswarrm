//! Prepare authorized fallback requests and forward responses through the proxy workflow.
use crate::application::error::FailureStage;
#[cfg(test)]
use hyper::StatusCode;
use hyper::{
    header::{self, HeaderValue},
    Method, Response,
};
use serde_json::Value;
use tracing::{debug, warn};

use crate::{
    application::request::remap_authorization,
    application::{
        error::ApplicationError,
        playback::{lifecycle::PendingPlaybackSessionUpdate, reports},
        request::{context::AuthorizedTarget, input::OperationRequest, routing},
        services::MediaServices,
    },
    processors::request_analyzer::PlaybackSessionAction,
    processors::request_processor::RequestProcessingContext,
    processors::response_processor::ResponseProcessingProfile,
    protocol::url::{decoded_path_segments, replace_path_id},
    servers::compare_routes,
    upstream::headers::is_hop_by_hop_header,
};

/// Preserve the upstream-success observation point. Playback-binding commits
/// remain separate and revision-fenced after the required response validation.
async fn forwarded_success(
    state: &MediaServices,
    input: &OperationRequest,
    mutation_viewer: Option<&str>,
    server_id: crate::domain::ids::ServerId,
) {
    if let Some(viewer) = mutation_viewer {
        state.catalog_snapshots.invalidate_viewer(viewer);
    }
    let context = &input.context;
    let (Some(user), Some(token)) = (
        context.user(),
        context.authorization().and_then(|auth| auth.token_ref()),
    ) else {
        return;
    };
    let path = state.remove_prefix(input.original_request.url().path());
    let action = crate::processors::request_analyzer::playback_session_action(
        input.original_request.method(),
        path,
    );
    if let Some(action) = action {
        if let Some(report) = input
            .original_request
            .body()
            .and_then(|body| body.as_bytes())
            .and_then(|bytes| serde_json::from_slice(bytes).ok())
        {
            let id = state
                .client_sessions
                .ensure(user, token, context.device())
                .await;
            state
                .client_sessions
                .report(&id, action == PlaybackSessionAction::Remove, &report)
                .await;
            if action != PlaybackSessionAction::Remove {
                if let Some(item) = report.as_object().and_then(|object| {
                    object
                        .iter()
                        .find(|(key, _)| key.eq_ignore_ascii_case("ItemId"))
                        .and_then(|(_, value)| value.as_str())
                }) {
                    match crate::application::playback::continuity::EpisodeContinuity::prepare(
                        &state.media_storage,
                        &state.client_sessions,
                        context,
                        item,
                    )
                    .await
                    {
                        Ok(Some(continuity)) => {
                            continuity.remember(&state.client_sessions, server_id).await
                        }
                        Ok(None) => {}
                        Err(_) => warn!(
                            "Could not retain episode server preference after playback report"
                        ),
                    }
                }
            }
        }
    }
}

/// A selected, validated operation whose client IDs have not yet been encoded.
/// Retain the untouched input for local observation after successful delivery.
pub(crate) struct PreparedFallback {
    input: OperationRequest,
    normalized_request: reqwest::Request,
    target: AuthorizedTarget,
    pending_update: Option<PendingPlaybackSessionUpdate>,
}

pub(crate) async fn prepare(
    state: &MediaServices,
    input: OperationRequest,
    action: Option<PlaybackSessionAction>,
) -> anyhow::Result<PreparedFallback> {
    let mut request = input
        .original_request
        .try_clone()
        .ok_or_else(|| anyhow::anyhow!("failed to clone preprocessed request body"))?;
    let context = &input.context;
    let (server, session, _) = if action.is_some() {
        let target = reports::resolve_server(
            &context.backends().sessions,
            input
                .body_analysis
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing playback report"))?,
            &state.media_storage,
        )
        .await?;
        (target.server, Some(target.session), true)
    } else {
        routing::resolve_server(
            &context.backends().sessions,
            &input.body_analysis,
            state,
            &input.original_request,
            context.access_scope(),
        )
        .await?
    };
    let playback_member = if action.is_some() {
        let member = reports::resolve_member(
            input
                .body_analysis
                .as_ref()
                .expect("validated playback report"),
            &state.media_storage,
            &server,
        )
        .await?;
        let mut body: Value = request
            .body()
            .and_then(|body| body.as_bytes())
            .and_then(|bytes| serde_json::from_slice(bytes).ok())
            .ok_or_else(|| anyhow::anyhow!("missing playback body"))?;
        if let Some(object) = body.as_object_mut() {
            for (key, value) in object.iter_mut() {
                if key.eq_ignore_ascii_case("ItemId") {
                    *value = Value::String(member.virtual_media_id.clone());
                }
            }
        }
        *request.body_mut() = Some(serde_json::to_vec(&body)?.into());
        Some(member)
    } else {
        None
    };
    let pending_update = PendingPlaybackSessionUpdate::plan(
        action,
        input.body_analysis.as_ref(),
        context.user().map(|user| user.id.as_str()),
        server.id,
        playback_member
            .as_ref()
            .map(|member| member.original_media_id.as_str()),
    );
    state
        .processors
        .url_processor
        .validate_container_request(
            request.url(),
            session.as_ref(),
            context.access_scope(),
            server.id,
            input
                .body_analysis
                .as_ref()
                .map(|analysis| analysis.found_ids.as_slice())
                .unwrap_or_default(),
        )
        .await?;
    let auth = remap_authorization(context.authorization(), session.as_ref());
    let target = AuthorizedTarget::new(server, session, auth, context.access_scope().cloned())?;
    Ok(PreparedFallback {
        input,
        normalized_request: request,
        target,
        pending_update,
    })
}

/// Generic JSON/binary passthrough workflow. The HTTP adapter owns incoming
/// request parsing and converts this transport-neutral response to an Axum body.
/// This preserves the legacy buffered fallback; media delivery remains streamed
/// through the dedicated delivery adapter.
pub(crate) async fn forward(
    state: &MediaServices,
    prepared: PreparedFallback,
    response_profile: ResponseProcessingProfile,
) -> Result<Response<hyper::body::Bytes>, ApplicationError> {
    if let Some(response) = update_aggregate(state, &prepared.input).await? {
        let bytes = serde_json::to_vec(&response).map_err(ApplicationError::Serialization)?;
        return Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes.into())
            .map_err(|error| ApplicationError::Internal(error.into()));
    }
    let PreparedFallback {
        input,
        normalized_request,
        target,
        pending_update,
    } = prepared;
    let context = &input.context;
    let mut request = state
        .request_builder()
        .build(&target, &normalized_request)
        .await?;
    let request_url = request.url().clone();
    let mutation_viewer = (!matches!(*request.method(), Method::GET | Method::HEAD))
        .then(|| context.user().map(|user| user.id.clone()))
        .flatten();
    let server = target.server().clone();
    let proxy_api_key = context
        .authorization()
        .and_then(|auth| auth.token_ref())
        .map(str::to_string);
    let processing = RequestProcessingContext {
        user: context.user().cloned(),
        server: server.clone(),
        sessions: context.backends().sessions.clone(),
        auth: context.authorization().cloned(),
        session: target.session().cloned(),
        new_auth: target.authorization().cloned(),
    };
    state
        .processors
        .process_request_body(&mut request, &processing, &request_url)
        .await?;
    let response = state
        .reqwest_client
        .execute(request)
        .await
        .map_err(|error| ApplicationError::UpstreamTransport(error.without_url()))?;
    let status = response.status();
    if status.is_success() {
        forwarded_success(state, &input, mutation_viewer.as_deref(), server.id).await;
    } else {
        warn!("Upstream server returned error status: {}", status);
    }
    let mut headers = response.headers().clone();
    let mut body = response
        .bytes()
        .await
        .map_err(|error| ApplicationError::UpstreamTransport(error.without_url()))?;
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/json"));
    if status.is_success() && response_profile.requires_json() && (!is_json || body.is_empty()) {
        return Err(ApplicationError::InvalidUpstreamPayload(
            "required JSON response is empty or has wrong content type",
        ));
    }
    if is_json && !body.is_empty() {
        match serde_json::from_slice::<Value>(&body) {
            Ok(mut value) => {
                let modified = state
                    .process_response_json(
                        &mut value,
                        &server,
                        if status.is_success() {
                            response_profile
                        } else {
                            ResponseProcessingProfile::BestEffortMedia
                        },
                        proxy_api_key.as_deref(),
                    )
                    .await?;
                if status.is_success() && response_profile != ResponseProcessingProfile::Disabled {
                    if let Some(token) = proxy_api_key.as_deref() {
                        state
                            .client_sessions
                            .cache_media_response(token, &value)
                            .await;
                    }
                }
                if modified {
                    let processed =
                        serde_json::to_vec(&value).map_err(ApplicationError::Serialization)?;
                    headers.remove(header::CONTENT_LENGTH);
                    headers.remove(header::TRANSFER_ENCODING);
                    headers.insert(
                        header::CONTENT_LENGTH,
                        HeaderValue::from_str(&processed.len().to_string())
                            .map_err(|error| ApplicationError::Internal(error.into()))?,
                    );
                    body = processed.into();
                }
            }
            Err(error) => {
                if status.is_success() && response_profile.requires_json() {
                    return Err(ApplicationError::InvalidUpstreamJson(error));
                }
                debug!(
                    "Skipping response JSON processing because parsing failed: {}",
                    error
                );
            }
        }
    }
    let mut builder = Response::builder().status(status);
    for (name, value) in &headers {
        if !is_hop_by_hop_header(name) {
            builder = builder.header(name, value);
        }
    }
    let response = builder
        .body(body)
        .map_err(|error| ApplicationError::Internal(error.into()))?;
    if status.is_success() {
        if let Some(update) = pending_update {
            update.commit(&state.playback_registry).await;
        }
    }
    Ok(response)
}

/// A watched-state action on a merged item applies to every authorized copy.
/// Preflight availability before writing; never acknowledge a partial update
/// as success. Explicit member IDs continue through the ordinary proxy.
pub(crate) async fn update_aggregate(
    state: &MediaServices,
    request: &OperationRequest,
) -> Result<Option<Value>, ApplicationError> {
    if !matches!(
        *request.original_request.method(),
        reqwest::Method::POST | reqwest::Method::DELETE
    ) {
        return Ok(None);
    }
    let segments =
        decoded_path_segments(state.remove_prefix(request.original_request.url().path()));
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
        .map_err(ApplicationError::Storage)?
    else {
        return Ok(None);
    };
    let scope = request.context.access_scope().ok_or_else(|| {
        ApplicationError::MissingIdentity("aggregate viewer").at(FailureStage::CallerAuthentication)
    })?;
    let mut members = state
        .media_storage
        .get_media_version_members(group.id)
        .await
        .map_err(ApplicationError::Storage)?;
    members.retain(|member| scope.allows(member.server.id));
    if members.is_empty() {
        return Err(
            ApplicationError::InvalidInput("no authorized aggregate members")
                .at(FailureStage::ResourceLookup),
        );
    }
    members.sort_by(|left, right| {
        compare_routes(
            &right.server,
            &right.mapping.original_media_id,
            &left.server,
            &left.mapping.original_media_id,
        )
    });
    let mut targets = Vec::new();
    for member in members {
        let target = request
            .context
            .target(member.server.id)
            .map_err(ApplicationError::TargetSelection)?;
        let mut upstream =
            request
                .original_request
                .try_clone()
                .ok_or(ApplicationError::MissingIdentity(
                    "buffered aggregate request",
                ))?;
        *upstream.url_mut() =
            replace_path_id(upstream.url(), tag, &member.mapping.virtual_media_id)
                .ok_or(ApplicationError::InvalidInput("aggregate path identity"))?;
        let upstream = state.request_builder().build(&target, &upstream).await?;
        targets.push((upstream, member.server));
    }
    let mut result = None;
    for (upstream, server) in targets {
        let mut data: Value = crate::application::request::response::execute_json_request(
            &state.reqwest_client,
            upstream,
        )
        .await?;
        // Also invalidate after a partial write if a later member rejects it.
        state.catalog_snapshots.invalidate_viewer(scope.user_id());
        if result.is_none() {
            state
                .process_response_json(
                    &mut data,
                    &server,
                    ResponseProcessingProfile::Media,
                    request
                        .context
                        .authorization()
                        .and_then(|auth| auth.token_ref()),
                )
                .await?;
            data.as_object_mut()
                .ok_or(ApplicationError::InvalidUpstreamPayload(
                    "aggregate response must be an object",
                ))?
                .insert(
                    "ItemId".into(),
                    Value::String(group.virtual_media_id.clone()),
                );
            result = Some(data);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::{
        application::request::context::RequestContext,
        auth::service::{AuthorizationSession, LocalCredential, LocalUser},
        config::{AppConfig, MediaStreamingMode},
        libraries::service::VirtualLibraryAccessScope,
        media::storage::MediaStorageService,
        persistence::MIGRATOR,
        processors::request_analyzer::RequestBodyAnalysisResult,
        protocol::device::Device,
        servers::storage::ServerStorageService,
        test_support::MediaFixture,
    };

    async fn report_fixture() -> (MediaFixture, OperationRequest, wiremock::MockServer) {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&pool).await.unwrap();
        let upstream = wiremock::MockServer::start().await;
        let servers = ServerStorageService::new(pool.clone());
        let id = servers
            .add_server(
                "Report owner",
                &upstream.uri(),
                100,
                MediaStreamingMode::Proxy,
            )
            .await
            .unwrap();
        let server = servers.get_server_by_id(id).await.unwrap().unwrap();
        let media = MediaStorageService::new(pool.clone());
        let item = media
            .get_or_create_media_mapping("upstream-item", &server)
            .await
            .unwrap();
        let state = MediaFixture::new(pool, AppConfig::default());
        let mut original = reqwest::Request::new(
            reqwest::Method::POST,
            "http://proxy.example/Sessions/Playing".parse().unwrap(),
        );
        original.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        *original.body_mut() = Some(
            serde_json::to_vec(&serde_json::json!({
                "ItemId": item.virtual_media_id, "PlaySessionId": "report-session",
                "Extension": {"opaque":true},
            }))
            .unwrap()
            .into(),
        );
        let user = LocalUser {
            id: "viewer".into(),
            virtual_key: "proxy-token".into(),
            original_username: "Viewer".into(),
            local_credential: LocalCredential::Passwordless,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let session = AuthorizationSession {
            id: 1,
            user_id: user.id.clone(),
            mapping_id: 1,
            server_url: server.url.to_string(),
            device: Device {
                client: "Test".into(),
                device: "Player".into(),
                device_id: "player".into(),
                version: "1".into(),
            },
            jellyfin_token: "backend-token".into(),
            original_user_id: "backend-viewer".into(),
            expires_at: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        let context = RequestContext::new(
            &original,
            None,
            Some(user),
            Some(vec![(session, server)]),
            Some(VirtualLibraryAccessScope::new("viewer", [id])),
        );
        let input = OperationRequest {
            original_request: original,
            context,
            body_analysis: Some(RequestBodyAnalysisResult {
                found_ids: vec![item.virtual_media_id.clone()],
                requested_play_item_id: Some(item.virtual_media_id),
                requested_play_session_id: Some("report-session".into()),
                ..Default::default()
            }),
        };
        (state, input, upstream)
    }

    #[tokio::test]
    async fn report_prepare_does_not_send_or_commit_and_failure_does_not_commit() {
        let (state, input, upstream) = report_fixture().await;
        let prepared = prepare(&state, input, Some(PlaybackSessionAction::Start))
            .await
            .unwrap();
        assert_eq!(
            prepared.normalized_request.url().host_str(),
            Some("proxy.example")
        );
        assert!(upstream.received_requests().await.unwrap().is_empty());
        assert!(state
            .playback_registry
            .resolve_report_session("report-session", "viewer")
            .await
            .unwrap()
            .is_none());
        wiremock::Mock::given(wiremock::matchers::path("/Sessions/Playing"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&upstream)
            .await;
        let response = forward(&state, prepared, ResponseProcessingProfile::Disabled)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(state
            .playback_registry
            .resolve_report_session("report-session", "viewer")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn report_success_with_invalid_required_response_does_not_commit() {
        let (state, input, upstream) = report_fixture().await;
        wiremock::Mock::given(wiremock::matchers::path("/Sessions/Playing"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_raw("invalid json", "application/json"),
            )
            .mount(&upstream)
            .await;
        let prepared = prepare(&state, input, Some(PlaybackSessionAction::Start))
            .await
            .unwrap();
        let error = forward(
            &state,
            prepared,
            ResponseProcessingProfile::PlaylistPermissions,
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApplicationError::InvalidUpstreamJson(_)));
        assert!(state
            .playback_registry
            .resolve_report_session("report-session", "viewer")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn successful_report_commits_after_forward_and_translates_body_once() {
        let (state, input, upstream) = report_fixture().await;
        let item = input
            .body_analysis
            .as_ref()
            .unwrap()
            .requested_play_item_id
            .clone()
            .unwrap();
        wiremock::Mock::given(wiremock::matchers::path("/Sessions/Playing"))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "ItemId":"upstream-item", "Extension":{"opaque":true},
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&upstream)
            .await;
        let prepared = prepare(&state, input, Some(PlaybackSessionAction::Start))
            .await
            .unwrap();
        let response = forward(&state, prepared, ResponseProcessingProfile::Disabled)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let (binding, _) = state
            .playback_registry
            .resolve_report_session("report-session", "viewer")
            .await
            .unwrap()
            .unwrap()
            .into_parts();
        assert_eq!(binding.item_id, item);
        assert_eq!(binding.original_item_id.as_deref(), Some("upstream-item"));
    }
}
