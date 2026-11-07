//! Forward a client request that has no dedicated workflow to one backend.
//!
//! This is the proxy's default path. [`prepare`] decides *where* the request
//! goes; [`forward`] sends it and translates the answer:
//!
//! 1. select the backend: a playback report goes to the owner of its item,
//!    everything else through `request::routing`;
//! 2. authorize the target: playlist/collection IDs must live on it, and the
//!    proxy credentials are swapped for the backend session's;
//! 3. build the upstream request: backend URL, credentials, IDs in path, query
//!    and JSON body;
//! 4. send it and translate a JSON response back to virtual IDs;
//! 5. after success, update proxy state (catalog caches, client sessions,
//!    episode preference, playback bindings).
use hyper::{
    body::Bytes,
    header::{self, HeaderMap, HeaderValue},
    Method, Response, StatusCode,
};
use serde_json::Value;
use tracing::{debug, warn};

use crate::{
    application::{
        error::{ApplicationError, FailureStage},
        media_services::MediaServices,
        playback::{
            continuity::EpisodeContinuity, lifecycle::PendingPlaybackSessionUpdate, reports,
        },
        request::{
            context::AuthorizedTarget,
            input::{routing_error, OperationRequest},
        },
    },
    protocol::routes::PlaybackSessionAction,
    protocol::url::{decoded_path_segments, replace_path_id},
    servers::compare_routes,
    translation::{
        request_processor::RequestProcessingContext, response_processor::ResponseProcessingProfile,
    },
    upstream::headers::is_hop_by_hop_header,
};

/// A request with its backend chosen and authorized, not yet sent.
pub(crate) struct PreparedForward {
    input: OperationRequest,
    /// Request path without the proxy URL prefix.
    path: String,
    /// The client request, still addressed to the proxy with virtual IDs. Only a
    /// playback report's `ItemId` differs from `input.original_request`.
    normalized_request: reqwest::Request,
    target: AuthorizedTarget,
    report: Option<PlaybackReport>,
}

/// `POST /Sessions/Playing[/Progress|/Stopped]`: parsed once in `prepare`.
struct PlaybackReport {
    action: PlaybackSessionAction,
    /// The body as the client sent it.
    body: Value,
    /// Binding change to commit once the backend accepted the report.
    pending_update: Option<PendingPlaybackSessionUpdate>,
}

/// Choose and authorize the backend. Sends nothing and changes no state.
pub(crate) async fn prepare(
    state: &MediaServices,
    input: OperationRequest,
    action: Option<PlaybackSessionAction>,
) -> Result<PreparedForward, ApplicationError> {
    let path = state
        .remove_prefix(input.original_request.url().path())
        .to_owned();
    let mut normalized_request = input.original_request.try_clone().ok_or_else(|| {
        ApplicationError::Internal(anyhow::anyhow!("client request body is not buffered"))
    })?;
    let (target, report) = match action {
        None => (input.resolve_target(state).await?.target, None),
        Some(action) => {
            let (target, report) =
                prepare_report(state, &input, action, &mut normalized_request).await?;
            (target, Some(report))
        }
    };
    Ok(PreparedForward {
        input,
        path,
        normalized_request,
        target,
        report,
    })
}

/// Reports never use the generic best-server fallback: a wrong destination
/// would update another copy's playback state.
async fn prepare_report(
    state: &MediaServices,
    input: &OperationRequest,
    action: PlaybackSessionAction,
    request: &mut reqwest::Request,
) -> Result<(AuthorizedTarget, PlaybackReport), ApplicationError> {
    let invalid = |reason: &str| ApplicationError::InvalidRouting(anyhow::anyhow!("{reason}"));
    let analysis = input
        .body_analysis
        .as_ref()
        .ok_or_else(|| invalid("missing playback report"))?;
    let owner = reports::resolve_server(
        &input.context.backends().sessions,
        analysis,
        &state.media_storage,
    )
    .await
    .map_err(routing_error)?;
    let member = reports::resolve_member(analysis, &state.media_storage, &owner.server)
        .await
        .map_err(routing_error)?;

    // Point the report at the owner's member item. Body translation then maps
    // that member ID to the backend's own item ID.
    let body: Value = request
        .body()
        .and_then(|body| body.as_bytes())
        .and_then(|bytes| serde_json::from_slice(bytes).ok())
        .ok_or_else(|| invalid("missing playback body"))?;
    let mut rewritten = body.clone();
    if let Some(object) = rewritten.as_object_mut() {
        for (key, value) in object.iter_mut() {
            if key.eq_ignore_ascii_case("ItemId") {
                *value = Value::String(member.virtual_media_id.as_str().to_owned());
            }
        }
    }
    *request.body_mut() = Some(
        serde_json::to_vec(&rewritten)
            .map_err(ApplicationError::Serialization)?
            .into(),
    );

    let pending_update = PendingPlaybackSessionUpdate::plan(
        Some(action),
        Some(analysis),
        input.context.user().map(|user| user.id.as_str()),
        owner.server.id,
        Some(&member.original_media_id),
    );
    let target = input
        .authorize_target(state, owner.server, Some(owner.session))
        .await?;
    Ok((
        target,
        PlaybackReport {
            action,
            body,
            pending_update,
        },
    ))
}

/// Send the prepared request and return the translated backend response.
/// Bodies are buffered; streamed media delivery uses `upstream::delivery`.
pub(crate) async fn forward(
    state: &MediaServices,
    prepared: PreparedForward,
    response_profile: ResponseProcessingProfile,
) -> Result<Response<Bytes>, ApplicationError> {
    if let Some(response) = update_aggregate(state, &prepared).await? {
        let bytes = serde_json::to_vec(&response).map_err(ApplicationError::Serialization)?;
        return Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(bytes.into())
            .map_err(|error| ApplicationError::Internal(error.into()));
    }

    let upstream = build_upstream_request(state, &prepared).await?;
    let is_mutation = !matches!(*upstream.method(), Method::GET | Method::HEAD);
    let response = state
        .reqwest_client
        .execute(upstream)
        .await
        .map_err(|error| ApplicationError::UpstreamTransport(error.without_url()))?;
    let status = response.status();
    if status.is_success() {
        observe_success(state, &prepared, is_mutation).await;
    } else {
        warn!("Upstream server returned error status: {}", status);
    }

    let mut headers = response.headers().clone();
    let body = response
        .bytes()
        .await
        .map_err(|error| ApplicationError::UpstreamTransport(error.without_url()))?;
    let body = translate_response(
        state,
        &prepared,
        status,
        &mut headers,
        body,
        response_profile,
    )
    .await?;
    let response = client_response(status, &headers, body)?;

    if status.is_success() {
        if let Some(update) = prepared.report.and_then(|report| report.pending_update) {
            update.commit(&state.playback_registry).await;
        }
    }
    Ok(response)
}

/// Backend URL and credentials, plus virtual→backend IDs in the JSON body.
async fn build_upstream_request(
    state: &MediaServices,
    prepared: &PreparedForward,
) -> Result<reqwest::Request, ApplicationError> {
    let target = &prepared.target;
    let mut request = state
        .request_builder()
        .build(target, &prepared.normalized_request)
        .await?;
    let processing = RequestProcessingContext {
        server: target.server().clone(),
        session: target.session().cloned(),
    };
    let url = request.url().clone();
    state
        .translation
        .process_request_body(&mut request, &processing, &url)
        .await?;
    Ok(request)
}

/// Backend→virtual IDs in a JSON response. Non-JSON bodies pass through.
async fn translate_response(
    state: &MediaServices,
    prepared: &PreparedForward,
    status: StatusCode,
    headers: &mut HeaderMap,
    body: Bytes,
    profile: ResponseProcessingProfile,
) -> Result<Bytes, ApplicationError> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("application/json"));
    if status.is_success() && profile.requires_json() && (!is_json || body.is_empty()) {
        return Err(ApplicationError::InvalidUpstreamPayload(
            "required JSON response is empty or has wrong content type",
        ));
    }
    if !is_json || body.is_empty() {
        return Ok(body);
    }
    let mut value = match serde_json::from_slice::<Value>(&body) {
        Ok(value) => value,
        Err(error) if status.is_success() && profile.requires_json() => {
            return Err(ApplicationError::InvalidUpstreamJson(error));
        }
        Err(error) => {
            debug!("Skipping response JSON processing because parsing failed: {error}");
            return Ok(body);
        }
    };
    let token = prepared
        .input
        .context
        .authorization()
        .and_then(|auth| auth.token_ref());
    let modified = state
        .process_response_json(
            &mut value,
            prepared.target.server(),
            if status.is_success() {
                profile
            } else {
                ResponseProcessingProfile::BestEffortMedia
            },
            token,
        )
        .await?;
    if status.is_success() && profile != ResponseProcessingProfile::Disabled {
        if let Some(token) = token {
            state
                .client_sessions
                .cache_media_response(token, &value)
                .await;
        }
    }
    if !modified {
        return Ok(body);
    }
    let processed = serde_json::to_vec(&value).map_err(ApplicationError::Serialization)?;
    headers.remove(header::TRANSFER_ENCODING);
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&processed.len().to_string())
            .map_err(|error| ApplicationError::Internal(error.into()))?,
    );
    Ok(processed.into())
}

fn client_response(
    status: StatusCode,
    headers: &HeaderMap,
    body: Bytes,
) -> Result<Response<Bytes>, ApplicationError> {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        if !is_hop_by_hop_header(name) {
            builder = builder.header(name, value);
        }
    }
    builder
        .body(body)
        .map_err(|error| ApplicationError::Internal(error.into()))
}

/// Proxy-side effects of a request the backend accepted. Runs before the
/// response body is read; playback-binding commits wait for response validation.
async fn observe_success(state: &MediaServices, prepared: &PreparedForward, is_mutation: bool) {
    let context = &prepared.input.context;
    if is_mutation {
        if let Some(user) = context.user() {
            state.catalog_snapshots.invalidate_viewer(&user.id);
        }
    }
    let (Some(report), Some(user), Some(token)) = (
        prepared.report.as_ref(),
        context.user(),
        context.authorization().and_then(|auth| auth.token_ref()),
    ) else {
        return;
    };
    let client_session = state
        .client_sessions
        .ensure(user, token, context.device())
        .await;
    let stopped = report.action == PlaybackSessionAction::Remove;
    state
        .client_sessions
        .report(&client_session, stopped, &report.body)
        .await;
    if stopped {
        return;
    }
    let Some(item) = report.body.as_object().and_then(|object| {
        object
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("ItemId"))
            .and_then(|(_, value)| value.as_str())
    }) else {
        return;
    };
    match EpisodeContinuity::prepare(
        &state.media_storage,
        &state.client_sessions,
        context,
        &item.into(),
    )
    .await
    {
        Ok(Some(continuity)) => {
            continuity
                .remember(&state.client_sessions, prepared.target.server().id)
                .await
        }
        Ok(None) => {}
        Err(_) => warn!("Could not retain episode server preference after playback report"),
    }
}

/// A watched-state action on a merged item applies to every authorized copy.
/// Preflight availability before writing; never acknowledge a partial update
/// as success. Explicit member IDs continue through the ordinary proxy.
async fn update_aggregate(
    state: &MediaServices,
    prepared: &PreparedForward,
) -> Result<Option<Value>, ApplicationError> {
    let request = &prepared.input;
    if !matches!(
        *request.original_request.method(),
        reqwest::Method::POST | reqwest::Method::DELETE
    ) {
        return Ok(None);
    }
    let segments = decoded_path_segments(&prepared.path);
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
        .get_media_version_group(&id.as_str().into())
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
            right.mapping.original_media_id.as_str(),
            &left.server,
            left.mapping.original_media_id.as_str(),
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
        *upstream.url_mut() = replace_path_id(
            upstream.url(),
            tag,
            member.mapping.virtual_media_id.as_str(),
        )
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
                    Value::String(group.virtual_media_id.as_str().to_owned()),
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
        protocol::device::Device,
        servers::storage::ServerStorageService,
        test_support::MediaFixture,
        translation::request_analyzer::RequestBodyAnalysisResult,
    };
    use hyper::StatusCode;

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
            .get_or_create_media_mapping(&"upstream-item".into(), &server)
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
                "ItemId": item.virtual_media_id.as_str(), "PlaySessionId": "report-session",
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
                found_ids: vec![item.virtual_media_id.as_str().to_owned()],
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
        assert_eq!(
            binding.original_item_id.as_ref().map(|id| id.as_str()),
            Some("upstream-item")
        );
    }
}
